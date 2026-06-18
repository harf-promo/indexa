//! MCP Prompts: reusable, index-backed prompt templates a client can offer the user.
//! Each is a thin wrapper that seeds a message with data the tools already expose
//! (project overview, a file summary, a Context Pack), with secrets redacted.
//!
//! Catalog:
//! - `onboarding-overview` (no args) — a guided tour seeded with the project roll-up
//! - `explain-file` {path}          — explain a file, seeded with its indexed summary
//! - `pack-context` {name}          — work against a Context Pack's bundled context
//!
//! Inherent + pure (no `RequestContext`) so they unit-test directly; the `ServerHandler`
//! glue in `lib.rs` calls them. Missing data fails open to an explanatory message; a
//! missing required argument is an `invalid_params` error.

use rmcp::model::{
    GetPromptResult, JsonObject, Prompt, PromptArgument, PromptMessage, PromptMessageRole,
};
use rmcp::ErrorData;

use indexa_query::redact::redact_secrets;

use crate::packs::export_pack_body;
use crate::{mcp_err, IndexaMcp};

const OVERVIEW_BUDGET: usize = 4000;

impl IndexaMcp {
    /// The prompt catalog (`prompts/list`). Names are the golden contract (see
    /// `golden_prompts.txt`).
    pub(crate) fn list_prompts_inner(&self) -> Vec<Prompt> {
        vec![
            Prompt::new(
                "onboarding-overview",
                Some("Give a guided tour of this project, seeded with its indexed overview."),
                None,
            ),
            Prompt::new(
                "explain-file",
                Some("Explain a specific file, seeded with its indexed summary."),
                Some(vec![PromptArgument::new("path")
                    .with_description("Absolute path of the indexed file to explain.")
                    .with_required(true)]),
            ),
            Prompt::new(
                "pack-context",
                Some("Work against a Context Pack, seeded with its bundled context."),
                Some(vec![PromptArgument::new("name")
                    .with_description("Name of the Context Pack to load.")
                    .with_required(true)]),
            ),
        ]
    }

    /// Resolve a prompt by name + arguments (`prompts/get`).
    pub(crate) fn get_prompt_inner(
        &self,
        name: &str,
        args: Option<&JsonObject>,
    ) -> Result<GetPromptResult, ErrorData> {
        match name {
            "onboarding-overview" => {
                let store = self.store()?;
                let root = store.root_paths().ok().and_then(|r| r.into_iter().next());
                let overview = indexa_query::build_project_overview(
                    &store,
                    &[],
                    root.as_deref(),
                    OVERVIEW_BUDGET,
                );
                let body = if overview.trim().is_empty() {
                    "There is no project overview yet — ask me to run `indexa summarize` first."
                        .to_owned()
                } else {
                    format!(
                        "Here is an overview of this project (from its local index):\n\n{overview}\n\n\
                         Give me a concise guided tour: what this project is, how it's organized, \
                         and where I'd start reading."
                    )
                };
                Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                    PromptMessageRole::User,
                    body,
                )])
                .with_description("Guided project tour"))
            }
            "explain-file" => {
                let path = required_arg(args, "path")?;
                let store = self.store()?;
                let body = match store.summary_by_path(&path).map_err(mcp_err)? {
                    Some(rec) => {
                        let (summary, _n) = redact_secrets(&rec.summary);
                        format!(
                            "Explain the file `{path}`. Here is its indexed summary:\n\n{summary}\n\n\
                             Walk me through what it does, its key responsibilities, and anything \
                             notable. Use `read_file`/`get_chunk_context` if you need detail."
                        )
                    }
                    None => format!(
                        "There is no indexed summary for `{path}`. Ask me to run \
                         `indexa index {path}` first, or use `read_file` to read it raw."
                    ),
                };
                Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                    PromptMessageRole::User,
                    body,
                )])
                .with_description("Explain an indexed file"))
            }
            "pack-context" => {
                let name = required_arg(args, "name")?;
                let store = self.store()?;
                // export_pack_body redacts; on a missing/empty pack it returns an error we surface
                // as an explanatory message rather than failing the prompt fetch.
                let body = match export_pack_body(&store, &name, "md", None, false) {
                    Ok(pack) => format!(
                        "Here is the Context Pack `{name}` (a curated bundle from the local index):\n\n\
                         {pack}\n\nUse this as the working context for what I ask next."
                    ),
                    Err(_) => format!(
                        "The Context Pack `{name}` is empty or doesn't exist. Ask me to create it \
                         with `indexa pack create \"{name}\"` and add paths."
                    ),
                };
                Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                    PromptMessageRole::User,
                    body,
                )])
                .with_description("Load a Context Pack as working context"))
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown prompt: {other}"),
                None,
            )),
        }
    }
}

/// Extract a required string argument, or an `invalid_params` error.
fn required_arg(args: Option<&JsonObject>, key: &str) -> Result<String, ErrorData> {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ErrorData::invalid_params(format!("missing required argument: {key}"), None))
}
