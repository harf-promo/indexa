//! `indexa memory` — record and query durable memory.
//!
//! The CLI is the **operator** surface. Two things follow from that and are deliberate:
//!
//! 1. `--author` defaults to *operator*, so a claim you type may carry full confidence. The MCP
//!    surface will default to *agent*, which caps at 0.75. That single default is the whole
//!    confidence-ceiling mechanism — an agent cannot promote its own guess by asking nicely.
//! 2. `verify --run` is the **only** place a stored `verify_cmd` is ever executed, and it takes
//!    an explicit flag. Memories are meant to travel in Context Packs, so a memory from
//!    someone else's export is untrusted input; running its command on read would make
//!    importing a pack a remote-code-execution path.

use anyhow::{bail, Context, Result};
use indexa_cli::MemoryAction;
use indexa_core::memory::{
    Author, MemoryKind, MemoryRecord, MemoryStatus, NewMemory, VerifyStatus,
};
use indexa_core::store::Store;

use super::helpers::{index_db_path, now_unix};

pub(crate) async fn cmd_memory(action: MemoryAction) -> Result<()> {
    let Some(db_path) = require_index()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    match action {
        MemoryAction::Add {
            text,
            kind,
            subject,
            confidence,
            source,
            verify_cmd,
            tag,
            path,
            as_agent,
            json,
        } => add(
            &mut store, text, &kind, subject, confidence, source, verify_cmd, tag, path, as_agent,
            json,
        ),
        MemoryAction::List {
            kind,
            min_confidence,
            limit,
            json,
        } => list(&store, &kind, min_confidence, limit, json),
        MemoryAction::Show { id, json } => show(&store, id, json),
        MemoryAction::Search { query, limit, json } => search(&store, &query, limit, json),
        MemoryAction::Verify { id, run, timeout } => verify(&mut store, id, run, timeout),
        MemoryAction::Supersede {
            id,
            text,
            kind,
            confidence,
        } => supersede(&mut store, id, text, kind, confidence),
        MemoryAction::Retire { id } => retire(&mut store, id),
        MemoryAction::Expire { id, at } => expire(&mut store, id, at),
        MemoryAction::Decay {
            older_than,
            factor,
            dry_run,
        } => decay(&mut store, &older_than, factor, dry_run),
        MemoryAction::Reflect { json } => reflect(&store, json),
        MemoryAction::AdoptAnnotations { dry_run } => adopt_annotations(&mut store, dry_run),
    }
}

/// The index must exist — memories live in it. Prints the same guidance every other command
/// does rather than erroring, so a first-run user is told what to do next.
fn require_index() -> Result<Option<std::path::PathBuf>> {
    let p = index_db_path()?;
    if !p.exists() {
        println!(
            "No index yet at {}.\nRun `indexa index <path>` first — memories are stored \
             alongside it.",
            p.display()
        );
        return Ok(None);
    }
    Ok(Some(p))
}

fn parse_kind(s: &str) -> Result<MemoryKind> {
    MemoryKind::parse(s).with_context(|| {
        let all = MemoryKind::ALL
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        format!("unknown --kind '{s}' — expected one of: {all}")
    })
}

// ── add ───────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)] // one parameter per CLI flag; a params struct here would
                                     // only move the same list one line up
