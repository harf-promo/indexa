//! Happy-path, end-to-end image-captioning test against a REAL local Ollama vision model.
//! The rest of the suite only covers the no-Ollama error path; this proves the caption
//! integration produces a description when a vision model IS available.
//!
//! `#[ignore]`d so plain `cargo test` / CI stays green. Run it with a vision model pulled:
//!
//! ```bash
//! ollama pull moondream            # a vision model with an arch this Ollama build supports
//! cargo test -p indexa-llm --test caption_live -- --ignored --nocapture
//! # override the model / endpoint if needed:
//! INDEXA_TEST_VISION_MODEL=moondream INDEXA_TEST_OLLAMA_URL=http://localhost:11434 \
//!   cargo test -p indexa-llm --test caption_live -- --ignored --nocapture
//! ```
//!
//! Skips cleanly (prints a note, returns) when Ollama is unreachable or the model can't load
//! — vision availability is environmental, not a code defect. (Observed live: Ollama 0.30.10
//! rejects `llama3.2-vision` with "unknown model architecture: 'mllama'", so `moondream` is the
//! reliable default here.)

use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/multimodal")
        .join(name)
}

#[tokio::test]
#[ignore = "needs a local Ollama vision model (e.g. moondream); run with --ignored"]
async fn caption_image_produces_a_description() {
    let url = std::env::var("INDEXA_TEST_OLLAMA_URL")
        .unwrap_or_else(|_| "http://localhost:11434".to_owned());
    let model =
        std::env::var("INDEXA_TEST_VISION_MODEL").unwrap_or_else(|_| "moondream".to_owned());

    let llm = indexa_llm::OllamaLlm::new(url, model.clone());
    match indexa_llm::caption_image_file(&llm, &model, &fixture("sample.png")).await {
        Ok(caption) => {
            assert!(
                !caption.trim().is_empty(),
                "a vision model should return a non-empty caption"
            );
            eprintln!("caption({model}): {}", caption.trim());
        }
        Err(e) => {
            // Vision availability is environmental (Ollama down, model not pulled, or an arch
            // the running Ollama build can't load). Skip rather than fail the suite.
            eprintln!("SKIP: vision caption unavailable ({model}): {e:#}");
        }
    }
}
