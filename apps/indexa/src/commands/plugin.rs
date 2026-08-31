use anyhow::{bail, Result};
use indexa_parsers::plugin_directory::{self, PluginEntry};

/// `indexa plugin list` — print the curated third-party parser plugin directory
/// (`crates/parsers/plugins.toml`, embedded at compile time — no network fetch).
pub(crate) async fn cmd_plugin_list(json: bool) -> Result<()> {
    let plugins = plugin_directory::load()?;

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
