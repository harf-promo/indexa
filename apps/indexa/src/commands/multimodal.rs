use std::path::Path;

use anyhow::Result;
use indexa_core::config::{self, Config};

use super::helpers::have;

/// What's ready on this machine (external tools + a vision model), shared by the `multimodal`
/// command and the `doctor` readiness section.
pub(crate) struct Readiness {
    pub vision_ok: bool,
    pub ocr_ready: bool,
    pub whisper: bool,
    pub video_ready: bool,
    pub vision_model: String,
    pub whisper_bin: String,
}

/// Detect multimodal readiness + print the report (header + a line per feature). Returns the
/// [`Readiness`] so a caller (`--enable`) can act on it. Fail-open: a missing tool / unreachable
/// Ollama ⇒ that feature is simply "unavailable", never an error.
pub(crate) async fn multimodal_readiness(cfg: &Config) -> Readiness {
    // External tools (fail-open: a missing binary ⇒ not ready).
    let tesseract = have("tesseract");
    let pdftoppm = have("pdftoppm");
    let ffmpeg = have("ffmpeg");
    let whisper_bin = cfg.parsers.audio.transcribe_binary().to_owned();
    let whisper = have(&whisper_bin);

    // A vision model in Ollama (captioning uses the configured/default vision model).
    let vision_model = cfg.parsers.image.caption_model().to_owned();
    let base = vision_probe_base_url(cfg);
    let installed = indexa_llm::ollama_list_models(&base)
        .await
        .unwrap_or_default();
    let vision_ok = model_present(&installed, &vision_model);

    let r = Readiness {
        vision_ok,
        ocr_ready: tesseract && pdftoppm,
        whisper,
        video_ready: ffmpeg && vision_ok,
        vision_model,
        whisper_bin,
    };

    println!("Multimodal readiness\n");
    feat_line(
        "Image captioning",
        r.vision_ok,
        cfg.parsers.image.caption,
        &format!("vision model `{}` pulled in Ollama", r.vision_model),
        "[parsers.image] caption = true",
    );
    feat_line(
        "PDF OCR (scanned PDFs)",
        r.ocr_ready,
        cfg.parsers.pdf.ocr_enabled(),
        "tesseract + pdftoppm (poppler) on PATH",
        "[parsers.pdf] backend = \"ocr\"",
    );
    feat_line(
        "Audio transcription",
        r.whisper,
        cfg.parsers.audio.transcribe,
        &format!("`{}` on PATH (+ a whisper model file)", r.whisper_bin),
        "[parsers.audio] transcribe = true",
    );
    feat_line(
        "Video frame captioning",
        r.video_ready,
        cfg.parsers.video.caption,
        "ffmpeg on PATH + a vision model",
        "[parsers.video] caption = true",
    );
    r
}

/// `indexa multimodal [--enable]` — the multimodal parsers (image captioning, PDF OCR, audio
/// transcription, video-frame captioning) are fully built but opt-in; this reports which are ready
/// and, with `--enable`, turns on the `[parsers.*]` flags for the ready ones.
///
/// `--enable` uses the SAFE config round-trip (`config::load` → mutate → `config::save`): `load`
/// returns the default config when the file is missing, but **errors if it exists and fails to
/// parse**, so a broken config is never clobbered (the v0.69 anti-wipe lesson). Written 0600.
pub(crate) async fn cmd_multimodal(enable: bool, cfg: &Config, config_path: &Path) -> Result<()> {
    let r = multimodal_readiness(cfg).await;

    if !enable {
        println!(
            "\nRun `indexa multimodal --enable` to turn on every ready feature, then re-index to apply."
        );
        return Ok(());
    }

    // --enable: load (refuse on parse error), flip the ready-but-not-yet-on features, save safely.
    // Round-trip the SAME path the CLI loaded (honors --config), not the hardcoded default.
    let mut c = config::load(config_path)?;
    let mut changed: Vec<&str> = Vec::new();
    if r.vision_ok && !c.parsers.image.caption {
        c.parsers.image.caption = true;
        changed.push("image captioning");
    }
    if r.ocr_ready && !c.parsers.pdf.ocr_enabled() {
        c.parsers.pdf.backend = "ocr".to_owned();
        changed.push("PDF OCR");
    }
    if r.whisper && !c.parsers.audio.transcribe {
        c.parsers.audio.transcribe = true;
        changed.push("audio transcription");
    }
    if r.video_ready && !c.parsers.video.caption {
        c.parsers.video.caption = true;
        changed.push("video frame captioning");
    }

    if changed.is_empty() {
        println!(
            "\nNothing to enable — every ready feature is already on, and the rest are missing tools."
        );
        return Ok(());
    }
    config::save(&c, config_path)?;
    println!(
        "\n✅ Enabled: {}. Re-index (`indexa index <path>`) to apply.",
        changed.join(", ")
    );
    if r.whisper && c.parsers.audio.model.is_none() {
        println!(
            "ℹ️  Audio: also set `[parsers.audio] model = \"/path/to/ggml-*.bin\"` — whisper needs a model file."
        );
    }
    Ok(())
}

/// The Ollama host to probe for the vision/captioning model. Image/video captioning shares the
/// describer's Ollama endpoint (see `deep.rs`'s captioner setup), NOT the embedder's —
/// `cfg.embedding.base_url` and `cfg.describer.base_url` are independently settable (see
/// `helpers.rs`'s `ollama_requirements_resolves_each_provider_against_its_own_base`), so
/// resolving against the wrong one under-/over-reports readiness whenever the two point at
/// different hosts. Split out to a pure function so this resolution is unit-testable without a
/// live/mocked Ollama server.
fn vision_probe_base_url(cfg: &Config) -> String {
    indexa_llm::OllamaLlm::resolve_base_url(Some(cfg.describer.base_url.as_str()))
}

fn feat_line(name: &str, ready: bool, already_on: bool, needs: &str, flag: &str) {
    let status = if already_on {
        "✅ enabled"
    } else if ready {
        "ℹ️  ready"
    } else {
        "⚠️  unavailable"
    };
    println!("  {status:<16}{name}");
    if !ready {
        println!("    needs: {needs}");
    } else if !already_on {
        println!("    enable: {flag}");
    }
}

/// The model name without its `:tag` suffix.
fn model_base(s: &str) -> &str {
    s.split(':').next().unwrap_or(s)
}

/// Lenient model-presence check: exact match, or the same base name ignoring the `:tag`
/// (so a configured `gemma3:4b` matches an installed `gemma3:4b`, and `gemma3` matches either).
fn model_present(installed: &[String], want: &str) -> bool {
    installed
        .iter()
        .any(|m| m == want || model_base(m) == model_base(want))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test: the vision-model readiness check must resolve against the describer's
    /// Ollama host, not the embedder's — they're independently configurable, and captioning
    /// actually runs against the describer's endpoint (`deep.rs`).
    #[test]
    fn vision_probe_resolves_against_describer_base_not_embedder_base() {
        let mut cfg = Config::default();
        cfg.embedding.base_url = "http://embed-host:11434".into();
        cfg.describer.base_url = "http://describer-host:11434".into();
        assert_eq!(vision_probe_base_url(&cfg), "http://describer-host:11434");
    }
}
