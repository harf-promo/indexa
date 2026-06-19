//! Detect version skew between the running CLI/MCP binary and the installed
//! Indexa **desktop app**.
//!
//! The desktop app and the standalone CLI are separate artifacts. When the app
//! self-updates, it tries to refresh the CLI in place (see
//! [`crate::download_cli_to`]) — but that refresh is best-effort, so the user's
//! terminal `indexa` (and the MCP server that runs `indexa mcp`) can silently
//! rot behind the app. A stale binary serves stale behavior with no signal.
//!
//! This module gives every surface a cheap, **fail-open** way to detect that
//! skew and tell the user how to fix it. Nothing here ever blocks a command,
//! an answer, or an update: any error (no app installed, unreadable plist,
//! unparseable version) collapses to [`Skew::Unknown`], which surfaces as
//! "nothing to report".

use semver::Version;

/// Filename of the marker the desktop app writes (in the data dir) after a CLI
/// auto-refresh that did NOT land the expected version — and deletes on success.
/// The web `/api/health` handler reads it to surface a "your CLI is stale" banner.
/// Defined here so the writer (desktop) and reader (web) share one source of truth.
pub const CLI_SKEW_MARKER_FILE: &str = "cli_skew_warning.json";

/// Where a skew check is being surfaced — used only to tailor the fix wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// A terminal command (`indexa doctor` / `indexa status`) — fix is `indexa update`.
    Cli,
    /// The MCP server (`get_stats`) — fix is update the CLI, then restart MCP.
    Mcp,
}

/// The relationship between the running binary and the installed desktop app.
///
/// Every variant that carries versions carries **both**, so a message can show
/// `vCLI` and `vAPP` without re-reading `env!("CARGO_PKG_VERSION")`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skew {
    /// Running binary == installed app. Healthy.
    InSync,
    /// Running binary is **older** than the installed app — the harmful case
    /// (stale terminal/MCP behavior the user didn't ask for).
    CliBehind { cli: Version, app: Version },
    /// Running binary is **newer** than the installed app — normal during local
    /// development; informational, never a warning.
    CliAhead { cli: Version, app: Version },
    /// No app found, or a version couldn't be parsed. Say nothing harmful.
    Unknown,
}

/// Pure classifier: the unit-tested heart. No IO.
///
/// `running` is the binary asking; `app` is the installed desktop app version
/// (`None` when no app is installed or its version couldn't be read).
pub fn classify_skew(running: &Version, app: Option<&Version>) -> Skew {
    match app {
        None => Skew::Unknown,
        Some(app) => match running.cmp(app) {
            std::cmp::Ordering::Equal => Skew::InSync,
            std::cmp::Ordering::Less => Skew::CliBehind {
                cli: running.clone(),
                app: app.clone(),
            },
            std::cmp::Ordering::Greater => Skew::CliAhead {
                cli: running.clone(),
                app: app.clone(),
            },
        },
    }
}

impl Skew {
    /// One-line, actionable advice for a **harmful** skew, tailored to the surface.
    ///
    /// Returns `Some` only for [`Skew::CliBehind`]; `InSync` / `CliAhead` /
    /// `Unknown` return `None`, so callers print nothing in those cases.
    pub fn advice(&self, surface: Surface) -> Option<String> {
        match self {
            Skew::CliBehind { cli, app } => Some(match surface {
                Surface::Cli => format!(
                    "Your terminal `indexa` is v{cli} but the installed Indexa app is v{app}. \
                     Run `indexa update` to sync it (or reinstall the CLI from the app's menu: \
                     \"Install command-line tool\"). The MCP server runs this same binary."
                ),
                Surface::Mcp => format!(
                    "This MCP server is v{cli} but the installed Indexa app is v{app} — \
                     you may be getting stale behavior. Update the CLI (`indexa update`) \
                     and restart the MCP server to pick up the new binary."
                ),
            }),
            _ => None,
        }
    }
}

/// Extract `CFBundleShortVersionString` from `Info.plist` XML **without** a plist
/// crate (keeps the dependency tree unchanged).
///
/// Anchors on the *exact* `<key>CFBundleShortVersionString</key>` element, then
/// returns the **next** `<string>…</string>` value. Anchoring on the whole key
/// element is deliberate: the neighbouring `CFBundleInfoDictionaryVersion` (=`6.0`)
/// and `CFBundleVersion` keys also end in `Version`, so a looser "first string
/// after a Version line" scan would grab `6.0`. Returns `None` on any miss
/// (fail-open); semver validation is left to [`detect_skew`].
fn parse_plist_short_version(xml: &str) -> Option<String> {
    let key_pos = xml.find("<key>CFBundleShortVersionString</key>")?;
    let after = &xml[key_pos..];
    let open = after.find("<string>")? + "<string>".len();
    let close_rel = after[open..].find("</string>")?;
    let raw = after[open..open + close_rel].trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

/// Best-effort read of the installed Indexa **desktop app** version.
///
/// macOS only: reads `CFBundleShortVersionString` from the app bundle's
/// `Info.plist`, checking the system Applications folder then the per-user one.
/// Returns `None` on every other OS and on any IO/parse failure (fail-open).
#[cfg(target_os = "macos")]
pub fn installed_app_version() -> Option<String> {
    let mut candidates = vec![std::path::PathBuf::from(
        "/Applications/Indexa.app/Contents/Info.plist",
    )];
    if let Some(home) = std::env::var_os("HOME") {
        candidates
            .push(std::path::Path::new(&home).join("Applications/Indexa.app/Contents/Info.plist"));
    }
    candidates
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|text| parse_plist_short_version(&text))
}

