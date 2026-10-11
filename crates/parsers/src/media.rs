//! Audio/video metadata parser via ffprobe subprocess.
//! Default: duration, codec, title/artist tags from metadata.
//! Whisper transcription (audio files, and the audio track of video files) is opt-in via
//! config; the deep loops call [`transcribe_audio`] / [`transcribe_video_audio`].

use crate::types::{Chunk, Extracted, Parser};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

pub struct MediaParser;

impl Parser for MediaParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some(
                "mp3"
                    | "mp4"
                    | "m4a"
                    | "m4v"
                    | "aac"
                    | "flac"
                    | "wav"
                    | "ogg"
                    | "opus"
                    | "mkv"
                    | "avi"
                    | "mov"
                    | "webm"
                    | "wmv"
                    | "aiff"
                    | "alac"
                    | "m4b"
            )
        )
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        mime.starts_with("audio/") || mime.starts_with("video/")
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[
            ("mp3", Metadata),
            ("mp4", Metadata),
            ("m4a", Metadata),
            ("flac", Metadata),
            ("wav", Metadata),
            ("ogg", Metadata),
            ("opus", Metadata),
            ("mkv", Metadata),
            ("avi", Metadata),
            ("mov", Metadata),
            ("webm", Metadata),
            ("aiff", Metadata),
        ]
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let text = match run_ffprobe(path) {
            Ok(t) if !t.is_empty() => t,
            _ => format!(
                "Media file: {}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
            ),
        };

        let mime = if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("mp3" | "m4a" | "m4b" | "aac" | "flac" | "wav" | "ogg" | "opus" | "aiff" | "alac")
        ) {
            // `m4b` (audiobooks) + `alac` are audio, not video — so they reach the
            // transcription gate (which keys on an "audio/" mime).
            "audio/mpeg"
        } else {
            "video/mp4"
        };

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: mime.to_owned(),
            chunks: vec![Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text,
                language: None,
            }],
            edges: Vec::new(),
        })
    }
}

/// Transcribe an audio file by shelling out to a whisper.cpp-style CLI (e.g. `whisper-cli`),
/// mirroring [`run_ffprobe`]'s subprocess pattern. Runs `<binary> [-m <model>] -f <path> -nt
/// -np` and returns stdout (the transcript, no timestamps). The binary must accept the input
/// format — whisper.cpp expects 16 kHz WAV. This is a **blocking** subprocess (callers in an
/// async context must wrap it in `spawn_blocking`); transcription can take minutes. Errors
/// (binary absent, non-zero exit, empty output) propagate so the caller can warn and skip,
/// leaving the file's metadata chunk intact.
pub fn transcribe_audio(path: &Path, binary: &str, model: Option<&str>) -> Result<String> {
    run_whisper(path, binary, model, false)
}

/// Like [`transcribe_audio`], but keeps whisper's segment timestamps: one
/// `[HH:MM:SS.mmm --> HH:MM:SS.mmm] text` line per segment, so a search hit in a long
/// recording says *where* the words were spoken. Same blocking/error contract.
pub fn transcribe_audio_timestamped(
    path: &Path,
    binary: &str,
    model: Option<&str>,
) -> Result<String> {
    run_whisper(path, binary, model, true)
}