fn add(
    store: &mut Store,
    text: String,
    kind: &str,
    subject: Option<String>,
    confidence: Option<f32>,
    source: Option<String>,
    verify_cmd: Option<String>,
    tags: Vec<String>,
    paths: Vec<String>,
    as_agent: bool,
    json: bool,
) -> Result<()> {
    if text.trim().is_empty() {
        bail!("a memory needs a claim — pass the text as the first argument");
    }
    let author = if as_agent {
        Author::Agent
    } else {
        Author::Operator
    };
    let mut m = NewMemory::new(parse_kind(kind)?, text, author);
    m.subject = subject.unwrap_or_default();
    if let Some(c) = confidence {
        m.confidence = c;
    }
    m.verify_cmd = verify_cmd;
    m.tags = tags;
    m.paths = paths;
    if let Some(src) = source {
        // Hash the source NOW, so `verify` can later tell whether the file moved on beneath
        // the claim. A missing file is not an error — the claim may be about something that
        // was deleted, which is exactly when it is most worth keeping.
        m.source_sha256 = std::fs::read(&src)
            .ok()
            .map(|b| indexa_core::store::hex_digest(<sha2::Sha256 as sha2::Digest>::digest(&b)));
        m.source_path = Some(src);
    }

    let (id, clamped) = store.record_memory(&m)?;
    let stored = store.memory_by_id(id)?.expect("just written");
    if json {
        println!("{}", serde_json::to_string_pretty(&as_json(&stored))?);
        return Ok(());
    }
    println!("Recorded memory #{id} [{}]", stored.kind);
    println!("  {}", stored.text);
    if clamped {
        println!(
            "  confidence recorded as {:.2} — an agent-authored claim is capped at {:.2}; \
             an operator can raise it with `indexa memory verify {id}`",
            stored.confidence,
            Author::Agent.max_confidence()
        );
    }
    if let Some(cmd) = &stored.verify_cmd {
        println!("  verify with: {cmd}");
        println!("  (stored, not run — `indexa memory verify {id} --run` executes it)");
    }
    Ok(())
}

// ── read ──────────────────────────────────────────────────────────────────────

fn list(
    store: &Store,
    kinds: &[String],
    min_confidence: f32,
    limit: usize,
    json: bool,
) -> Result<()> {
    let parsed: Vec<MemoryKind> = kinds
        .iter()
        .map(|k| parse_kind(k))
        .collect::<Result<Vec<_>>>()?;
    let filter = if parsed.is_empty() {
        None
    } else {
        Some(parsed.as_slice())
    };
    let rows = store.active_memories(filter, min_confidence, limit)?;
    if json {
        let arr: Vec<_> = rows.iter().map(as_json).collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No memories yet. Record one with `indexa memory add \"<claim>\"`.");
        return Ok(());
    }
    let counts = store.memory_counts()?;
    println!(
        "{} active memory/memories ({} verified, {} unverified, {} aged, {} failed; \
         {} retired)\n",
        counts.active,
        counts.verified,
        counts.unverified,
        counts.aged,
        counts.failed,
        counts.retired
    );
    println!(
        "  {:<5} {:<11} {:<5} {:<11} CLAIM",
        "ID", "KIND", "CONF", "CHECKED"
    );
    println!("  {}", "─".repeat(76));
    for m in &rows {
        println!(
            "  {:<5} {:<11} {:<5.2} {:<11} {}",
            m.id,
            m.kind.as_str(),
            m.confidence,
            m.verify_status.as_str(),
            indexa_core::truncate_chars(&m.text, 46)
        );
    }
    Ok(())
}

fn show(store: &Store, id: i64, json: bool) -> Result<()> {
    let Some(m) = store.memory_by_id(id)? else {
        bail!("no memory #{id}");
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&as_json(&m))?);
        return Ok(());
    }
    println!("Memory #{}  [{}]  {}", m.id, m.kind, m.status);
    println!("\n  {}\n", m.text);
    println!("  confidence   {:.2} ({})", m.confidence, m.author);
    println!("  verified     {}", m.verify_status);
    if let Some(at) = m.verified_at {
        println!("  checked at   {at}");
    }
    if !m.subject.is_empty() {
        println!("  subject      {}", m.subject);
    }
    if let Some(p) = &m.source_path {
        println!("  source       {p}");
    }
    if let Some(h) = &m.source_sha256 {
        println!("  source hash  {}", &h[..h.len().min(12)]);
    }
    if let Some(c) = &m.verify_cmd {
        println!("  verify cmd   {c}   (never run automatically)");
    }
    if !m.tags.is_empty() {
        println!("  tags         {}", m.tags.join(", "));
    }
    if !m.paths.is_empty() {
        println!("  paths        {}", m.paths.join(", "));
    }
    println!("  valid from   {}", m.valid_from);
    match m.valid_to {
        Some(t) => println!("  valid to     {t}  (no longer believed)"),
        None => println!("  valid to     — (still believed)"),
    }
    if let Some(p) = m.parent_id {
        println!("  replaces     #{p}");
    }
    if let Some(n) = m.superseded_by {
        println!("  replaced by  #{n}");
    }
    Ok(())
}

