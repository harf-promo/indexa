use axum::{extract::State, Json};

use crate::dto::ProviderStatus;
use crate::AppState;

/// `GET /api/providers/status` — report the active describer provider plus the
/// state of the Claude subscription backend (CLI present? logged in? which plan?).
///
/// Backed by [`indexa_llm::claude_status`], which runs only token-free local
/// probes (`claude --version`, `claude auth status --json`) — no model is
/// invoked, so this is safe to hit on every Settings load.
pub(crate) async fn api_providers_status(State(state): State<AppState>) -> Json<ProviderStatus> {
    let describer = &state.config.describer;
    let claude = indexa_llm::claude_status(&describer.claude_bin).await;
    Json(ProviderStatus {
        describer_provider: describer.provider.clone(),
        claude_cli_present: claude.cli_present,
        claude_cli_version: claude.cli_version,
        claude_logged_in: claude.logged_in,
        claude_auth_method: claude.auth_method,
        claude_subscription: claude.subscription_type,
    })
}
