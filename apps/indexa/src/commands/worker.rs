use anyhow::Result;
use indexa_core::{
    config::{parse_reindex_interval, Config},
    store::Store,
};
use indexa_embed::OllamaEmbedder;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::cmd_index;
use super::helpers::{now_unix, require_index_db, select_summary_models};

/// Default re-index interval when `--auto-reindex` is passed but `[scan] auto_reindex`
/// is `off`/unset — a week is a sane "keep it fresh" cadence without being aggressive.
const DEFAULT_REINDEX_SECS: u64 = 7 * 86_400;

/// `[scan] auto_reindex` value that activates 3.3's continuous git-poll mode.
const GIT_POLL_MODE: &str = "git-poll";

/// Indexed roots whose newest deep-indexed content is older than `interval_secs`.
/// Roots that have never been deep-indexed (no chunks) are skipped — auto-reindex
/// refreshes existing context, it doesn't deep-index something the user never did.
fn stale_roots(store: &Store, interval_secs: u64, now: i64) -> Result<Vec<String>> {
    let cutoff = now - interval_secs as i64;
    let mut stale = Vec::new();
    for root in store.root_paths()? {
        if let Some(ts) = store.last_indexed_at_for_root(&root)? {
            if ts < cutoff {
                stale.push(root);
            }
        }
    }
    Ok(stale)
}

/// Re-index every stale root (incremental scan→deep→summarize) before the worker
/// starts draining. Runs to completion synchronously; per-root failures only warn.
async fn run_auto_reindex(db_path: &std::path::Path, cfg: &Config) -> Result<()> {
    let interval = parse_reindex_interval(&cfg.scan.auto_reindex).unwrap_or_else(|| {
        println!(
            "auto-reindex: [scan] auto_reindex is \"{}\"; using the default 7d interval.",
            cfg.scan.auto_reindex
        );
        DEFAULT_REINDEX_SECS
    });
    let stale = {
        let store = Store::open(db_path)?;
        stale_roots(&store, interval, now_unix())?
    };
    if stale.is_empty() {
        println!("auto-reindex: all indexed roots are current (interval {interval}s). Nothing to refresh.");
        return Ok(());
    }
    println!(
        "auto-reindex: {} root(s) older than {interval}s — refreshing:",
        stale.len()
    );
    for root in &stale {
        println!("  ↻ {root}");
    }
    for root in stale {
        // Reuse the one-shot pipeline; it's incremental (deep skips unchanged files,
        // summarize refreshes stale summaries). A failure on one root must not abort
        // the others or the worker.
        if let Err(e) = cmd_index(
            vec![root.clone()],
            None,
            "augment".to_owned(),
            None,
            false,
            false, // contextual_prefix off here; config [describer] contextual_prefix still applies
            true,  // yes: worker already resolved the root; skip the huge-root guard
            cfg,
        )
        .await
        {
            eprintln!("auto-reindex: failed to refresh {root}: {e:#}");
        }
    }
    Ok(())
}

/// 5s + 1s per 500 indexed entries, capped at 60s (3.3) — the bigger the index, the more
/// expensive a false-positive reindex trigger, so poll a bit less eagerly at scale.
fn git_poll_interval_secs(entry_count: u64) -> u64 {
    (5 + entry_count / 500).min(60)
}

/// `git rev-parse HEAD` + `git status --porcelain --untracked-files=no`, concatenated — a
/// state fingerprint that changes whenever HEAD moves (commit/checkout/merge) or the tracked
/// working tree gets dirty. Shells out (no git2 dependency), mirroring
/// `indexa_core::gitdiff`/`cochange`. `None` when `root` isn't inside a git work tree
/// (missing `git`, not a repo, or the command fails) — the caller falls back to interval-based
/// staleness for that root. Uses `git`'s own resolution rather than assuming a plain
/// `<root>/.git/HEAD` layout, since `root` may be a worktree, a submodule, or a subdirectory
/// of the repo — the same class of path-relativity trap as `cochange.rs`'s
/// `--name-only` (see [[feedback-git-log-path-scoping]] in project memory).
fn git_state_hash(root: &Path) -> Option<String> {
    let root_str = root.to_str()?;
    let run = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root_str)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    // rev-parse HEAD both gives the commit fingerprint and confirms `root` is actually
    // inside a git work tree with at least one commit — fails cleanly otherwise.
    let head = run(&["rev-parse", "HEAD"])?;
    let dirty = run(&["status", "--porcelain", "--untracked-files=no"]).unwrap_or_default();
    Some(format!("{head}:{dirty}"))
}