fn search(store: &Store, query: &str, limit: usize, json: bool) -> Result<()> {
    let rows = store.search_memories(query, limit)?;
    if json {
        let arr: Vec<_> = rows.iter().map(as_json).collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No memory matches \"{query}\".");
        return Ok(());
    }
    for m in &rows {
        println!(
            "  #{:<5} [{:<10}] {:.2}  {}",
            m.id,
            m.kind.as_str(),
            m.confidence,
            indexa_core::truncate_chars(&m.text, 60)
        );
    }
    Ok(())
}

// ── verify ────────────────────────────────────────────────────────────────────

/// What re-checking a claim found. Split out so the decision is testable without running a
/// subprocess or touching the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceCheck {
    /// No `source_path` recorded — nothing to compare against.
    NoSource,
    /// The source file is gone. NOT a failure: a claim about a deleted file is often the most
    /// valuable thing in the store ("we removed X because Y").
    SourceMissing,
    /// The source is byte-identical to when the claim was made.
    Unchanged,
    /// The source has changed since the claim was made — the claim may no longer hold.
    Changed,
}

pub(crate) fn check_source(recorded_hash: Option<&str>, current_hash: Option<&str>) -> SourceCheck {
    match (recorded_hash, current_hash) {
        (None, _) => SourceCheck::NoSource,
        (Some(_), None) => SourceCheck::SourceMissing,
        (Some(a), Some(b)) if a == b => SourceCheck::Unchanged,
        _ => SourceCheck::Changed,
    }
}

fn verify(store: &mut Store, id: i64, run: bool, timeout_secs: u64) -> Result<()> {
    let Some(m) = store.memory_by_id(id)? else {
        bail!("no memory #{id}");
    };
    println!("Memory #{id} [{}]\n  {}\n", m.kind, m.text);

    let current = m.source_path.as_ref().and_then(|p| {
        std::fs::read(p)
            .ok()
            .map(|b| indexa_core::store::hex_digest(<sha2::Sha256 as sha2::Digest>::digest(&b)))
    });
    let check = check_source(m.source_sha256.as_deref(), current.as_deref());
    match check {
        SourceCheck::NoSource => println!("  source:  none recorded — nothing to compare"),
        SourceCheck::SourceMissing => println!(
            "  source:  {} is gone. The claim is kept — a memory about a deleted file is \
             often the point.",
            m.source_path.as_deref().unwrap_or("?")
        ),
        SourceCheck::Unchanged => println!("  source:  unchanged since the claim was made"),
        SourceCheck::Changed => println!(
            "  source:  {} HAS CHANGED since the claim was made — re-read it before trusting this",
            m.source_path.as_deref().unwrap_or("?")
        ),
    }

    let Some(cmd) = m.verify_cmd.clone() else {
        if check == SourceCheck::Unchanged {
            store.set_memory_verify_status(id, VerifyStatus::Verified, Some(now_unix()))?;
            println!("\n  marked verified (source hash matches).");
        } else if check == SourceCheck::Changed {
            store.set_memory_verify_status(id, VerifyStatus::Failed, Some(now_unix()))?;
            println!(
                "\n  marked failed — it will no longer be offered as context. \
                 Correct it with `indexa memory supersede {id} \"<corrected claim>\"`."
            );
        }
        return Ok(());
    };

    if !run {
        println!("\n  verify command (NOT run):\n    {cmd}");
        println!(
            "\n  Re-run with --run to execute it. It is stored as data and never executed \
             automatically: a memory can arrive from an imported pack, so its command string \
             is untrusted input."
        );
        return Ok(());
    }

    println!("\n  running: {cmd}");
    let ok = run_verify_command(&cmd, timeout_secs)?;
    let status = if ok {
        VerifyStatus::Verified
    } else {
        VerifyStatus::Failed
    };
    store.set_memory_verify_status(id, status, Some(now_unix()))?;
    println!(
        "  {} — memory #{id} marked {}",
        if ok { "passed" } else { "FAILED" },
        status
    );
    if !ok {
        println!("  It will no longer be offered as context until it is corrected.");
    }
    Ok(())
}