fn run_whisper(path: &Path, binary: &str, model: Option<&str>, timestamps: bool) -> Result<String> {
    let path_str = path.to_str().context("non-UTF-8 audio path")?;
    let mut cmd = Command::new(binary);
    if let Some(m) = model {
        cmd.args(["-m", m]);
    }
    cmd.args(["-f", path_str]);
    if !timestamps {
        cmd.arg("-nt");
    }
    cmd.arg("-np");
    let output = crate::proc::run_capped(cmd, crate::proc::WHISPER_TIMEOUT)
        .with_context(|| format!("running {binary} (is it installed and on PATH?)"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{binary} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let text = if timestamps {
        tidy_timestamped(&raw)
    } else {
        raw.trim().to_owned()
    };
    if text.is_empty() {
        anyhow::bail!("{binary} produced no transcript");
    }
    Ok(text)
}

/// Normalise whisper's timestamped stdout: drop blank lines and collapse the padding
/// between the `[.. --> ..]` stamp and the segment text to a single space.
fn tidy_timestamped(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| match l.split_once(']') {
            Some((stamp, rest)) if l.starts_with('[') && stamp.contains("-->") => {
                format!("{stamp}] {}", rest.trim())
            }
            _ => l.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract a video's first audio track to a 16 kHz mono WAV (the input whisper.cpp
/// expects) in a temp dir, by shelling out to ffmpeg like [`extract_video_frames`].
/// Returns `Ok(None)` when the video has **no audio track** (a silent screen recording is
/// normal, not an error), otherwise the temp dir — keep it alive until the WAV is consumed —
/// and the WAV path. Blocking subprocess.
pub fn extract_audio_track(
    path: &Path,
    ffmpeg_binary: &str,
) -> Result<Option<(TempDir, std::path::PathBuf)>> {
    let dir = tempfile::tempdir().context("creating temp dir for the audio track")?;
    let wav = dir.path().join("audio.wav");
    let mut cmd = Command::new(ffmpeg_binary);
    cmd.args([
        "-nostdin",
        "-v",
        "error",
        "-i",
        path.to_str().context("non-UTF-8 video path")?,
        "-map",
        "0:a:0?",
        "-vn",
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_s16le",
        wav.to_str().context("non-UTF-8 temp dir")?,
        "-y",
    ]);
    let output = crate::proc::run_capped(cmd, crate::proc::FFMPEG_AUDIO_TIMEOUT)
        .with_context(|| format!("running {ffmpeg_binary} (is ffmpeg installed and on PATH?)"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        // `-map 0:a:0?` makes the audio optional, so a silent video leaves the output with
        // no streams at all — ffmpeg reports that rather than a decode failure.
        if stderr.contains("does not contain any stream") {
            return Ok(None);
        }
        anyhow::bail!(
            "{ffmpeg_binary} audio extraction failed ({}): {}",
            output.status,
            stderr.trim()
        );
    }
    let has_audio = std::fs::metadata(&wav)
        .map(|m| m.len() > WAV_HEADER_BYTES)
        .unwrap_or(false);
    Ok(has_audio.then_some((dir, wav)))
}

/// A canonical PCM WAV header; a file no larger than this holds no samples.
const WAV_HEADER_BYTES: u64 = 44;

/// Transcribe a video's spoken audio: [`extract_audio_track`] with `ffmpeg_binary`, then
/// [`transcribe_audio_timestamped`] on the extracted WAV. `Ok(None)` when the video has no
/// audio track. Blocking (ffmpeg + whisper) — wrap in `spawn_blocking` from async code.
pub fn transcribe_video_audio(
    path: &Path,
    ffmpeg_binary: &str,
    whisper_binary: &str,
    model: Option<&str>,
) -> Result<Option<String>> {
    let Some((_dir, wav)) = extract_audio_track(path, ffmpeg_binary)? else {
        return Ok(None);
    };
    transcribe_audio_timestamped(&wav, whisper_binary, model).map(Some)
}

fn run_ffprobe(path: &Path) -> Result<String> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-show_format",
        path.to_str().context("non-UTF-8 path")?,
    ]);
    let output = crate::proc::run_capped(cmd, crate::proc::FFPROBE_TIMEOUT)
        .context("running ffprobe (is it installed?)")?;

    if !output.status.success() {
        anyhow::bail!("ffprobe exited with {}", output.status);
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let fmt = &json["format"];

    let mut parts = Vec::new();

    if let Some(name) = fmt["filename"].as_str() {
        if let Some(base) = std::path::Path::new(name)
            .file_name()
            .and_then(|n| n.to_str())
        {
            parts.push(format!("File: {base}"));
        }
    }

    if let Some(dur) = fmt["duration"].as_str() {
        if let Ok(secs) = dur.parse::<f64>() {
            let mins = (secs / 60.0) as u64;
            let s = secs as u64 % 60;
            parts.push(format!("Duration: {mins}m {s}s"));
        }
    }

    if let Some(bit) = fmt["bit_rate"].as_str() {
        if let Ok(bps) = bit.parse::<u64>() {
            parts.push(format!("Bitrate: {} kbps", bps / 1000));
        }
    }

    if let Some(tags) = fmt["tags"].as_object() {
        let interesting = [
            "title",
            "artist",
            "album",
            "genre",
            "date",
            "comment",
            "description",
        ];
        for key in &interesting {
            if let Some(val) = tags.get(*key).and_then(|v| v.as_str()) {
                if !val.is_empty() {
                    parts.push(format!("{}: {val}", capitalize(key)));
                }
            }
        }
    }

    Ok(parts.join(", "))
}

/// Extract frames from a video by shelling out to ffmpeg.
/// Returns a list of `(temp_dir, jpg_paths)` so the caller can caption them.
/// The returned `TempDir` must be kept alive until frames are consumed.
///
/// `fps_sample`: frames per second to extract (e.g. 0.5 = one every 2 s).
/// `max_frames`: hard cap on extracted frame count.
pub fn extract_video_frames(
    path: &Path,
    ffmpeg_binary: &str,
    fps_sample: f32,
    max_frames: usize,
) -> Result<(TempDir, Vec<std::path::PathBuf>)> {
    let dir = tempfile::tempdir().context("creating temp dir for video frames")?;
    let pattern = dir.path().join("frame_%03d.jpg");
    let mut cmd = Command::new(ffmpeg_binary);
    cmd.args([
        "-nostdin",
        "-i",
        path.to_str().context("non-UTF-8 video path")?,
        "-vf",
        &format!("fps={fps_sample}"),
        "-frames:v",
        &max_frames.to_string(),
        "-q:v",
        "2",
        pattern.to_str().context("non-UTF-8 temp dir")?,
        "-y",
    ]);
    let output = crate::proc::run_capped(cmd, crate::proc::FFMPEG_TIMEOUT)
        .with_context(|| format!("running {ffmpeg_binary} (is ffmpeg installed and on PATH?)"))?;

    if !output.status.success() {
        anyhow::bail!(
            "{ffmpeg_binary} frame extraction failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // Collect the extracted frame files in order.
    let mut frames: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .context("reading frame temp dir")?
        .filter_map(|e| {
            let e = e.ok()?;
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("jpg") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    frames.sort(); // frame_001.jpg < frame_002.jpg etc.
    Ok((dir, frames))
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_parser_accepts_known_extensions() {
        let p = MediaParser;
        assert!(p.accepts_path(Path::new("song.mp3")));
        assert!(p.accepts_path(Path::new("video.mp4")));
        assert!(p.accepts_path(Path::new("audio.flac")));
        assert!(!p.accepts_path(Path::new("doc.pdf")));
    }

    #[test]
    fn transcribe_audio_errors_gracefully_when_binary_missing() {
        // A missing transcription binary must return Err (the deep loop warns + skips,
        // keeping the metadata chunk) — never panic.
        let res = transcribe_audio(
            Path::new("/tmp/nonexistent.wav"),
            "indexa-no-such-whisper-binary",
            None,
        );
        assert!(
            res.is_err(),
            "missing binary must Err, not panic or succeed"
        );
    }

    #[test]
    fn transcribe_video_audio_errors_gracefully_when_ffmpeg_missing() {
        let res = transcribe_video_audio(
            Path::new("/tmp/nonexistent.mp4"),
            "indexa-no-such-ffmpeg-binary",
            "indexa-no-such-whisper-binary",
            None,
        );
        assert!(
            res.is_err(),
            "missing ffmpeg must Err, not panic or succeed"
        );
    }

    #[test]
    fn tidy_timestamped_collapses_padding_and_blank_lines() {
        let raw = "\n[00:00:00.000 --> 00:00:03.120]   Indexa makes local context.\n\n\
                   [00:00:03.120 --> 00:00:05.000]  Second line.\n";
        assert_eq!(
            tidy_timestamped(raw),
            "[00:00:00.000 --> 00:00:03.120] Indexa makes local context.\n\
             [00:00:03.120 --> 00:00:05.000] Second line."
        );
    }
}
