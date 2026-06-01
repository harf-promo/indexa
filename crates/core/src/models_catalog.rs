//! A curated catalog of local models worth pulling.
//!
//! This module is **pure**: types plus a bundled builder, no network and no
//! global mutable state. The optional online refresh (a fail-open JSON fetch)
//! and the installed ∪ catalog merge live in the web crate, so this crate's
//! unit tests stay process-isolated.
//!
//! Vendor policy: per the project's model preferences, Chinese-vendor models
//! may be *listed* but are flagged `safe_default = false` so they are never
//! auto-selected as a default.

use serde::{Deserialize, Serialize};

/// What a model is best suited for in Indexa's pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    /// Embedding model (vectors), e.g. `nomic-embed-text`.
    Embed,
    /// Per-file summaries.
    File,
    /// Directory roll-ups.
    Dir,
    /// General question answering. The default for an unspecified role.
    #[default]
    Qa,
    /// Code-aware generation.
    Code,
    /// Multimodal / image understanding.
    Vision,
}

/// One catalog entry: a model a user could pull and use.
///
/// `Deserialize` + `#[serde(default)]` make this lenient for the optional online
/// refresh — a fetched JSON object may carry only `{name, params_b}` and the
/// rest fall back to sensible defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogModel {
    /// Ollama model name (`ollama pull <name>`).
    pub name: String,
    /// Parameter count in billions (e.g. `12.0` for a 12B model).
    pub params_b: f64,
    /// Quantisation level (e.g. `"Q4_K_M"`); drives the weights estimate.
    pub quant: String,
    /// What this model is good for.
    pub role: ModelRole,
    /// Publisher (e.g. `"Google"`, `"Meta"`).
    pub vendor: String,
    /// True for Indexa's current recommended defaults.
    pub recommended_default: bool,
    /// False flags models we won't auto-select as a default (vendor policy).
    pub safe_default: bool,
    /// Short human-readable note.
    pub notes: String,
}

impl Default for CatalogModel {
    fn default() -> Self {
        Self {
            name: String::new(),
            params_b: 0.0,
            quant: "Q4_K_M".into(),
            role: ModelRole::Qa,
            vendor: String::new(),
            recommended_default: false,
            safe_default: true,
            notes: String::new(),
        }
    }
}

/// The bundled, hand-curated catalog. Current defaults are
/// `recommended_default: true`; Chinese-vendor models are `safe_default: false`.
pub fn bundled_catalog() -> Vec<CatalogModel> {
    vec![
        CatalogModel {
            name: "nomic-embed-text".into(),
            params_b: 0.137,
            quant: "F16".into(),
            role: ModelRole::Embed,
            vendor: "Nomic".into(),
            recommended_default: true,
            safe_default: true,
            notes: "Default local embedder (768-dim).".into(),
        },
        CatalogModel {
            name: "gemma3:4b".into(),
            params_b: 4.0,
            quant: "Q4_K_M".into(),
            role: ModelRole::File,
            vendor: "Google".into(),
            recommended_default: true,
            safe_default: true,
            notes: "Default file-summary model; fast and light.".into(),
        },
        CatalogModel {
            name: "gemma3:12b".into(),
            params_b: 12.0,
            quant: "Q4_K_M".into(),
            role: ModelRole::Dir,
            vendor: "Google".into(),
            recommended_default: true,
            safe_default: true,
            notes: "Default dir roll-up + Q&A model; multimodal.".into(),
        },
        CatalogModel {
            name: "llama3.1:8b".into(),
            params_b: 8.0,
            quant: "Q4_K_M".into(),
            role: ModelRole::Qa,
            vendor: "Meta".into(),
            recommended_default: false,
            safe_default: true,
            notes: "General-purpose 8B; strong all-rounder.".into(),
        },
        CatalogModel {
            name: "phi3.5:3.8b".into(),
            params_b: 3.8,
            quant: "Q4_K_M".into(),
            role: ModelRole::File,
            vendor: "Microsoft".into(),
            recommended_default: false,
            safe_default: true,
            notes: "Compact, low-footprint file summaries.".into(),
        },
        CatalogModel {
            name: "mistral:7b".into(),
            params_b: 7.0,
            quant: "Q4_K_M".into(),
            role: ModelRole::Qa,
            vendor: "Mistral".into(),
            recommended_default: false,
            safe_default: true,
            notes: "Efficient 7B general model.".into(),
        },
        CatalogModel {
            name: "llama3.2-vision:11b".into(),
            params_b: 11.0,
            quant: "Q4_K_M".into(),
            role: ModelRole::Vision,
            vendor: "Meta".into(),
            recommended_default: false,
            safe_default: true,
            notes: "Vision-capable; image understanding.".into(),
        },
        CatalogModel {
            name: "qwen2.5-coder:7b".into(),
            params_b: 7.0,
            quant: "Q4_K_M".into(),
            role: ModelRole::Code,
            vendor: "Alibaba".into(),
            recommended_default: false,
            safe_default: false,
            notes: "Code-focused; listed but flagged non-default per vendor policy.".into(),
        },
        CatalogModel {
            name: "qwen2.5:14b".into(),
            params_b: 14.0,
            quant: "Q4_K_M".into(),
            role: ModelRole::Qa,
            vendor: "Alibaba".into(),
            recommended_default: false,
            safe_default: false,
            notes: "Capable 14B; listed but flagged non-default per vendor policy.".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalog_is_non_empty_and_has_an_embedder() {
        let cat = bundled_catalog();
        assert!(cat.len() >= 6);
        assert!(cat.iter().any(|m| m.role == ModelRole::Embed));
    }

    #[test]
    fn chinese_vendor_models_are_not_safe_defaults() {
        for m in bundled_catalog() {
            if m.vendor == "Alibaba" {
                assert!(!m.safe_default, "{} should be flagged non-default", m.name);
            }
        }
    }

    #[test]
    fn current_defaults_are_recommended() {
        let cat = bundled_catalog();
        for name in ["nomic-embed-text", "gemma3:4b", "gemma3:12b"] {
            let m = cat.iter().find(|m| m.name == name).unwrap();
            assert!(m.recommended_default, "{name} should be recommended");
        }
    }

    #[test]
    fn lenient_deserialize_fills_defaults() {
        // The online-refresh JSON may carry only name + params_b.
        let m: CatalogModel = serde_json::from_str(r#"{"name":"foo:7b","params_b":7.0}"#).unwrap();
        assert_eq!(m.name, "foo:7b");
        assert_eq!(m.role, ModelRole::Qa);
        assert_eq!(m.quant, "Q4_K_M");
        assert!(m.safe_default);
    }
}