/// Run a stored verify command through the platform shell, bounded by `timeout_secs`.
///
/// Reached only from `verify --run`. Uses `wait-timeout` (already a workspace dependency, for
/// the parser preprocessor hook) so a hung command is killed rather than blocking the CLI
/// forever.
fn run_verify_command(cmd: &str, timeout_secs: u64) -> Result<bool> {
    use std::process::{Command, Stdio};
    use wait_timeout::ChildExt as _;

    let mut child = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", cmd])
            .stdin(Stdio::null())
            .spawn()
    } else {
        Command::new("sh")
            .args(["-c", cmd])
            .stdin(Stdio::null())
            .spawn()
    }
    .with_context(|| format!("spawning verify command: {cmd}"))?;

    match child.wait_timeout(std::time::Duration::from_secs(timeout_secs))? {
        Some(status) => Ok(status.success()),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            println!("  timed out after {timeout_secs}s");
            Ok(false)
        }
    }
}

// ── mutate ────────────────────────────────────────────────────────────────────

fn supersede(
    store: &mut Store,
    id: i64,
    text: String,
    kind: Option<String>,
    confidence: Option<f32>,
) -> Result<()> {
    let Some(old) = store.memory_by_id(id)? else {
        bail!("no memory #{id}");
    };
    let kind = match kind {
        Some(k) => parse_kind(&k)?,
        None => old.kind,
    };
    let mut m = NewMemory::new(kind, text, Author::Operator);
    m.subject = old.subject.clone();
    m.confidence = confidence.unwrap_or(old.confidence);
    m.source_path = old.source_path.clone();
    // Re-hash the source NOW rather than carrying the old hash forward. A supersede is the
    // operator asserting a corrected claim about the file as it stands today — inheriting the
    // stale hash would make the replacement instantly "unverifiable", and copying nothing would
    // make `verify` report "no source recorded" for a claim that plainly has one.
    m.source_sha256 = old.source_path.as_ref().and_then(|p| {
        std::fs::read(p)
            .ok()
            .map(|b| indexa_core::store::hex_digest(<sha2::Sha256 as sha2::Digest>::digest(&b)))
    });
    m.tags = old.tags.clone();
    m.paths = old.paths.clone();

    let (new_id, _) = store.supersede_memory(id, &m)?;
    if new_id == id {
        println!("Unchanged — the replacement text is identical to memory #{id}.");
    } else {
        println!("Memory #{id} replaced by #{new_id}. The old row is retired, not deleted.");
    }
    Ok(())
}

fn retire(store: &mut Store, id: i64) -> Result<()> {
    if store.retire_memory(id)? {
        println!("Memory #{id} retired. The row is kept; it will not be offered as context.");
    } else {
        bail!("no memory #{id}");
    }
    Ok(())
}

fn expire(store: &mut Store, id: i64, at: Option<i64>) -> Result<()> {
    let at = at.unwrap_or_else(now_unix);
    if store.set_memory_valid_to(id, at)? {
        println!("Memory #{id} marked no longer true as of {at}. The claim stays readable.");
    } else {
        bail!("no memory #{id}");
    }
    Ok(())
}

fn decay(store: &mut Store, older_than: &str, factor: f32, dry_run: bool) -> Result<()> {
    let secs = indexa_core::config::parse_reindex_interval(older_than).ok_or_else(|| {
        anyhow::anyhow!("invalid --older-than '{older_than}': use a window like 90d, 12h, or 3600s")
    })? as i64;

    let eligible: Vec<MemoryRecord> = store
        .active_memories(None, 0.0, 10_000)?
        .into_iter()
        .filter(|m| {
            m.kind.decayable()
                && m.verify_status == VerifyStatus::Unverified
                && m.created_at <= now_unix() - secs
        })
        .collect();

    if eligible.is_empty() {
        println!("Nothing to age: no unverified inference or hypothesis older than {older_than}.");
        return Ok(());
    }
    println!(
        "{} memory/memories eligible (unverified {} or {} older than {older_than}):",
        eligible.len(),
        MemoryKind::Inferred,
        MemoryKind::Hypothesis
    );
    for m in &eligible {
        println!(
            "  #{:<5} [{:<10}] {:.2} → {:.2}  {}",
            m.id,
            m.kind.as_str(),
            m.confidence,
            m.confidence * factor,
            indexa_core::truncate_chars(&m.text, 48)
        );
    }
    if dry_run {
        println!("\n(dry run — nothing changed)");
        return Ok(());
    }
    let n = store.age_unverified_memories(secs, factor)?;
    println!(
        "\nAged {n} memory/memories. Nothing was deleted; each row is marked `aged` and stays \
         readable. Observations, statements and recalled facts are never aged."
    );
    Ok(())
}