/// 3.3 — continuous git-poll auto-freshness: for each indexed root, watch its git state
/// (HEAD + tracked-tree dirtiness) at an adaptive interval and re-index on change; a
/// non-git root falls back to the same interval-based staleness check as the default mode.
/// Runs forever (the caller spawns this as a background task alongside the queue-draining
/// workers) — a per-root failure only warns, and the change is retried on the NEXT poll
/// since the baseline only advances on a successful reindex (never silently lost).
async fn run_git_poll(db_path: PathBuf, cfg: Config) {
    let mut baselines: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    loop {
        let snapshot = Store::open(&db_path).and_then(|store| {
            let roots = store.root_paths()?;
            let entry_count = store.entry_count()?;
            Ok((roots, entry_count))
        });
        let (roots, entry_count) = match snapshot {
            Ok(v) => v,
            Err(e) => {
                eprintln!("git-poll: could not read the index ({e:#}); retrying in 30s.");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
        };

        for root in &roots {
            match git_state_hash(Path::new(root)) {
                Some(hash) => {
                    let changed = baselines.get(root).is_some_and(|prev| *prev != hash);
                    if changed {
                        println!(
                            "git-poll: {root} changed (HEAD moved or tree dirty) — refreshing."
                        );
                        match cmd_index(
                            vec![root.clone()],
                            None,
                            "augment".to_owned(),
                            None,
                            false,
                            false,
                            true,
                            &cfg,
                        )
                        .await
                        {
                            Ok(()) => {
                                baselines.insert(root.clone(), hash);
                            }
                            Err(e) => {
                                eprintln!("git-poll: failed to refresh {root}: {e:#}");
                                // Baseline NOT advanced — the same change is detected and
                                // retried on the next poll.
                            }
                        }
                    } else {
                        baselines.entry(root.clone()).or_insert(hash);
                    }
                }
                None => {
                    // Non-git root: fall back to the existing interval-based staleness check.
                    if let Ok(store) = Store::open(&db_path) {
                        let is_stale = stale_roots(&store, DEFAULT_REINDEX_SECS, now_unix())
                            .map(|stale| stale.iter().any(|r| r == root))
                            .unwrap_or(false);
                        if is_stale {
                            if let Err(e) = cmd_index(
                                vec![root.clone()],
                                None,
                                "augment".to_owned(),
                                None,
                                false,
                                false,
                                true,
                                &cfg,
                            )
                            .await
                            {
                                eprintln!(
                                    "git-poll (interval fallback): failed to refresh {root}: {e:#}"
                                );
                            }
                        }
                    }
                }
            }
        }

        let interval = git_poll_interval_secs(entry_count);
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }
}

pub(crate) async fn cmd_worker(concurrency: usize, auto_reindex: bool, cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    // Auto-reindex: refresh stale roots before draining (opt-in via the flag so an
    // expensive rebuild never starts implicitly). `"git-poll"` instead spawns a persistent
    // background task (3.3) that continuously watches git state, superseding the one-shot
    // interval check for the lifetime of this worker.
    if auto_reindex {
        if cfg.scan.auto_reindex == GIT_POLL_MODE {
            println!(
                "auto-reindex: git-poll mode — continuously watching indexed git roots for changes."
            );
            tokio::spawn(run_git_poll(db_path.clone(), cfg.clone()));
        } else if let Err(e) = run_auto_reindex(&db_path, cfg).await {
            eprintln!("auto-reindex: skipped ({e:#})");
        }
    }

    // Pre-flight: for local Ollama, downgrade the dir roll-up model to one that fits
    // the budget (non-interactive CLI "ask me first"). For claude-code the models run
    // on the user's subscription (no local RAM to fit), so use them as configured.
    let (file_model, dir_model) = if cfg.describer.provider == "claude-code" {
        (
            cfg.describer.file_model.clone(),
            cfg.describer.dir_model.clone(),
        )
    } else {
        select_summary_models(cfg)
    };
    // Route through the factory so `provider = "claude-code"` is honored, not just Ollama.
    let describer: Arc<dyn indexa_llm::Describer + Send + Sync> =
        Arc::from(indexa_llm::describer_from_config(
            &cfg.describer.provider,
            &file_model,
            &dir_model,
            &cfg.describer.base_url,
            cfg.describer.num_ctx,
            &cfg.describer.claude_bin,
        )?);
    let embed_base = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync> = Arc::new(OllamaEmbedder::new(
        &embed_base,
        &cfg.embedding.model,
        cfg.embedding.dim,
    ));

    let store = Arc::new(tokio::sync::Mutex::new(Store::open(&db_path)?));

    // Startup sweep before any worker claims: reset items left `in_flight` by a prior
    // crash/kill back to `pending` (failing those past the attempt cap), so they aren't
    // stranded. Must run before the worker tasks spawn.
    match store.lock().await.requeue_stale_in_flight(3) {
        Ok((requeued, failed)) if requeued > 0 || failed > 0 => println!(
            "Requeued {requeued} stale in-flight item(s) from a previous run ({failed} failed over the attempt cap)."
        ),
        Ok(_) => {}
        Err(e) => eprintln!("Warning: could not sweep stale in-flight items: {e}"),
    }

    let stats = store.lock().await.queue_stats()?;
    println!(
        "Summary worker starting ({concurrency} concurrent). Queue: {} pending, {} done, {} failed.",
        stats.pending, stats.done, stats.failed
    );
    println!("Press Ctrl-C to stop.");

    let mut summary_cfg = cfg.describer.clone();
    // Keep the cfg models truthful under auto-downgrade: summary rows record
    // cfg.file_model/dir_model as their `model`, and provenance marks the substitution.
    summary_cfg.model_fallback =
        file_model != cfg.describer.file_model || dir_model != cfg.describer.dir_model;
    summary_cfg.file_model = file_model.clone();
    summary_cfg.dir_model = dir_model.clone();
    let headroom = cfg.resource.effective_headroom_bytes();
    let mut handles = Vec::new();
    for _ in 0..concurrency {
        let s = Arc::clone(&store);
        let d = Arc::clone(&describer);
        let e = Arc::clone(&embedder);
        let c = summary_cfg.clone();
        handles.push(tokio::spawn(indexa_query::run_worker(s, d, e, c, headroom)));
    }

    // Wait for all (runs forever until Ctrl-C)
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_poll_interval_scales_with_entry_count_and_caps_at_60() {
        assert_eq!(git_poll_interval_secs(0), 5);
        assert_eq!(git_poll_interval_secs(500), 6);
        assert_eq!(git_poll_interval_secs(2_500), 10);
        assert_eq!(git_poll_interval_secs(1_000_000), 60);
    }

    fn run(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn git_state_hash_is_none_for_a_non_repo_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(git_state_hash(tmp.path()).is_none());
    }

    #[test]
    fn git_state_hash_changes_on_new_commit_and_on_dirty_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.email", "t@example.com"]);
        run(root, &["config", "user.name", "t"]);
        run(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("a.txt"), "1").unwrap();
        run(root, &["add", "a.txt"]);
        run(root, &["commit", "-q", "-m", "init"]);

        let after_init = git_state_hash(root).unwrap();

        // Dirtying a tracked file changes the hash (status output differs) without a commit.
        std::fs::write(root.join("a.txt"), "2").unwrap();
        let dirty = git_state_hash(root).unwrap();
        assert_ne!(
            after_init, dirty,
            "a dirty tracked file must change the state hash"
        );

        // Committing changes HEAD, also changing the hash.
        run(root, &["add", "a.txt"]);
        run(root, &["commit", "-q", "-m", "second"]);
        let after_second = git_state_hash(root).unwrap();
        assert_ne!(
            dirty, after_second,
            "a new commit must change the state hash"
        );

        // A clean tree at a stable commit is deterministic across repeated calls.
        assert_eq!(git_state_hash(root).unwrap(), after_second);
    }

    #[test]
    fn git_state_hash_ignores_untracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.email", "t@example.com"]);
        run(root, &["config", "user.name", "t"]);
        run(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("a.txt"), "1").unwrap();
        run(root, &["add", "a.txt"]);
        run(root, &["commit", "-q", "-m", "init"]);

        let before = git_state_hash(root).unwrap();
        std::fs::write(root.join("untracked.txt"), "noise").unwrap();
        let after = git_state_hash(root).unwrap();
        assert_eq!(
            before, after,
            "an untracked file must not trigger a poll cycle (--untracked-files=no)"
        );
    }
}
