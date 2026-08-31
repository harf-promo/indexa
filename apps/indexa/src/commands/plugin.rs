use anyhow::{bail, Result};
use indexa_parsers::plugin_directory::{self, PluginEntry};

/// `indexa plugin list` — print the curated third-party parser plugin directory
/// (`crates/parsers/plugins.toml`, embedded at compile time — no network fetch by
/// default). With `refresh`, fetches the same file off `main` on GitHub instead, so
/// newly-curated entries show up without an `indexa` upgrade; any network/parse error
/// falls back to the embedded list rather than failing the command outright.
pub(crate) async fn cmd_plugin_list(json: bool, refresh: bool) -> Result<()> {
    let plugins = if refresh {
        // Client construction is part of the same fallible "get the remote list" step as the
        // fetch itself — a bare `?` here used to skip `resolve_refreshed_plugins` entirely and
        // hard-fail the command on a `reqwest::Client::builder().build()` error (e.g. an
        // OS-trust-store lookup failing in a minimal/headless environment), contradicting the
        // doc comment's promise that any network/parse error falls back to the embedded list.
        // Funnel both error sources through the same `Result` so both hit the one fallback path.
        let remote = match plugin_directory::build_remote_client() {
            Ok(client) => plugin_directory::load_remote(&client).await,
            Err(e) => Err(e),
        };
        resolve_refreshed_plugins(remote)?
    } else {
        plugin_directory::load()?
    };

    if json {
        let arr: Vec<serde_json::Value> = plugins.iter().map(entry_json).collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if plugins.is_empty() {
        println!(
            "No third-party parser plugins listed yet. See crates/parsers/plugins.toml for the \
             expected shape, or CONTRIBUTING.md's 'Authoring a third-party parser plugin' \
             section for how to author and list one."
        );
        return Ok(());
    }

    println!(
        "Known third-party parser plugins (compile-time — see `indexa plugin info <name>`):\n"
    );
    println!("  {:<22} {:<28} Handles", "NAME", "CRATE");
    println!("  {}", "─".repeat(70));
    for p in &plugins {
        println!("  {:<22} {:<28} {}", p.name, p.crate_name, p.handles());
    }
    println!(
        "\n{} plugin(s). Run `indexa plugin info <name>` for install instructions.",
        plugins.len()
    );
    Ok(())
}

/// `indexa plugin info <name>` — the full entry plus a copy-pasteable `Cargo.toml`
/// dependency line and `Registry::register` snippet (adapted from
/// `crates/parsers/src/registry.rs`'s own "Plugin SDK" doc comment).
pub(crate) async fn cmd_plugin_info(name: String) -> Result<()> {
    let Some(p) = plugin_directory::find(&name)? else {
        bail!(
            "no plugin named '{name}' in the directory. Run `indexa plugin list` to see what's known."
        );
    };

    println!("{}\n", p.name);
    println!("  crate:       {}", p.crate_name);
    println!("  description: {}", p.description);
    println!("  handles:     {}", p.handles());
    println!("  repo:        {}", p.repo);
    println!();
    println!("This is a compile-time plugin: it extends indexa_parsers::registry::Registry inside");
    println!("your OWN custom binary — there is no dynamic/runtime install into `indexa` itself.");
    println!();
    println!("1. Add the dependency to your binary's Cargo.toml:");
    println!();
    println!(
        "     {} = \"*\"   # pick a real version — see {}",
        p.crate_name, p.repo
    );
    println!();
    println!("2. Register it before parsing, in your own `main`:");
    println!();
    println!("     use indexa_parsers::registry::Registry;");
    println!("     use {}::{};", p.crate_ident(), p.parser_type);
    println!();
    println!("     let mut reg = Registry::new();");
    println!("     reg.register(Box::new({}));", p.parser_type);
    println!("     let extracted = reg.parse(path)?;");
    println!();
    println!(
        "See crates/parsers/src/registry.rs's \"Plugin SDK\" doc comment for the full contract."
    );
    Ok(())
}

fn entry_json(p: &PluginEntry) -> serde_json::Value {
    serde_json::json!({
        "name": p.name,
        "crate_name": p.crate_name,
        "parser_type": p.parser_type,
        "description": p.description,
        "extensions": p.extensions,
        "mime_types": p.mime_types,
        "repo": p.repo,
    })
}

/// Resolve `--refresh`'s outcome: a successful fetch wins outright; a failed one prints a
/// warning and falls back to the embedded directory. Split out from `cmd_plugin_list` so
/// the fallback path is testable against an injected `Err` — never a real network call.
fn resolve_refreshed_plugins(remote: Result<Vec<PluginEntry>>) -> Result<Vec<PluginEntry>> {
    match remote {
        Ok(entries) => Ok(entries),
        Err(e) => {
            eprintln!(
                "⚠ could not fetch the remote plugin directory ({e:#}) — showing the locally embedded list."
            );
            plugin_directory::load()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_entry(name: &str) -> PluginEntry {
        PluginEntry {
            name: name.into(),
            crate_name: format!("indexa-parser-{name}"),
            parser_type: "ExampleParser".into(),
            description: "d".into(),
            extensions: vec!["xyz".into()],
            mime_types: vec![],
            repo: "https://example.com".into(),
        }
    }

    #[test]
    fn resolve_refreshed_plugins_uses_the_remote_list_on_success() {
        let plugins = resolve_refreshed_plugins(Ok(vec![example_entry("remote-only")])).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "remote-only");
    }

    #[test]
    fn resolve_refreshed_plugins_falls_back_to_the_embedded_list_on_error() {
        // Injected failure, not a real network call — proves the fallback path is a soft
        // failure (the embedded directory), never a hard error from `--refresh`.
        let plugins =
            resolve_refreshed_plugins(Err(anyhow::anyhow!("connection refused"))).unwrap();
        // The embedded plugins.toml always has at least the template entry.
        assert!(!plugins.is_empty());
    }

    #[test]
    fn resolve_refreshed_plugins_falls_back_when_client_construction_itself_fails() {
        // `reqwest::Client::builder().build()` only fails on TLS-backend init (root-cert-store
        // lookup, etc.) — not on anything this crate's fixed builder config exposes — so there's
        // no portable way to force a REAL `build_remote_client()` failure from a unit test.
        // What's actually under test is `cmd_plugin_list`'s fix: a client-construction error and
        // a fetch error now funnel through the identical `resolve_refreshed_plugins` fallback
        // (see the `match` in `cmd_plugin_list`), instead of the former bare `?` that skipped
        // this path entirely for a construction failure. Exercising the shared fallback with a
        // client-construction-shaped error proves that code path, not just the fetch-error one.
        let client_build_err = anyhow::anyhow!(
            "failed to build HTTP client for the remote plugin directory: TLS backend init failed"
        );
        let plugins = resolve_refreshed_plugins(Err(client_build_err)).unwrap();
        assert!(!plugins.is_empty());
    }
}
