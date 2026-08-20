use anyhow::Result;
use indexa_core::{
    config::{Config, SummaryMode},
    resource::{detect_machine, estimate_eta, format_duration_pub},
    store::{chunk_content_hash, ChunkRecord, EdgeRecord, Store, SymbolRecord},
    walker::{walk, WalkConfig},
};
use indexa_llm::OllamaLlm;
use indexa_query::{contextual::ContextualEvent, enqueue_subtree, redact::chunk_text_for_store};
use std::io::{IsTerminal, Write};

use super::helpers::{
    build_embedder, preflight_ollama, require_index_db, resolve_summary_mode, resolve_target_roots,
};

#[allow(clippy::too_many_arguments)] // thin CLI fan-out; grouping into a struct would just move fields
pub(crate) async fn cmd_deep(
    paths: Vec<String>,
    embed_model_flag: Option<String>,
    dry_run: bool,
    mode: Option<String>,
    contextual: bool,
    contextual_prefix: bool,
    no_embed: bool,
    cfg: &Config,
) -> Result<()> {
    // ── Preflight: confirm Ollama is up and required models are pulled ─────────
    // Skip during dry-run (no actual model calls are made) and in `--no-embed`
    // mode (FTS-only: no embeddings, no contextual/caption/transcribe/OCR calls,
    // so no model needs to be reachable — this is what makes a CI run hermetic).
    // NOT skipped for `summaries-only` alone: that mode still needs the file_model/
    // dir_model describer check preflighted here, since `cmd_index`'s later summarize
    // phase is the one that actually calls them.
    if !dry_run && !no_embed {
        preflight_ollama(cfg).await?;
    }

    let summary_mode = resolve_summary_mode(mode.as_deref(), cfg.describer.mode.clone())?;
    // `summaries-only` never stores chunks, so every model call that only exists to enrich
    // stored chunks (embeddings, contextual blurbs, image captions, audio transcription, PDF
    // OCR) is wasted work here — treat it exactly like `--no-embed` for all of those, while
    // still parsing + writing entries/edges/symbols. `summarize_file` re-parses the file
    // itself when no chunks are stored (`sample_via_parse` in `indexa_query::summarize`), so
    // this phase doesn't need to feed it a sample — but that means the describer prompt is
    // built from the default 800/100-word registry, not this pass's `[chunking]`/parser config.
    let skip_embed_work = no_embed || summary_mode == SummaryMode::SummariesOnly;
    let roots = resolve_target_roots(paths, false)?;
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let max_parse_bytes = cfg.parsers.max_file_mb.saturating_mul(1024 * 1024);
    // Parser registry honoring `[chunking]` size/overlap; built once and reused for every file
    // (both the dry-run count and the real parse) instead of the free `parse_guarded`.
    let registry = super::helpers::chunk_registry(cfg);
    let walk_cfg = WalkConfig {
        respect_gitignore: cfg.scan.respect_gitignore,
        ignore: cfg.scan.ignore.clone(),
        // Whole-computer groundwork: when `[scan] skip_binary` is on, the walk NUL-sniffs files
        // so the loops below can skip binaries without opening them for a parse attempt.
        sniff_binary: cfg.scan.skip_binary,
        threads: cfg.scan.threads,
        custom_ignore: cfg.scan.custom_ignore,
        ..Default::default()
    };

    let embed_model = embed_model_flag
        .as_deref()
        .unwrap_or(&cfg.embedding.model)
        .to_owned();

    if dry_run {
        println!("Dry run — nothing will be written to the index.\n");
        // Collect the file set (path + size) across roots up front; family is classified by
        // extension (cheap, no parse) so it's always exact and independent of the chunk-count
        // sampling below — deliberately a coarser, faster axis than the old MIME-sniffed
        // breakdown (see `classify_file_by_extension`'s `category` — extension-based
        // classification misses files with wrong/missing extensions that MIME sniffing would
        // have caught, e.g. an extensionless script or a mislabeled `.txt`).
        let mut all_files: Vec<(std::path::PathBuf, u64)> = Vec::new();
        let mut by_family: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for root in &roots {
            let entries = walk(root, &walk_cfg)?;
            for e in entries
                .iter()
                .filter(|e| e.kind == indexa_core::walker::EntryKind::File && !e.is_binary)
            {
                let family = indexa_core::surface::classify_file_by_extension(&e.path)
                    .map(|h| h.category.to_owned())
                    .unwrap_or_else(|| "other".to_owned());
                *by_family.entry(family).or_default() += 1;
                all_files.push((e.path.clone(), e.size));
            }
        }
        let total_files = all_files.len();
        // `walk()` honors `[scan] threads` and returns entries in whatever order the parallel
        // walk completed in — nondeterministic across runs. Sort before sampling so the
        // evenly-spaced sample (and therefore the estimate below) is a deterministic function of
        // the file set: an unchanged tree reports the same estimated chunk count every time,
        // rather than a different sample producing a different number on each `--dry-run`.
        all_files.sort();

        // Chunk count: a tree at or below `DRY_RUN_SAMPLE_MAX` files is parsed in full (the
        // sample IS the whole set, so the count is exact). A larger tree parses an
        // evenly-spaced sample of that many files and extrapolates chunks-per-byte to the
        // whole tree — trading a few percent of accuracy for a preview that doesn't take
        // nearly as long as the real deep run it's previewing. See `estimate_total_chunks`.
        const DRY_RUN_SAMPLE_MAX: usize = 64;
        let (total_chunks, sampled, was_sampled) =
            estimate_total_chunks(&all_files, DRY_RUN_SAMPLE_MAX, max_parse_bytes, |p, sz| {
                registry
                    .parse_guarded(p, sz, max_parse_bytes)
                    .ok()
                    .map(|ex| ex.chunks.len())
            });

        if was_sampled {
            println!(
                "Would parse {total_files} files (chunks estimated from a {sampled}-file sample):"
            );
        } else {
            println!("Would parse {total_files} files:");
        }
        let mut pairs: Vec<_> = by_family.into_iter().collect();
        pairs.sort_by_key(|b| std::cmp::Reverse(b.1));
        for (family, n) in pairs {
            println!("  {:>5}  {family}", n);
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
    // `--no-embed` / `summaries-only` build no embedder at all.
    let embedder = if skip_embed_work {
        None
    } else {
        Some(build_embedder(cfg, Some(&embed_model))?)
    };

    // Effective contextual-retrieval flag: CLI --contextual OR config [describer] contextual_retrieval.
    // Forced off when embedding work is skipped — a situating blurb needs an LLM call.
    let use_contextual = !skip_embed_work && (contextual || cfg.describer.contextual_retrieval);
    // Effective deterministic contextual-prefix flag: CLI --contextual-prefix OR config
    // [describer] contextual_prefix. Forced off when embedding work is skipped (nothing is embedded).
    let use_prefix = !skip_embed_work && (contextual_prefix || cfg.describer.contextual_prefix);
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
    let captioner = if !skip_embed_work && cfg.parsers.image.caption {
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
    // Disabled when embedding work is skipped (that path makes no model/tool calls).
    let transcribe = !skip_embed_work && cfg.parsers.audio.transcribe;
    let transcribe_binary = cfg.parsers.audio.transcribe_binary().to_owned();
    let transcribe_model = cfg.parsers.audio.model.clone();
    // Optional PDF OCR (opt-in): pdftoppm + tesseract for scanned PDFs with no text layer.
    // Disabled when embedding work is skipped (that path makes no model/tool calls).
    let ocr_enabled = !skip_embed_work && cfg.parsers.pdf.ocr_enabled();
    let ocr_binary = cfg.parsers.pdf.ocr_binary().to_owned();
    let ocr_lang = cfg.parsers.pdf.ocr_lang.clone();

    for root in &roots {
        if no_embed {
            println!(
                "Deep-scanning {} (FTS only — no embeddings)",
                root.display()
            );
        } else if summary_mode == SummaryMode::SummariesOnly {
            println!(
                "Deep-scanning {} (summaries-only — chunks not stored)",
                root.display()
            );
        } else {
            println!(
                "Deep-scanning {} with embed model '{}'",
                root.display(),
                embed_model
            );
        }
        let entries = walk(root, &walk_cfg)?;
        // `[scan] skip_binary` sniffs binaries during the walk; skip them here so `deep`/`index`
        // never opens/parses an executable/image/DB blob (matches the dry-run + web deep paths).
        // Secret files (`.env`, keys, `.pem`/keystores) are recorded by scan but not embedded
        // unless `[scan] include_sensitive` — redaction can't scrub a raw key, so their contents
        // stay out of the searchable index by default.
        let include_sensitive = walk_cfg.include_sensitive;
        let is_sensitive = |e: &&indexa_core::walker::Entry| {
            e.hint
                .as_ref()
                .is_some_and(|h| h.deep_scan == indexa_core::surface::DeepScanPolicy::Sensitive)
        };
        let files: Vec<_> = entries
            .iter()
            .filter(|e| {
                e.kind == indexa_core::walker::EntryKind::File
                    && !e.is_binary
                    && (include_sensitive || !is_sensitive(e))
            })
            .collect();
        let binaries_skipped = entries
            .iter()
            .filter(|e| e.kind == indexa_core::walker::EntryKind::File && e.is_binary)
            .count();
        let sensitive_skipped = if include_sensitive {
            0
        } else {
            entries
                .iter()
                .filter(|e| {
                    e.kind == indexa_core::walker::EntryKind::File
                        && !e.is_binary
                        && is_sensitive(e)
                })
                .count()
        };

        let mut notes: Vec<String> = Vec::new();
        if binaries_skipped > 0 {
            notes.push(format!("{binaries_skipped} binaries"));
        }
        if sensitive_skipped > 0 {
            notes.push(format!("{sensitive_skipped} sensitive"));
        }
        if notes.is_empty() {
            println!("  parsing {} files...", files.len());
        } else {
            println!(
                "  parsing {} files ({} skipped)...",
                files.len(),
                notes.join(", ")
            );
        }
        let mut total_chunks = 0usize;
        let mut skipped = 0usize;

        // Lightweight in-place progress on stderr (carriage-return rewrite), shown only when
        // stderr is a terminal so piped/CI output stays clean. Hand-rolled to avoid pulling in
        // indicatif, whose transitive `number_prefix` dep is flagged unmaintained (RUSTSEC-2025-0119).
        let show_progress = std::io::stderr().is_terminal();
        let total_files = files.len();
        let prog_start = std::time::Instant::now();

        for (i, entry) in files.iter().enumerate() {
            if show_progress {
                let name = entry
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                // Cumulative rate + ETA so a long deep pass shows it's progressing, not frozen.
                let done = i + 1;
                let elapsed = prog_start.elapsed().as_secs_f64();
                let rate = if elapsed > 0.5 {
                    done as f64 / elapsed
                } else {
                    0.0
                };
                let eta = if rate > 0.0 {
                    indexa_core::resource::format_duration_pub(
                        ((total_files - done) as f64 / rate) as u64,
                    )
                } else {
                    "—".to_string()
                };
                eprint!("\r\x1b[K  [{done}/{total_files}] {rate:.0}/s · ETA {eta} · {name:.40}");
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
            // NOTE: in `summaries-only` mode this check can never see a chunk row (none are
            // ever stored), so it always re-parses every file on every pass — cheap relative
            // to embedding, but the dominant cost once nothing is embedded. Known, not a bug;
            // a real fix needs a freshness marker independent of the chunks table.
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

            let mut extracted =
                match registry.parse_guarded(&entry.path, entry.size, max_parse_bytes) {
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

            // PDF OCR (opt-in): a scanned PDF with no text layer gets rasterised + OCR'd and
            // the recognised text appended as an extra chunk. Blocking subprocess →
            // spawn_blocking; fails open (a missing tool just leaves the text-layer stub).
            if ocr_enabled && extracted.mime == "application/pdf" {
                let layer_words: usize = extracted
                    .chunks
                    .iter()
                    .map(|c| c.text.split_whitespace().count())
                    .sum();
                if layer_words < 10 {
                    let bin = ocr_binary.clone();
                    let lang = ocr_lang.clone();
                    let p = entry.path.clone();
                    let res = tokio::task::spawn_blocking(move || {
                        indexa_parsers::pdf::ocr_pdf(&p, &bin, lang.as_deref())
                    })
                    .await;
                    match res {
                        Ok(Ok(text)) if !text.trim().is_empty() => {
                            let seq = extracted.chunks.len();
                            extracted.chunks.push(indexa_parsers::types::Chunk {
                                source: entry.path.clone(),
                                seq,
                                heading: "ocr".to_owned(),
                                text,
                                language: None,
                            });
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            if show_progress {
                                eprint!("\r\x1b[K");
                            }
                            eprintln!("  OCR failed for {path_str}: {e:#}");
                        }
                        Err(e) => {
                            if show_progress {
                                eprint!("\r\x1b[K");
                            }
                            eprintln!("  OCR task panicked for {path_str}: {e}");
                        }
                    }
                }
            }

            if extracted.chunks.is_empty() {
                continue;
            }

            // Compute SHA-256 of each chunk's raw text — used as a cache key to skip
            // re-embedding chunks whose content is unchanged since the last deep run.
            // The hash is over the ORIGINAL text (not the enriched blurb) so the cache
            // stays valid across contextual-retrieval runs on the same source text.
            let chunk_hashes: Vec<String> = extracted
                .chunks
                .iter()
                .map(|c| chunk_content_hash(&c.text))
                .collect();

            // Resolve a per-chunk embedding vector (aligned to `extracted.chunks`).
            // `--no-embed` stores every chunk text-only (vector = None) for sparse/FTS
            // search; a later plain `deep` self-heals them (the skip-if-current check
            // requires COUNT(*) = COUNT(embedding), so vector-less chunks aren't "current").
            // `summaries-only` never stores the chunk at all, so its embedding is moot —
            // skip computing one exactly like `--no-embed` does.
            let all_embeddings: Vec<Option<Vec<f32>>> = if skip_embed_work {
                vec![None; extracted.chunks.len()]
            } else {
                // Load the cached embedding map for this file (hash → Vec<f32>).
                // Fail-open: if the lookup errors (e.g. first run, column missing), treat as empty.
                let hash_cache = store
                    .cached_embeddings_by_hash(&path_str)
                    .unwrap_or_default();

                // Partition chunks into cache-hits (no embed needed) and misses (must embed).
                // A hit requires BOTH a matching hash AND a stored non-NULL vector.
                let mut cache_hits: Vec<Option<Vec<f32>>> = vec![None; extracted.chunks.len()];
                let mut miss_indices: Vec<usize> = Vec::new();
                for (i, hash) in chunk_hashes.iter().enumerate() {
                    if let Some(cached_vec) = hash_cache.get(hash) {
                        cache_hits[i] = Some(cached_vec.clone());
                    } else {
                        miss_indices.push(i);
                    }
                }

                // Build embed-text only for cache-miss chunks. With contextual retrieval
                // enabled, enrich each miss chunk with a situating blurb before embedding.
                let miss_raw_texts: Vec<&str> = miss_indices
                    .iter()
                    .map(|&i| extracted.chunks[i].text.as_str())
                    .collect();
                let miss_embed_texts: Vec<String> = if !miss_raw_texts.is_empty() {
                    if let Some(ref llm) = ctx_llm {
                        // Build doc context from the FULL file (all chunks), not just misses,
                        // so the situating blurbs are grounded in the whole document.
                        let all_raw: Vec<&str> =
                            extracted.chunks.iter().map(|c| c.text.as_str()).collect();
                        let doc_context = indexa_query::contextual::build_doc_context(&all_raw);
                        let path_str_clone = path_str.clone();
                        indexa_query::contextual::contextual_embed_texts(
                            llm,
                            &doc_context,
                            &miss_raw_texts,
                            None,
                            &path_str,
                            move |event| match event {
                                ContextualEvent::BlurbFragment { .. } => {}
                                ContextualEvent::BlurbFailed { error, .. } => {
                                    eprintln!(
                                        "  ⚠  {path_str_clone}: context blurb failed: {error}"
                                    );
                                }
                            },
                        )
                        .await
                    } else if use_prefix {
                        // Deterministic, local, no-LLM contextual prefix: prepend the file path,
                        // section heading, and a document-context snippet to each miss chunk's
                        // embed input. Grounds the embedding in the whole document at zero token
                        // cost. Applied to embed text ONLY — the stored/hashed text is untouched.
                        let all_raw: Vec<&str> =
                            extracted.chunks.iter().map(|c| c.text.as_str()).collect();
                        let doc_context = indexa_query::contextual::build_doc_context(&all_raw);
                        let miss_headings: Vec<&str> = miss_indices
                            .iter()
                            .map(|&i| extracted.chunks[i].heading.as_str())
                            .collect();
                        indexa_query::contextual::contextual_prefix_texts(
                            &doc_context,
                            &miss_headings,
                            &miss_raw_texts,
                            &path_str,
                        )
                    } else {
                        miss_raw_texts.iter().map(|s| s.to_string()).collect()
                    }
                } else {
                    Vec::new()
                };

                // Embed only the cache-miss chunks.
                let miss_embed_refs: Vec<&str> =
                    miss_embed_texts.iter().map(|s| s.as_str()).collect();
                let mut miss_embeddings = if !miss_embed_refs.is_empty() {
                    indexa_embed::embed_all(
                        embedder
                            .as_ref()
                            .expect("embedder is built whenever embedding work isn't skipped")
                            .as_ref(),
                        &miss_embed_refs,
                        indexa_embed::EMBED_BATCH_SIZE,
                    )
                    .await
                } else {
                    Vec::new()
                };

                // Drop embeddings whose dim ≠ the configured `[embedding] dim` (model/config
                // mismatch) — they'd corrupt dense search; the chunk stays BM25-searchable.
                let (dim_mismatch, sample_dim) =
                    indexa_embed::enforce_embedding_dim(&mut miss_embeddings, cfg.embedding.dim);
                if dim_mismatch > 0 {
                    eprintln!(
                        "  ⚠  {dim_mismatch} chunk(s) in {path_str} embedded at dim {} ≠ configured {} \
                         — stored text-only; fix [embedding] model/dim and re-run deep.",
                        sample_dim.unwrap_or(0),
                        cfg.embedding.dim
                    );
                }
                let embed_failures = miss_embeddings.iter().filter(|e| e.is_none()).count();
                if embed_failures > 0 && dim_mismatch == 0 {
                    eprintln!(
                        "  ⚠  {embed_failures}/{} chunk(s) in {path_str} failed to embed (stored text-only).",
                        miss_embeddings.len()
                    );
                }

                // Merge cache hits and fresh embeddings into one aligned vector.
                let mut miss_iter = miss_embeddings.into_iter();
                let mut merged: Vec<Option<Vec<f32>>> = Vec::with_capacity(extracted.chunks.len());
                for slot in cache_hits.iter_mut().take(extracted.chunks.len()) {
                    if slot.is_some() {
                        merged.push(slot.take());
                    } else {
                        merged.push(miss_iter.next().unwrap_or(None));
                    }
                }
                merged
            };

            let mut chunk_records = Vec::with_capacity(extracted.chunks.len());
            for ((chunk, embedding), hash) in extracted
                .chunks
                .iter()
                .zip(all_embeddings)
                .zip(chunk_hashes)
            {
                // Redact obvious secrets before writing to the searchable store (shared choke
                // point so every index path — deep + watch, CLI + web — behaves identically).
                let text = chunk_text_for_store(&chunk.text, cfg.scan.redact_at_index);
                chunk_records.push(ChunkRecord {
                    entry_path: path_str.clone(),
                    seq: chunk.seq,
                    heading: chunk.heading.clone(),
                    text,
                    language: chunk.language.clone(),
                    embedding,
                    // No model produced a vector when embedding work is skipped → leave it NULL.
                    embed_model: if skip_embed_work {
                        None
                    } else {
                        Some(embed_model.clone())
                    },
                    content_hash: Some(hash),
                });
            }

            // `deep` can run without a preceding `scan` (its own skip-if-unchanged comment
            // above says so), so without this the file has no `entries` row — its chunks are
            // orphans: never summarized (`entries_for_summarization`/`enqueue_subtree` skip
            // entry-less paths) and silently deleted the next time `prune_orphans` runs (every
            // `indexa scan`, once ANY entries row exists) — wiping the embedding work this pass
            // just paid for. `upsert_entries` is an idempotent ON-CONFLICT upsert (matches the
            // `watch` command's write path, which already does this correctly). Always written
            // regardless of mode — `summaries-only` still needs a live entries row so the file
            // is summarizable.
            store.upsert_entries(&[(**entry).clone()])?;
            // `summaries-only` never persists chunk rows — that's the entire ~100× size win;
            // `summarize_file` re-parses the file itself (via the default registry) when no
            // chunks are stored, so nothing downstream needs these in the store.
            if summary_mode != SummaryMode::SummariesOnly {
                store.upsert_chunks(&chunk_records)?;
            }
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
                // Symbols (2.1): kind + line range, extracted alongside `defines` edges.
                // Same call-when-non-empty convention as the edges block above (and its
                // known limitation: a file that goes from N symbols to zero on a re-deep
                // doesn't clear its old rows here — matches upsert_edges' existing behavior).
                let symbol_records: Vec<SymbolRecord> = extracted
                    .edges
                    .iter()
                    .filter(|e| e.kind == "defines")
                    .filter_map(|e| {
                        let (start, end) = e.line_range?;
                        Some(SymbolRecord {
                            path: path_str.clone(),
                            name: e.to.clone(),
                            kind: e.symbol_kind.unwrap_or("other").to_owned(),
                            start_line: start as i64,
                            end_line: end as i64,
                        })
                    })
                    .collect();
                if !symbol_records.is_empty() {
                    if let Err(e) = store.upsert_symbols(&symbol_records) {
                        eprintln!(
                            "  ⚠  {path_str}: failed to store {} symbol(s): {e:#}",
                            symbol_records.len()
                        );
                    }
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
        if no_embed {
            println!("  indexed {total_chunks} new chunks (FTS only, no embeddings).");
        } else if summary_mode == SummaryMode::SummariesOnly {
            println!(
                "  parsed {total_chunks} chunks (summaries-only — not stored; entries + code graph updated)."
            );
        } else {
            println!("  embedded {total_chunks} new chunks.");
        }
    }

    // Enqueue summarization for every mode — `summaries-only` needs this MOST, since the
    // summary is the only artifact this mode produces (no chunks/embeddings are stored above).
    for root in &roots {
        match enqueue_subtree(&mut store, root) {
            Ok(n) if n > 0 => println!(
                "  enqueued {n} items for background summarization. Run `indexa worker` or use the web UI."
            ),
            Ok(_) => {}
            Err(e) => println!("  warning: failed to enqueue summaries: {e}"),
        }
    }

    println!("\nDeep index done. Run `indexa ask \"<question>\"` to query.");
    Ok(())
}

/// Estimates `deep --dry-run`'s chunk count for a file population: exactly for a small tree, or
/// via an evenly-spaced sample extrapolated by chunks-per-byte for a large one.
///
/// File size alone is a poor predictor of chunk count — code chunks per function, far
/// finer-grained than prose — so a pure size heuristic can underestimate a real corpus by
/// multiples. Sampling actual parses and extrapolating their chunks-per-byte ratio to the whole
/// tree is the accurate-yet-cheap middle ground: a `deep --dry-run` preview no longer has to
/// nearly-fully parse a huge tree (the cost it exists to help someone avoid) just to report a
/// count.
///
/// `chunks_of` is injected (rather than this function calling the real parser registry directly)
/// so the selection + extrapolation logic stays pure and unit-testable over a synthetic
/// population; production wires it to `registry.parse_guarded`.
///
/// Returns `(estimated_total_chunks, files_actually_parsed, was_sampled)`. A tree at or below
/// `sample_max` files is parsed in full — the "sample" and "everything" are the same set, so the
/// count is exact and `was_sampled` is `false`.
fn estimate_total_chunks(
    files: &[(std::path::PathBuf, u64)],
    sample_max: usize,
    max_parse_bytes: u64,
    mut chunks_of: impl FnMut(&std::path::Path, u64) -> Option<usize>,
) -> (usize, usize, bool) {
    // A file over the parse-size cap is truncated at parse time (`parse_guarded` refuses it
    // outright above `max_parse_bytes`), so it never contributes more than that many parseable
    // bytes. Clamp both the sample's and the whole tree's byte totals to match, or a tree with a
    // few oversized files skews the chunks-per-byte ratio.
    let effective_bytes = |sz: u64| {
        if max_parse_bytes == 0 {
            sz
        } else {
            sz.min(max_parse_bytes)
        }
    };

    if files.len() <= sample_max {
        let mut total_chunks = 0usize;
        for (path, size) in files {
            if let Some(n) = chunks_of(path, *size) {
                total_chunks += n;
            }
        }
        return (total_chunks, files.len(), false);
    }

    let step = (files.len() / sample_max).max(1);
    let mut sample_chunks = 0usize;
    let mut sample_bytes = 0u64;
    let mut sampled = 0usize;
    for (path, size) in files.iter().step_by(step) {
        // Accumulated unconditionally — even when the parse below fails — so the sample's byte
        // basis matches `total_bytes`'s below. Gating this on parse success (as the file's chunk
        // count already is) would inflate chunks-per-byte by the unparseable fraction, since the
        // numerator (chunks) and denominator (bytes) would no longer describe the same files.
        sample_bytes += effective_bytes(*size);
        if let Some(n) = chunks_of(path, *size) {
            sample_chunks += n;
        }
        sampled += 1;
    }
    let total_bytes: u64 = files.iter().map(|(_, size)| effective_bytes(*size)).sum();
    let estimated = if sample_bytes == 0 {
        0
    } else {
        ((sample_chunks as f64 / sample_bytes as f64) * total_bytes as f64).round() as usize
    };
    (estimated, sampled, true)
}

#[cfg(test)]
mod tests {
    use super::estimate_total_chunks;
    use std::path::PathBuf;

    /// A synthetic population with real chunks-per-byte VARIANCE (not a fixed ratio) — code-like
    /// files chunk far more densely per byte than prose-like ones — so the extrapolation is
    /// actually exercised, not just algebra that cancels out.
    fn synthetic_population(
        n: usize,
    ) -> (Vec<(PathBuf, u64)>, std::collections::HashMap<usize, usize>) {
        let mut files = Vec::with_capacity(n);
        let mut true_chunks = std::collections::HashMap::new();
        for i in 0..n {
            let path = PathBuf::from(format!("/corpus/file_{i}.txt"));
            // Alternate two size/density profiles so the population isn't uniform.
            let (size, chunks) = if i.is_multiple_of(3) {
                (4_000u64, 12usize) // code-like: dense
            } else {
                (2_000u64, 2usize) // prose-like: sparse
            };
            true_chunks.insert(i, chunks);
            files.push((path, size));
        }
        (files, true_chunks)
    }

    fn chunks_for(
        path: &std::path::Path,
        true_chunks: &std::collections::HashMap<usize, usize>,
    ) -> Option<usize> {
        let idx: usize = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("file_"))
            .and_then(|s| s.parse().ok())
            .expect("synthetic path shape");
        true_chunks.get(&idx).copied()
    }

    #[test]
    fn small_tree_is_parsed_exactly_not_sampled() {
        let (files, truth) = synthetic_population(40);
        let exact_total: usize = truth.values().sum();
        let (estimated, parsed, was_sampled) =
            estimate_total_chunks(&files, 64, 0, |p, _sz| chunks_for(p, &truth));
        assert!(!was_sampled, "40 files with sample_max 64 must not sample");
        assert_eq!(parsed, 40);
        assert_eq!(
            estimated, exact_total,
            "at/below sample_max must be exact, not estimated"
        );
    }

    #[test]
    fn large_tree_estimate_is_within_tolerance_of_true_total() {
        let (files, truth) = synthetic_population(2_000);
        let exact_total: usize = truth.values().sum();
        let (estimated, parsed, was_sampled) =
            estimate_total_chunks(&files, 64, 0, |p, _sz| chunks_for(p, &truth));
        assert!(was_sampled, "2000 files with sample_max 64 must sample");
        // `step_by`'s count is `ceil(len / step)`, and `step` itself is a floor division, so the
        // actual sample can land a few files above `sample_max` (e.g. 65 for this population) —
        // assert "approximately the sample size", not the exact bound, while still catching a
        // sampling failure that degenerates into a full parse (2000 files).
        assert!(
            parsed <= 64 + 8,
            "must sample ~sample_max files, not the whole tree; parsed {parsed}"
        );
        // Don't assert exact equality — that's the whole point of sampling. Assert the estimate
        // lands within a generous tolerance of the true total (real-corpus PR measurements were
        // ~4% off; this synthetic population's alternating profile is a harder case, so allow
        // more headroom while still catching a badly broken extrapolation).
        let tolerance = (exact_total as f64 * 0.20).ceil() as usize;
        let diff = estimated.abs_diff(exact_total);
        assert!(
            diff <= tolerance,
            "estimate {estimated} vs true {exact_total} (diff {diff}) exceeds {tolerance} tolerance"
        );
    }

    #[test]
    fn unparseable_files_dont_inflate_the_estimate() {
        // Half of ALL files (not just sampled ones) always fail to parse (return None) — e.g. a
        // binary misdetected as text, or a format the parser doesn't handle. The correct ground
        // truth here is what running this SAME (partially-failing) parse function over the whole
        // tree would produce, not the idealized "every file parses" total — a consistently
        // failing subset would contribute zero chunks in a real exact run too.
        let (files, truth) = synthetic_population(2_000);
        let flaky_chunks_of = |p: &std::path::Path, _sz: u64| -> Option<usize> {
            let idx: usize = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("file_"))
                .and_then(|s| s.parse().ok())
                .unwrap();
            if idx.is_multiple_of(2) {
                None // simulate a parse failure
            } else {
                truth.get(&idx).copied()
            }
        };
        let full_total: usize = files
            .iter()
            .filter_map(|(p, sz)| flaky_chunks_of(p, *sz))
            .sum();

        let (estimated, _parsed, was_sampled) =
            estimate_total_chunks(&files, 64, 0, flaky_chunks_of);
        assert!(was_sampled);

        // Regression guard: if the byte basis were gated on parse success instead of accumulated
        // unconditionally, chunks-per-byte would be computed over only the successfully-parsed
        // sampled bytes but multiplied by the WHOLE tree's bytes (successes + failures) —
        // roughly doubling the estimate versus `full_total`. The unconditional basis keeps the
        // sample and total byte counts on the same footing, so this should land close.
        let tolerance = (full_total as f64 * 0.35).ceil() as usize; // sampling noise headroom
        let diff = estimated.abs_diff(full_total);
        assert!(
            diff <= tolerance,
            "estimate {estimated} vs full-parse total {full_total} (diff {diff}) exceeds \
             {tolerance} — byte basis likely regressed to gate on parse success"
        );
    }

    #[test]
    fn oversized_files_are_clamped_on_both_sides() {
        // One file is far larger than `max_parse_bytes` on both sample and total sides; without
        // clamping, its huge byte count would swamp the ratio even though `parse_guarded` would
        // refuse to parse more than the cap in the real path.
        let mut files = vec![(PathBuf::from("/corpus/huge.bin"), 10_000_000u64)];
        let mut truth = std::collections::HashMap::new();
        truth.insert(usize::MAX, 0usize); // the huge file never parses (over the cap)
        for i in 0..200 {
            files.push((PathBuf::from(format!("/corpus/file_{i}.txt")), 1_000));
            truth.insert(i, 3);
        }
        let max_parse_bytes = 5_000u64;
        let (estimated, _parsed, was_sampled) =
            estimate_total_chunks(&files, 64, max_parse_bytes, |p, _sz| {
                if p.file_name().unwrap() == "huge.bin" {
                    None // over the cap: parse_guarded would refuse it
                } else {
                    chunks_for(p, &truth)
                }
            });
        assert!(was_sampled);
        // Without clamping, the ~10MB file alone would make the denominator ~200x too large,
        // crushing the estimate toward ~0. With clamping it should land close to the ~600 true
        // chunks from the 200 small files (huge.bin contributes 0 chunks either way).
        let true_total = 200 * 3;
        assert!(
            estimated > true_total / 2,
            "estimate {estimated} collapsed toward zero — oversized file likely wasn't clamped"
        );
    }
}