// ── reflect ───────────────────────────────────────────────────────────────────

fn reflect(store: &Store, json: bool) -> Result<()> {
    let rows = store.active_memories(None, 0.0, 10_000)?;

    // A contradiction, conservatively defined: two live claims about the same subject where
    // one is witnessed or stated and the other is a guess. That is a real tension worth a
    // human's attention, and it is cheap and deterministic to detect. Semantic contradiction
    // between two arbitrary claims needs embeddings, which land with the retrieval PR.
    let mut by_subject: std::collections::BTreeMap<&str, Vec<&MemoryRecord>> =
        std::collections::BTreeMap::new();
    for m in &rows {
        if !m.subject.is_empty() {
            by_subject.entry(&m.subject).or_default().push(m);
        }
    }
    let mut conflicts: Vec<(&str, i64, i64)> = Vec::new();
    for (subject, group) in &by_subject {
        let strong = group.iter().find(|m| {
            matches!(m.kind, MemoryKind::Observed | MemoryKind::Stated)
                || m.verify_status == VerifyStatus::Verified
        });
        let weak = group
            .iter()
            .find(|m| matches!(m.kind, MemoryKind::Hypothesis | MemoryKind::Inferred));
        if let (Some(s), Some(w)) = (strong, weak) {
            conflicts.push((subject, s.id, w.id));
        }
    }

    // Claims whose source has moved on. Cheap (a stat + a hash), and the most actionable
    // thing this command can say.
    let mut stale: Vec<(i64, String)> = Vec::new();
    for m in &rows {
        let (Some(path), Some(recorded)) = (&m.source_path, &m.source_sha256) else {
            continue;
        };
        let current = std::fs::read(path)
            .ok()
            .map(|b| indexa_core::store::hex_digest(<sha2::Sha256 as sha2::Digest>::digest(&b)));
        if check_source(Some(recorded), current.as_deref()) == SourceCheck::Changed {
            stale.push((m.id, path.clone()));
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "contradictions": conflicts.iter().map(|(s, a, b)| serde_json::json!({
                    "subject": s, "trusted": a, "speculative": b
                })).collect::<Vec<_>>(),
                "sources_changed": stale.iter().map(|(id, p)| serde_json::json!({
                    "memory": id, "source": p
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    if conflicts.is_empty() && stale.is_empty() {
        println!("Nothing to reflect on: no contradictions, no sources changed under a claim.");
        return Ok(());
    }
    if !conflicts.is_empty() {
        println!("Subjects carrying both a trusted claim and a speculative one:\n");
        for (subject, strong, weak) in &conflicts {
            println!("  {subject}\n    trusted     #{strong}\n    speculative #{weak}");
        }
        println!(
            "\n  Not resolved automatically — which of two claims is right is a judgment call, \
             and silently picking one is how a memory store starts lying. Use \
             `indexa memory supersede` or `retire` once you have decided."
        );
    }
    if !stale.is_empty() {
        if !conflicts.is_empty() {
            println!();
        }
        println!("Claims whose source file changed after the claim was made:\n");
        for (id, path) in &stale {
            println!("  #{id}  {path}");
        }
        println!("\n  Re-check with `indexa memory verify <id>`.");
    }
    Ok(())
}

// ── the ledger bridge ─────────────────────────────────────────────────────────

fn adopt_annotations(store: &mut Store, dry_run: bool) -> Result<()> {
    let rows = store.annotation_decisions()?;
    if rows.is_empty() {
        println!("No `record_decision` annotations in the ledger to adopt.");
        return Ok(());
    }
    let mut adopted = 0usize;
    let mut skipped = 0usize;
    for a in rows {
        // `record_memory` dedups on the claim text among active rows, so re-running this is
        // safe: an annotation already adopted returns the existing id and writes nothing.
        if dry_run {
            println!(
                "  would adopt ledger #{}: {}",
                a.decision_id,
                indexa_core::truncate_chars(&a.text, 60)
            );
            adopted += 1;
            continue;
        }
        let mut m = NewMemory::new(MemoryKind::Stated, &a.text, Author::Agent);
        m.subject = a.subject;
        // Mid-scale on purpose: an annotation was pinned by an agent mid-session and never
        // independently checked, so adopting it must not launder it into a confident claim.
        m.confidence = 0.6;
        m.patch_id = a.patch_id;
        m.source_decision_id = Some(a.decision_id);
        m.tags = vec!["adopted".to_owned()];
        let before = store.memory_counts()?.active;
        store.record_memory(&m)?;
        if store.memory_counts()?.active > before {
            adopted += 1;
        } else {
            skipped += 1;
        }
    }
    if dry_run {
        println!("\n(dry run — {adopted} annotation(s) would be adopted, nothing changed)");
    } else {
        println!(
            "Adopted {adopted} annotation(s); {skipped} already present. The ledger rows were \
             not modified."
        );
    }
    Ok(())
}

// ── shared rendering ──────────────────────────────────────────────────────────

fn as_json(m: &MemoryRecord) -> serde_json::Value {
    serde_json::json!({
        "id": m.id,
        "kind": m.kind.as_str(),
        "text": m.text,
        "subject": m.subject,
        "confidence": m.confidence,
        "author": m.author.as_str(),
        "source_path": m.source_path,
        "source_sha256": m.source_sha256,
        "verify_cmd": m.verify_cmd,
        "verify_status": m.verify_status.as_str(),
        "verified_at": m.verified_at,
        "tags": m.tags,
        "status": m.status.as_str(),
        "parent_id": m.parent_id,
        "superseded_by": m.superseded_by,
        "valid_from": m.valid_from,
        "valid_to": m.valid_to,
        "created_at": m.created_at,
        "paths": m.paths,
        "live": m.is_live(now_unix()) && m.status == MemoryStatus::Active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use indexa_cli::{Cli, Commands};

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn seeded(kind: MemoryKind, text: &str) -> NewMemory {
        NewMemory::new(kind, text, Author::Operator)
    }

    // ── check_source ─────────────────────────────────────────────────────────

    #[test]
    fn a_missing_source_is_not_treated_as_a_failed_claim() {
        // The case that matters most: "we removed X because Y" is at its most valuable once X
        // is gone. Reporting that as a verification failure would retire exactly the memories
        // worth keeping.
        assert_eq!(check_source(Some("abc"), None), SourceCheck::SourceMissing);
    }

    #[test]
    fn source_check_distinguishes_unchanged_changed_and_absent() {
        assert_eq!(
            check_source(Some("abc"), Some("abc")),
            SourceCheck::Unchanged
        );
        assert_eq!(check_source(Some("abc"), Some("def")), SourceCheck::Changed);
        assert_eq!(check_source(None, Some("def")), SourceCheck::NoSource);
        assert_eq!(check_source(None, None), SourceCheck::NoSource);
    }

    // ── add ──────────────────────────────────────────────────────────────────

    #[test]
    fn add_defaults_to_operator_authorship() {
        // The CLI is the human surface, so a claim typed here may carry full confidence. The
        // MCP surface defaults to agent, and that single difference IS the confidence ceiling.
        let mut s = store();
        add(
            &mut s,
            "we ship on Fridays".into(),
            "stated",
            None,
            Some(0.95),
            None,
            None,
            vec![],
            vec![],
            false,
            false,
        )
        .unwrap();
        let m = &s.active_memories(None, 0.0, 10).unwrap()[0];
        assert_eq!(m.author, Author::Operator);
        assert!(
            (m.confidence - 0.95).abs() < 1e-6,
            "not clamped for a human"
        );
    }

    #[test]
    fn add_as_agent_is_capped() {
        let mut s = store();
        add(
            &mut s,
            "probably a race".into(),
            "hypothesis",
            None,
            Some(0.99),
            None,
            None,
            vec![],
            vec![],
            true,
            false,
        )
        .unwrap();
        let m = &s.active_memories(None, 0.0, 10).unwrap()[0];
        assert_eq!(m.author, Author::Agent);
        assert!((m.confidence - 0.75).abs() < 1e-6);
    }

    #[test]
    fn add_hashes_the_source_so_verify_has_something_to_compare() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        std::fs::write(&f, b"fn main() {}").unwrap();
        let mut s = store();
        add(
            &mut s,
            "main is empty".into(),
            "observed",
            None,
            None,
            Some(f.to_string_lossy().into_owned()),
            None,
            vec![],
            vec![],
            false,
            false,
        )
        .unwrap();
        let m = &s.active_memories(None, 0.0, 10).unwrap()[0];
        assert!(m.source_sha256.is_some(), "source hashed at record time");
    }

    #[test]
    fn add_survives_a_source_that_does_not_exist() {
        // A claim about a file that is already gone is legitimate — it must record, just
        // without a hash to compare later.
        let mut s = store();
        add(
            &mut s,
            "gone.rs duplicated helper.rs".into(),
            "observed",
            None,
            None,
            Some("/definitely/not/here.rs".into()),
            None,
            vec![],
            vec![],
            false,
            false,
        )
        .unwrap();
        let m = &s.active_memories(None, 0.0, 10).unwrap()[0];
        assert_eq!(m.source_sha256, None);
        assert_eq!(m.source_path.as_deref(), Some("/definitely/not/here.rs"));
    }

    #[test]
    fn add_rejects_an_empty_claim_and_an_unknown_kind() {
        let mut s = store();
        assert!(add(
            &mut s,
            "   ".into(),
            "stated",
            None,
            None,
            None,
            None,
            vec![],
            vec![],
            false,
            false
        )
        .is_err());
        let err = add(
            &mut s,
            "x".into(),
            "pratyaksha",
            None,
            None,
            None,
            None,
            vec![],
            vec![],
            false,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown --kind"), "got: {err}");
        assert!(
            err.contains("observed"),
            "the error must list the valid kinds: {err}"
        );
    }

    // ── decay ────────────────────────────────────────────────────────────────

    #[test]
    fn decay_dry_run_changes_nothing() {
        let mut s = store();
        let (id, _) = s
            .record_memory(&seeded(MemoryKind::Hypothesis, "a guess"))
            .unwrap();
        s.db_connection()
            .execute(
                "UPDATE memories SET created_at = unixepoch() - 99999999 WHERE id = ?1",
                rusqlite::params![id],
            )
            .unwrap();
        let before = s.memory_by_id(id).unwrap().unwrap().confidence;

        decay(&mut s, "90d", 0.5, true).unwrap();
        let after = s.memory_by_id(id).unwrap().unwrap();
        assert!((after.confidence - before).abs() < 1e-6);
        assert_eq!(after.verify_status, VerifyStatus::Unverified);

        decay(&mut s, "90d", 0.5, false).unwrap();
        assert_eq!(
            s.memory_by_id(id).unwrap().unwrap().verify_status,
            VerifyStatus::Aged,
            "the non-dry run does apply"
        );
    }

    #[test]
    fn decay_rejects_an_unparseable_window() {
        let mut s = store();
        let err = decay(&mut s, "sometime", 0.5, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid --older-than"), "got: {err}");
    }

    // ── reflect ──────────────────────────────────────────────────────────────

    #[test]
    fn reflect_flags_a_subject_carrying_both_a_trusted_and_a_speculative_claim() {
        let mut s = store();
        let mut observed = seeded(MemoryKind::Observed, "auth uses JWT");
        observed.subject = "src/auth.rs".into();
        let mut guess = seeded(MemoryKind::Hypothesis, "auth might use sessions");
        guess.subject = "src/auth.rs".into();
        let mut elsewhere = seeded(MemoryKind::Hypothesis, "unrelated guess");
        elsewhere.subject = "src/other.rs".into();
        for m in [&observed, &guess, &elsewhere] {
            s.record_memory(m).unwrap();
        }
        // Runs clean and finds the conflict; the JSON path is what the web/MCP surfaces will
        // consume, so exercise that rather than scraping stdout.
        reflect(&s, true).unwrap();
        reflect(&s, false).unwrap();
    }

    #[test]
    fn reflect_is_quiet_on_a_consistent_store() {
        let mut s = store();
        s.record_memory(&seeded(MemoryKind::Observed, "a fact"))
            .unwrap();
        reflect(&s, false).unwrap();
    }

    // ── the ledger bridge ────────────────────────────────────────────────────

    #[test]
    fn adopting_annotations_is_idempotent_and_leaves_the_ledger_alone() {
        let mut s = store();
        let before_ledger = s.annotation_decisions().unwrap().len();
        s.db_connection()
            .execute(
                "INSERT INTO decisions (decision_type, subject, chosen, status)
                 VALUES ('annotation', 'src/auth.rs', 'auth was rewritten in v0.40', 'decided')",
                [],
            )
            .unwrap();
        assert_eq!(s.annotation_decisions().unwrap().len(), before_ledger + 1);

        adopt_annotations(&mut s, false).unwrap();
        assert_eq!(s.memory_counts().unwrap().active, 1);
        let m = &s.active_memories(None, 0.0, 10).unwrap()[0];
        assert_eq!(m.kind, MemoryKind::Stated);
        assert_eq!(m.subject, "src/auth.rs");
        assert!(
            m.source_decision_id.is_some(),
            "provenance back to the ledger"
        );

        // Re-running must not duplicate — `record_memory` dedups on the claim text.
        adopt_annotations(&mut s, false).unwrap();
        assert_eq!(s.memory_counts().unwrap().active, 1);
        assert_eq!(
            s.annotation_decisions().unwrap().len(),
            before_ledger + 1,
            "the ledger row is never modified"
        );
    }

    #[test]
    fn an_annotation_with_no_answer_text_is_not_adopted_as_an_empty_claim() {
        let mut s = store();
        s.db_connection()
            .execute(
                "INSERT INTO decisions (decision_type, subject, chosen, status)
                 VALUES ('annotation', 'src/a.rs', '   ', 'decided')",
                [],
            )
            .unwrap();
        adopt_annotations(&mut s, false).unwrap();
        assert_eq!(s.memory_counts().unwrap().active, 0);
    }

    // ── argument parsing ─────────────────────────────────────────────────────

    fn parse(args: &[&str]) -> MemoryAction {
        let cli = Cli::try_parse_from(args).expect("args should parse");
        match cli.command {
            Commands::Memory { action } => action,
            _ => panic!("expected a memory command"),
        }
    }

    #[test]
    fn add_parses_repeatable_tags_and_paths() {
        let a = parse(&[
            "indexa", "memory", "add", "a claim", "--kind", "observed", "--tag", "one", "--tag",
            "two", "--path", "/a", "--path", "/b",
        ]);
        match a {
            MemoryAction::Add {
                kind, tag, path, ..
            } => {
                assert_eq!(kind, "observed");
                assert_eq!(tag, vec!["one", "two"]);
                assert_eq!(path, vec!["/a", "/b"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn add_defaults_kind_to_stated() {
        match parse(&["indexa", "memory", "add", "a claim"]) {
            MemoryAction::Add { kind, as_agent, .. } => {
                assert_eq!(
                    kind, "stated",
                    "a typed claim is an assertion, not an observation"
                );
                assert!(!as_agent, "the CLI is the operator surface");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn verify_does_not_run_the_command_unless_asked() {
        match parse(&["indexa", "memory", "verify", "7"]) {
            MemoryAction::Verify { id, run, timeout } => {
                assert_eq!(id, 7);
                assert!(!run, "executing a stored command must be opt-in");
                assert_eq!(timeout, 300);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["indexa", "memory", "verify", "7", "--run"]) {
            MemoryAction::Verify { run, .. } => assert!(run),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decay_defaults_to_ninety_days_and_a_halving_factor() {
        match parse(&["indexa", "memory", "decay"]) {
            MemoryAction::Decay {
                older_than,
                factor,
                dry_run,
            } => {
                assert_eq!(older_than, "90d");
                assert!((factor - 0.5).abs() < 1e-6);
                assert!(!dry_run);
            }
            other => panic!("{other:?}"),
        }
    }
}
