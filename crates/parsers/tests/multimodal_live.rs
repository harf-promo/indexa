//! Happy-path, end-to-end multimodal tests that exercise the REAL external tools
//! (tesseract, whisper-cli, ffmpeg) on committed fixtures. The rest of the suite only
//! covers graceful degradation when those tools are absent; these prove the integrations
//! actually produce text/frames when the tools ARE present.
//!
//! They are `#[ignore]`d so plain `cargo test` (and CI without the tools) stays green —
//! run them explicitly with the tools installed:
//!
//! ```bash
//! brew install tesseract ffmpeg poppler whisper-cpp
//! # for the audio test, point at a whisper.cpp ggml model:
//! export INDEXA_TEST_WHISPER_MODEL=/path/to/ggml-base.en.bin
//! cargo test -p indexa-parsers --test multimodal_live -- --ignored --nocapture
//! ```
//!
//! Each test skips cleanly (returns early with a printed note) when its tool/model is
//! unavailable, so running `--ignored` on a partial setup never spuriously fails.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve a committed fixture under the workspace `fixtures/multimodal/` dir.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/multimodal")
        .join(name)
}

/// Whether an external binary is on PATH (so a missing tool skips, not fails).
fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty())
        .unwrap_or(false)
}

#[test]
#[ignore = "needs tesseract + poppler (pdftoppm); run with --ignored"]
fn ocr_extracts_text_from_an_image_only_pdf() {
    if !have("tesseract") || !have("pdftoppm") {
        eprintln!("SKIP: tesseract/pdftoppm not installed");
        return;
    }
    let pdf = fixture("scanned.pdf");
    let text = indexa_parsers::pdf::ocr_pdf(&pdf, "tesseract", None)
        .expect("OCR over an image-only PDF should succeed when the tools are present");
    let lc = text.to_lowercase();
    assert!(
        lc.contains("indexa") && lc.contains("quick brown fox"),
        "OCR text should contain the rendered words, got: {text:?}"
    );
}

#[test]
#[ignore = "needs whisper-cli + a ggml model in INDEXA_TEST_WHISPER_MODEL; run with --ignored"]
fn transcribe_recovers_spoken_words_from_wav() {
    if !have("whisper-cli") {
        eprintln!("SKIP: whisper-cli not installed");
        return;
    }
    let Ok(model) = std::env::var("INDEXA_TEST_WHISPER_MODEL") else {
        eprintln!("SKIP: set INDEXA_TEST_WHISPER_MODEL=/path/to/ggml-*.bin to run this");
        return;
    };
    let wav = fixture("speech.wav");
    let text = indexa_parsers::media::transcribe_audio(&wav, "whisper-cli", Some(&model))
        .expect("transcription should succeed with whisper-cli + a model");
    let lc = text.to_lowercase();
    assert!(
        lc.contains("indexa") && lc.contains("local context"),
        "transcript should contain the spoken phrase, got: {text:?}"
    );
}

#[test]
#[ignore = "needs ffmpeg; run with --ignored"]
fn extract_video_frames_produces_jpegs() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let mp4 = fixture("clip.mp4");
    let (_keep, frames) = indexa_parsers::media::extract_video_frames(&mp4, "ffmpeg", 0.5, 8)
        .expect("frame extraction should succeed with ffmpeg present");
    assert!(
        !frames.is_empty(),
        "at least one frame should be extracted from the clip"
    );
    for f in &frames {
        assert!(f.exists(), "extracted frame {f:?} should exist on disk");
        assert!(
            std::fs::metadata(f).map(|m| m.len() > 0).unwrap_or(false),
            "extracted frame {f:?} should be non-empty"
        );
    }
}

#[test]
#[ignore = "needs ffmpeg; run with --ignored"]
fn extract_audio_track_yields_wav_and_skips_silent_video() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let (_keep, wav) =
        indexa_parsers::media::extract_audio_track(&fixture("speech_clip.mp4"), "ffmpeg")
            .expect("audio extraction should succeed with ffmpeg present")
            .expect("speech_clip.mp4 has an audio track");
    assert!(
        std::fs::metadata(&wav)
            .map(|m| m.len() > 44)
            .unwrap_or(false),
        "extracted WAV {wav:?} should hold samples"
    );
    // clip.mp4 is video-only: no audio track is a quiet `None`, not an error.
    let silent = indexa_parsers::media::extract_audio_track(&fixture("clip.mp4"), "ffmpeg")
        .expect("a video without audio must not Err");
    assert!(
        silent.is_none(),
        "video-only clip should yield no audio track"
    );
}

#[test]
#[ignore = "needs ffmpeg + whisper-cli + a ggml model in INDEXA_TEST_WHISPER_MODEL; run with --ignored"]
fn transcribe_video_audio_recovers_spoken_words_with_timestamps() {
    if !have("ffmpeg") || !have("whisper-cli") {
        eprintln!("SKIP: ffmpeg/whisper-cli not installed");
        return;
    }
    let Ok(model) = std::env::var("INDEXA_TEST_WHISPER_MODEL") else {
        eprintln!("SKIP: set INDEXA_TEST_WHISPER_MODEL=/path/to/ggml-*.bin to run this");
        return;
    };
    let text = indexa_parsers::media::transcribe_video_audio(
        &fixture("speech_clip.mp4"),
        "ffmpeg",
        "whisper-cli",
        Some(&model),
    )
    .expect("video transcription should succeed with ffmpeg + whisper-cli + a model")
    .expect("speech_clip.mp4 has an audio track");
    let lc = text.to_lowercase();
    assert!(
        lc.contains("indexa") && lc.contains("local context"),
        "transcript should contain the spoken phrase, got: {text:?}"
    );
    assert!(
        text.starts_with("[00:00:00.000 -->"),
        "video transcript should keep segment timestamps, got: {text:?}"
    );
}
