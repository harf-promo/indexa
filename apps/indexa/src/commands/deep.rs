use anyhow::Result;
use indexa_core::{
    config::{Config, SummaryMode},
    resource::{detect_machine, estimate_eta, format_duration_pub},
    store::{ChunkRecord, EdgeRecord, Store},
    walker::{walk, WalkConfig},
};
use indexa_llm::OllamaLlm;
use indexa_query::{contextual::ContextualEvent, enqueue_subtree};
use std::io::{IsTerminal, Write};

use super::helpers::{build_embedder, parse_summary_mode, require_index_db, resolve_roots};

pub(crate) async fn cmd_deep(
    paths: Vec<String>,
    embed_model_flag: Option<String>,
    dry_run: bool,
    mode: String,
    contextual: bool,
    cfg: &Config,
) -> Result<()> {
    let summary_mode = parse_summary_mode(&mode)?;
    let roots = resolve_roots(paths, false)?;
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let max_parse_bytes = cfg.parsers.max_file_mb.saturating_mul(1024 * 1024);
    let walk_cfg = WalkConfig {
        respect_gitignore: cfg.scan.respect_gitignore,
        ignore: cfg.scan.ignore.clone(),
        ..Default::default()
    };

    let embed_model = embed_model_flag
        .as_deref()
        .unwrap_or(&cfg.embedding.model)
        .to_owned();

    if dry_run {
        println!("Dry run — nothing will be written to the index.\n");
        let mut total_files = 0usize;
        let mut total_chunks = 0usize;
        let mut by_mime: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for root in &roots {
            let entries = walk(root, &walk_cfg)?;
            let files: Vec<_> = entries
                .iter()
                .filter(|e| e.kind == indexa_core::walker::EntryKind::File)
                .collect();
            total_files += files.len();
            for entry in files {
                if let Ok(ex) = indexa_parsers::registry::parse_guarded(
                    &entry.path,
                    entry.size,
                    max_parse_bytes,
                ) {
                    total_chunks += ex.chunks.len();
                    let family = ex.mime.split('/').next().unwrap_or("other").to_owned();
                    *by_mime.entry(family).or_default() += 1;
                }
            }
        }

        println!("Would parse {total_files} files:");
        let mut pairs: Vec<_> = by_mime.into_iter().collect();
        pairs.sort_by_key(|b| std::cmp::Reverse(b.1));
        for (mime, n) in pairs {
            println!("  {:>5}  {mime}", n);
        }
        println!("\nEstimated embedding calls: {total_chunks} chunks");
        // Use the calibrated ETA table instead of the old hardcoded 300 chunks/min.
        let spec = detect_machine();
        let embed_eta = estimate_eta(&embed_model, 0, total_chunks, 0, 1, spec.is_apple_silicon);
        let sum_eta = estimate_eta(
            &cfg.describer.file_model,
            total_files,
            0,
            600,
            cfg.describer.passes_first,
            spec.is_apple_silicon,
        );
        println!(
            "Estimated time: {} embed + {} summarize = {} total",
            embed_eta.display,
            sum_eta.display,
            format_duration_pub((embed_eta.total_secs + sum_eta.total_secs) as u64),
        );
        println!(
            "  (model: {embed_model} + {}, Apple Silicon: {})",
            cfg.describer.file_model, spec.is_apple_silicon
        );
        println!("  Run `indexa doctor --files {total_files} --chunks {total_chunks}` for a full breakdown.");
        return Ok(());
    }

    let mut store = Store::open(&db_path)?;
    let embedder = build_embedder(cfg, Some(&embed_model))?;

    // Effective contextual-retrieval flag: CLI --contextual OR config [describer] contextual_retrieval.
    let use_contextual = contextual || cfg.describer.contextual_retrieval;
    // Build the contextual LLM once (re-used per file) when the feature is enabled.
    // Uses the same file-describer model and base URL — no extra model pull needed.
    let ctx_llm: Option<OllamaLlm> = if use_contextual {
        let base = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
        Some(OllamaLlm::new(&base, &cfg.describer.file_model).with_num_ctx(cfg.describer.num_ctx))
    } else {
        None
    };
    if use_contextual {
        eprintln!(
            "  contextual retrieval enabled (model: {})",
            cfg.describer.file_model
        );
    }

    // Optional image captioning (opt-in): a vision model adds a caption chunk per image.
    // Built once, gated on [parsers.image] caption; shares the describer's Ollama endpoint.
    let captioner = if cfg.parsers.image.caption {
        let base = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
        Some(
            OllamaLlm::new(&base, cfg.parsers.image.caption_model())
                .with_num_ctx(cfg.describer.num_ctx),
        )
    } else {
        None
    };
    let caption_model = cfg.parsers.image.caption_model().to_owned();
    // Optional audio transcription (opt-in): a whisper.cpp-style CLI per audio file.
    let transcribe = cfg.parsers.audio.transcribe;
    let transcribe_binary = cfg.parsers.audio.transcribe_binary().to_owned();
    let transcribe_model = cfg.parsers.audio.model.clone();

    for root in &roots {
        println!(
            "Deep-scanning {} with embed model '{}'",
            root.display(),
            embed_model
        );
        let entries = walk(root, &walk_cfg)?;
        let files: Vec<_> = entries
            .iter()
            .filter(|e| e.kind == indexa_core::walker::EntryKind::File)
            .collect();

        println!("  parsing {} files...", files.len());
        let mut total_chunks = 0usize;
        let mut skipped = 0usize;

        // Lightweight in-place progress on stderr (carriage-return rewrite), shown only when
        // stderr is a terminal so piped/CI output stays clean. Hand-rolled to avoid pulling in
        // indicatif, whose transitive `number_prefix` dep is flagged unmaintained (RUSTSEC-2025-0119).
        let show_progress = std::io::stderr().is_terminal();
        let total_files = files.len();

        for (i, entry) in files.iter().enumerate() {
            if show_progress {
                let name = entry
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                eprint!("\r\x1b[K  [{}/{total_files}] {:.50}", i + 1, name);
                let _ = std::io::stderr().flush();
            }
            let path_str = entry.path.to_string_lossy().into_owned();

            // Skip-if-unchanged: re-embedding is expensive; skip files whose chunks
            // are already indexed at or after the file's last modification time.
            // Compare against the *fresh* on-disk mtime from this walk, not the DB's
            // `modified_s` — `deep` can run without a preceding `scan`, so the stored
            // mtime may be stale and would wrongly skip an edited file (the web
            // pipeline avoids this by re-scanning first). Fall back to the stored
            // check when the filesystem gives us no mtime.
            let mtime_secs = entry
                .modified
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let is_current = match mtime_secs {
                Some(m) => store
                    .chunks_current_for_mtime(&path_str, m)
                    .unwrap_or(false),
                None => store.chunks_are_current(&path_str).unwrap_or(false),
            };
            if is_current {
                skipped += 1;
                continue;
            }

            let mut extracted = match indexa_parsers::registry::parse_guarded(
                &entry.path,
                entry.size,
                max_parse_bytes,
            ) {
                Ok(e) => e,
                Err(_) => continue,
            };

            // Image captioning (opt-in): append a vision-model caption as an extra chunk
            // (kept alongside the EXIF chunk, not replacing it — both are searchable).
            if let Some(cap) = &captioner {
                if extracted.mime.starts_with("image/") {
                    match indexa_llm::caption_image_file(cap, &caption_model, &entry.path).await {
                        Ok(text) if !text.trim().is_empty() => {
                            let seq = extracted.chunks.len();
                            extracted.chunks.push(indexa_parsers::types::Chunk {
                                source: entry.path.clone(),
                                seq,
                                heading: "caption".to_owned(),
                                text,
                                language: None,
                            });
                        }
                        Ok(_) => {}
                        Err(e) => {
                            // Warn unconditionally (clearing the progress line first on a TTY)
                            // so the failure isn't lost on piped/CI runs.
                            if show_progress {
                                eprint!("\r\x1b[K");
                            }
                            eprintln!("  caption failed for {path_str}: {e:#}");
                        }
                    }
                }
            }

            // Audio transcription (opt-in): append a whisper transcript as an extra chunk
            // alongside the ffprobe metadata chunk. Blocking subprocess → spawn_blocking.
            if transcribe && extracted.mime.starts_with("audio/") {
                let bin = transcribe_binary.clone();
                let model = transcribe_model.clone();
                let p = entry.path.clone();
                let res = tokio::task::spawn_blocking(move || {
                    indexa_parsers::media::transcribe_audio(&p, &bin, model.as_deref())
                })
                .await;
                match res {
                    Ok(Ok(text)) if !text.trim().is_empty() => {
                        let seq = extracted.chunks.len();
                        extracted.chunks.push(indexa_parsers::types::Chunk {
                            source: entry.path.clone(),
                            seq,
                            heading: "transcript".to_owned(),
                            text,
                            language: None,
                        });
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        if show_progress {
                            eprint!("\r\x1b[K");
                        }
                        eprintln!("  transcription failed for {path_str}: {e:#}");
                    }
                    Err(e) => {
                        if show_progress {
                            eprint!("\r\x1b[K");
                        }
                        eprintln!("  transcription task panicked for {path_str}: {e}");
                    }
                }
            }

            if extracted.chunks.is_empty() {
                continue;
            }

            // Embed all of a file's chunks in batched round-trips (≫ faster than one HTTP
            // call per chunk), preserving order; per-chunk-resilient on a batch failure.
            // With contextual retrieval enabled, each chunk is first enriched with a
            // 1–2 sentence situating blurb from the file LLM; the ORIGINAL text is stored,
            // but the ENRICHED text (blurb + chunk) is what gets embedded. This is the
            // Anthropic Contextual Retrieval technique (−35% retrieval failures).
            let raw_texts: Vec<&str> = extracted.chunks.iter().map(|c| c.text.as_str()).collect();
            let embed_texts: Vec<String> = if let Some(ref llm) = ctx_llm {
                let doc_context = indexa_query::contextual::build_doc_context(&raw_texts);
                let path_str_clone = path_str.clone();
                indexa_query::contextual::contextual_embed_texts(
                    llm,
                    &doc_context,
                    &raw_texts,
                    None,
                    &path_str,
                    move |event| match event {
                        ContextualEvent::BlurbFragment { .. } => {} // silent — no streaming to stderr
                        ContextualEvent::BlurbFailed { error, .. } => {
                            eprintln!("  ⚠  {path_str_clone}: context blurb failed: {error}");
                        }
                    },
                )
                .await
            } else {
                raw_texts.iter().map(|s| s.to_string()).collect()
            };
            let embed_text_refs: Vec<&str> = embed_texts.iter().map(|s| s.as_str()).collect();
            let mut embeddings = indexa_embed::embed_all(
                embedder.as_ref(),
                &embed_text_refs,
                indexa_embed::EMBED_BATCH_SIZE,
            )
            .await;
            // Drop embeddings whose dim ≠ the configured `[embedding] dim` (model/config
            // mismatch) — they'd corrupt dense search; the chunk stays BM25-searchable.
            let (dim_mismatch, sample_dim) =
                indexa_embed::enforce_embedding_dim(&mut embeddings, cfg.embedding.dim);
            if dim_mismatch > 0 {
                eprintln!(
                    "  ⚠  {dim_mismatch} chunk(s) in {path_str} embedded at dim {} ≠ configured {} \
                     — stored text-only; fix [embedding] model/dim and re-run deep.",
                    sample_dim.unwrap_or(0),
                    cfg.embedding.dim
                );
            }
            let embed_failures = embeddings.iter().filter(|e| e.is_none()).count();
            if embed_failures > 0 && dim_mismatch == 0 {
                eprintln!(
                    "  ⚠  {embed_failures}/{} chunk(s) in {path_str} failed to embed (stored text-only).",
                    embeddings.len()
                );
            }
            let mut chunk_records = Vec::with_capacity(extracted.chunks.len());
            for (chunk, embedding) in extracted.chunks.iter().zip(embeddings) {
                chunk_records.push(ChunkRecord {
                    entry_path: path_str.clone(),
                    seq: chunk.seq,
                    heading: chunk.heading.clone(),
                    text: chunk.text.clone(),
                    language: chunk.language.clone(),
                    embedding,
                    embed_model: Some(embed_model.clone()),
                });
            }

            store.upsert_chunks(&chunk_records)?;
            total_chunks += chunk_records.len();

            // Persist the file's code-graph edges (imports/defines) keyed on the same
            // entry-path string as its chunks, so `edges_from(path)` lines up with search.
            if !extracted.edges.is_empty() {
                let edge_records: Vec<EdgeRecord> = extracted
                    .edges
                    .iter()
                    .map(|e| EdgeRecord {
                        from_path: path_str.clone(),
                        kind: e.kind.to_owned(),
                        to_ref: e.to.clone(),
                    })
                    .collect();
                // Best-effort (parity with the web deep path): code-graph edges are an
                // enrichment, not the index — a failure warns rather than aborting the scan.
                if let Err(e) = store.upsert_edges(&edge_records) {
                    eprintln!(
                        "  ⚠  {path_str}: failed to store {} code-graph edge(s): {e:#}",
                        edge_records.len()
                    );
                }
            }
        }

        if show_progress {
            eprint!("\r\x1b[K"); // clear the progress line
            let _ = std::io::stderr().flush();
        }
        if skipped > 0 {
            println!("  skipped {skipped}/{} files (unchanged)", files.len());
        }
        println!("  embedded {total_chunks} new chunks.");
    }

    // Enqueue summarization for non-Augment modes or always to populate the queue
    if summary_mode != SummaryMode::SummariesOnly {
        for root in &roots {
            match enqueue_subtree(&mut store, root) {
                Ok(n) if n > 0 => println!(
                    "  enqueued {n} items for background summarization. Run `indexa worker` or use the web UI."
                ),
                Ok(_) => {}
                Err(e) => println!("  warning: failed to enqueue summaries: {e}"),
            }
        }
    }

    println!("\nDeep index done. Run `indexa ask \"<question>\"` to query.");
    Ok(())
}
