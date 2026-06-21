use anyhow::Result;
use indexa_core::config::Config;
use indexa_core::store::Store;

use super::cmd_ask;
use super::helpers::require_index_db;

/// Save (or overwrite) a named query.
pub(crate) async fn cmd_saved_add(
    name: String,
    question: String,
    mode: String,
    scope: Option<String>,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    store.save_query(&name, &question, &mode, scope.as_deref())?;
    println!("Saved query \"{name}\" (mode: {mode}).");
    Ok(())
}

/// List saved queries.
pub(crate) async fn cmd_saved_list(json: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let rows = store.list_saved_queries()?;
    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|q| {
                serde_json::json!({ "name": q.name, "question": q.question, "mode": q.mode, "scope": q.scope })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No saved queries. Add one with `indexa saved add <name> \"<question>\"`.");
        return Ok(());
    }
    println!("Saved queries ({}):", rows.len());
    for q in &rows {
        let scope = q
            .scope
            .as_deref()
            .map(|s| format!(" [scope: {s}]"))
            .unwrap_or_default();
        println!("  {} ({}){}\n    {}", q.name, q.mode, scope, q.question);
    }
    Ok(())
}

/// Run a saved query through the normal `ask` pipeline.
pub(crate) async fn cmd_saved_run(name: String, json: bool, cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let Some(q) = store.get_saved_query(&name)? else {
        anyhow::bail!("no saved query named \"{name}\". List them with `indexa saved list`.");
    };
    drop(store);
    // Map the stored mode onto the ask flags; everything else takes config defaults.
    let (sparse_only, dense_only, agentic) = match q.mode.as_str() {
        "sparse" => (true, false, false),
        "dense" => (false, true, false),
        "agentic" => (false, false, true),
        _ => (false, false, false), // rrf / default
    };
    cmd_ask(
        q.question,
        None,
        None,
        q.scope,
        None,
        sparse_only,
        dense_only,
        agentic,
        None,
        false,
        None,  // session_id: saved queries are stateless
        false, // continue_
        json,
        false, // no_synthesize: saved queries synthesize normally
        cfg,
    )
    .await
}

/// Delete a saved query.
pub(crate) async fn cmd_saved_rm(name: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    if store.delete_saved_query(&name)? == 0 {
        anyhow::bail!("no saved query named \"{name}\".");
    }
    println!("Deleted saved query \"{name}\".");
    Ok(())
}