/// Non-macOS: desktop-vs-CLI skew is out of scope (the `.app` bundle is macOS).
#[cfg(not(target_os = "macos"))]
pub fn installed_app_version() -> Option<String> {
    None
}

/// Convenience wiring: classify the `running_version` against the installed app.
///
/// `running_version` is typically `env!("CARGO_PKG_VERSION")`. A leading `v` is
/// tolerated. Any parse failure (running or app) collapses to [`Skew::Unknown`]
/// so a caller never has to handle an error.
pub fn detect_skew(running_version: &str) -> Skew {
    let running = match Version::parse(running_version.trim_start_matches('v')) {
        Ok(v) => v,
        Err(_) => return Skew::Unknown,
    };
    let app = installed_app_version().and_then(|s| Version::parse(s.trim_start_matches('v')).ok());
    classify_skew(&running, app.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn classify_covers_every_state() {
        assert_eq!(
            classify_skew(&v("0.64.0"), Some(&v("0.64.0"))),
            Skew::InSync
        );
        assert_eq!(
            classify_skew(&v("0.51.0"), Some(&v("0.64.0"))),
            Skew::CliBehind {
                cli: v("0.51.0"),
                app: v("0.64.0")
            }
        );
        assert_eq!(
            classify_skew(&v("0.65.0"), Some(&v("0.64.0"))),
            Skew::CliAhead {
                cli: v("0.65.0"),
                app: v("0.64.0")
            }
        );
        assert_eq!(classify_skew(&v("0.64.0"), None), Skew::Unknown);
    }

    #[test]
    fn classify_respects_semver_precedence() {
        // Pre-release sorts BELOW its release → behind.
        assert!(matches!(
            classify_skew(&v("0.65.0-rc.1"), Some(&v("0.65.0"))),
            Skew::CliBehind { .. }
        ));
        // NOTE: the `semver` crate's `Ord` is a total order that DOES include build
        // metadata (unlike the spec's "precedence" rule), so `0.65.0+build.7` sorts
        // above `0.65.0`. Harmless for us — our tags are clean `vX.Y.Z`, and a build
        // -metadata CLI would only ever land in the silent `CliAhead` state, never a
        // false `CliBehind` warning. Locked here so the behavior can't regress unnoticed.
        assert!(matches!(
            classify_skew(&v("0.65.0+build.7"), Some(&v("0.65.0"))),
            Skew::CliAhead { .. }
        ));
    }

    // Real bundle key ordering: CFBundleInfoDictionaryVersion (=6.0) BEFORE the
    // target, CFBundleVersion AFTER it. A loose parser would return 6.0.
    const PLIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<plist version=\"1.0\">
<dict>
\t<key>CFBundleInfoDictionaryVersion</key>
\t<string>6.0</string>
\t<key>CFBundleName</key>
\t<string>Indexa</string>
\t<key>CFBundleShortVersionString</key>
\t<string>0.64.0</string>
\t<key>CFBundleVersion</key>
\t<string>0.64.0</string>
</dict>
</plist>";

    #[test]
    fn plist_parser_returns_short_version_not_the_dictionary_version() {
        assert_eq!(parse_plist_short_version(PLIST).as_deref(), Some("0.64.0"));
    }

    #[test]
    fn plist_parser_fails_open() {
        assert_eq!(parse_plist_short_version(""), None);
        assert_eq!(parse_plist_short_version("<plist></plist>"), None);
        // Key present but no value element.
        assert_eq!(
            parse_plist_short_version("<key>CFBundleShortVersionString</key>"),
            None
        );
        // Empty value.
        assert_eq!(
            parse_plist_short_version("<key>CFBundleShortVersionString</key><string></string>"),
            None
        );
    }

    #[test]
    fn advice_only_warns_on_cli_behind() {
        let behind = Skew::CliBehind {
            cli: v("0.51.0"),
            app: v("0.64.0"),
        };
        let cli_msg = behind.advice(Surface::Cli).expect("behind → advice");
        assert!(cli_msg.contains("0.51.0") && cli_msg.contains("0.64.0"));
        assert!(cli_msg.contains("indexa update"));
        let mcp_msg = behind.advice(Surface::Mcp).expect("behind → advice");
        assert!(mcp_msg.contains("restart") && mcp_msg.contains("MCP"));

        // Every non-harmful state is silent on both surfaces.
        for skew in [
            Skew::InSync,
            Skew::CliAhead {
                cli: v("0.65.0"),
                app: v("0.64.0"),
            },
            Skew::Unknown,
        ] {
            assert!(skew.advice(Surface::Cli).is_none());
            assert!(skew.advice(Surface::Mcp).is_none());
        }
    }

    #[test]
    fn detect_skew_is_fail_open_on_bad_input() {
        assert_eq!(detect_skew("not-a-version"), Skew::Unknown);
        assert_eq!(detect_skew(""), Skew::Unknown);
    }
}
