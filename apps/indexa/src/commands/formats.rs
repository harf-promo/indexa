use anyhow::Result;
use indexa_parsers::registry::Registry;

/// `indexa formats` — list the file formats Indexa understands and at what level, so the
/// "understands every file" claim is queryable instead of asserted.
pub(crate) async fn cmd_formats(json: bool, level: Option<String>) -> Result<()> {
    let mut formats = Registry::new().supported_formats();
    if let Some(l) = &level {
        let l = l.to_ascii_lowercase();
        formats.retain(|f| f.support_level == l);
    }

    if json {
        let arr: Vec<serde_json::Value> = formats
            .iter()
            .map(|f| {
                serde_json::json!({
                    "extension": f.extension,
                    "support_level": f.support_level,
                    "mime": f.mime,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    println!("Indexa understands these file formats:\n");
    println!("  {:<14} {:<13} MIME", "EXTENSION", "SUPPORT");
    println!("  {}", "─".repeat(58));
    for f in &formats {
        let ext = if f.extension.starts_with('(') {
            f.extension.clone()
        } else {
            format!(".{}", f.extension)
        };
        println!(
            "  {:<14} {:<13} {}",
            ext,
            f.support_level,
            f.mime.as_deref().unwrap_or("—")
        );
    }
    println!(
        "\n{} formats. full = text extracted · metadata = listing/EXIF only · \
         stub = recognised, not extracted · textfallback = sniffed as text.",
        formats.len()
    );
    Ok(())
}
