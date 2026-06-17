//! Memory-pressure watchdog for background jobs: samples RAM/swap between heavy
//! steps and pauses (or resumes) summarization/deep work so a job can't OOM the box.
//! Extracted from `jobs_exec` (v0.61) — pure move, no behavior change.

use crate::jobs::{push, JobEvent, JobHandle, PressureInfo};
use indexa_core::resource::{
    assess, pause_step, MachineSpec, PauseAction, Pressure, WatchdogState, MAX_PAUSE_SECS,
};
use indexa_embed::Embedder;
use indexa_llm::Describer;
use std::sync::Arc;

/// Compute the swap-used percentage of total swap (0–100), saturating to 100 on no swap.
fn swap_pct(sample: &indexa_core::resource::MemSample) -> u64 {
    sample
        .swap_used_bytes
        .checked_mul(100)
        .and_then(|v| v.checked_div(sample.swap_total_bytes))
        .unwrap_or(100)
}

/// Check memory pressure before an Ollama call.
///
/// If pressure is Throttle or Critical:
///   1. Emits a calm, actionable Warning event so the user can see it in the Jobs UI.
///   2. On a **Critical** entry, unloads the resident model(s) once so their wired RAM
///      frees — that is what lets the recovery check below trigger (macOS swap is sticky
///      and never drains on its own, so we cannot wait for swap to fall).
///   3. Loops on the recover-aware [`pause_step`] predicate: resumes the moment free RAM
///      climbs back above headroom (`compute_budget > 0`) — even while swap stays high —
///      or the entry signal clears; otherwise sleeps on the 5 s/2 s cadence up to
///      [`MAX_PAUSE_SECS`], then proceeds.
///
/// `embedder` / `llm` are the handles whose models to unload on a Critical pause; the deep
/// loop always passes the embedder (and the contextual-retrieval LLM when present), and the
/// summarize loop passes its describer + embedder.
///
/// The caller should invoke this before every embedding or LLM call
/// in the hot loops of `run_deep_phase` and `run_summarize_phase`.
pub(crate) async fn run_watchdog_check(
    wdog: &mut WatchdogState,
    spec: &MachineSpec,
    headroom: u64,
    handle: &Arc<JobHandle>,
    stage: &str,
    embedder: Option<&dyn Embedder>,
    // Unload target on a Critical pause. Typed `Describer` (not `Generator`): both the
    // summarize describer and the deep-phase context LLM implement Describer, and only
    // `unload()` — shared by both traits — is used here.
    llm: Option<&(dyn Describer + Send + Sync)>,
) {
    let sample = wdog.sample();
    // Gate entry on the SAME recover-aware predicate as resume, not raw `assess()`. macOS swap
    // is sticky: after the first event `assess()` reports Critical for the rest of the job even
    // once RAM has recovered. Using `assess()` here would re-enter the pause (warn + unload +
    // reload the model) on *every* subsequent file. `pause_step(.., 0) == Resume` means "RAM is
    // fine OR no real signal" → skip. Only when RAM is genuinely low (compute_budget <= 0) do we
    // fall through and pause.
    if pause_step(spec, &sample, headroom, 0) == PauseAction::Resume {
        return;
    }
    // RAM is genuinely low. Use `assess()` only to choose the unload gate (Critical vs Throttle).
    let pressure = assess(&sample, spec, headroom);

    let pct = swap_pct(&sample);
    push(
        handle,
        JobEvent::Warning {
            stage: stage.to_owned(),
            item_path: None,
            message: format!(
                "Low on memory (swap {pct}%). Easing off and freeing the model to keep your \
                 machine responsive — this resumes automatically. \
                 Tip: lower the workload in Settings → Resource Profile."
            ),
            // Structured snapshot so the UI can line the warning up with the live RAM gauge
            // instead of parsing the prose. Every value is already in hand here.
            pressure: Some(PressureInfo {
                level: match pressure {
                    Pressure::Critical => "critical",
                    _ => "throttle",
                }
                .to_owned(),
                swap_percent: pct,
                used_bytes: sample.used_bytes,
                budget_bytes: indexa_core::resource::compute_budget(spec, &sample, headroom),
                headroom_bytes: headroom,
            }),
        },
    );

    // On a Critical entry, unload the resident model(s) once so their wired pages free and
    // `compute_budget` can climb back above 0. macOS swap is sticky and never drains on its
    // own, so gating resume on swap level alone would stall here for the full backstop.
    if pressure == Pressure::Critical {
        if let Some(e) = embedder {
            e.unload().await;
        }
        if let Some(l) = llm {
            l.unload().await;
        }
    }

    // Wait until memory actually recovers, capped at resource::MAX_PAUSE_SECS. `pause_step`
    // re-evaluates a fresh sample each tick: it resumes when free RAM returns above headroom
    // (recovery) regardless of sticky swap, and escalation (Throttle → Critical) tightens the
    // cadence immediately.
    let mut elapsed = 0u64;
    let mut next_status_at = 30u64;
    loop {
        let s = wdog.sample();
        match pause_step(spec, &s, headroom, elapsed) {
            PauseAction::Resume => break,
            PauseAction::Proceed => {
                push(
                    handle,
                    JobEvent::Warning {
                        stage: stage.to_owned(),
                        item_path: None,
                        message: format!(
                            "Memory didn't recover within {MAX_PAUSE_SECS}s — continuing gently. \
                             If this repeats, lower the workload in Settings → Resource Profile."
                        ),
                        pressure: None,
                    },
                );
                break;
            }
            PauseAction::Sleep(secs) => {
                // Emit a calm follow-up roughly every 30 s so the user isn't left wondering.
                if elapsed >= next_status_at {
                    push(
                        handle,
                        JobEvent::Warning {
                            stage: stage.to_owned(),
                            item_path: None,
                            message: format!(
                                "Still easing off while memory recovers … ({elapsed}s)"
                            ),
                            pressure: None,
                        },
                    );
                    next_status_at += 30;
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(secs)).await;
                elapsed += secs;
            }
        }
    }
}
