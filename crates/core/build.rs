//! Validates `canonical_model` cross-references at build time, so a typo in either
//! `providers/<name>/models.json` or `providers/model_catalog.json` fails `cargo build`/`check`/
//! `test` (and therefore CI) instead of silently just not inheriting anything at runtime — see
//! "OpenRouter auto-sync & shared model catalog" in CLAUDE.md.
//!
//! Auto-synced providers (OpenRouter, Requesty, ...) auto-fill `canonical_model` from every
//! model's upstream identity field, most of which nobody has curated a `model_catalog.json`
//! entry for yet — that's expected and not a typo, so those only get a `cargo:warning`.
//! Hand-curated providers set `canonical_model` deliberately, with a human typing both sides of
//! the reference — a non-match there is almost certainly a typo, so that's a hard build failure.

use std::collections::HashSet;
use std::path::Path;

/// Provider directories whose `models.json` is machine-generated, not hand-edited.
const AUTO_SYNCED_PROVIDERS: &[&str] = &["openrouter", "requesty", "nousportal", "orcarouter"];

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let providers_dir = Path::new(&manifest_dir).join("../../providers");

    println!("cargo:rerun-if-changed={}", providers_dir.display());

    if !providers_dir.exists() {
        // Not every checkout/package has the providers/ tree (e.g. a published crate) — nothing
        // to validate.
        return;
    }

    let catalog_path = providers_dir.join("model_catalog.json");
    let catalog_keys: HashSet<String> = if catalog_path.exists() {
        let raw = std::fs::read_to_string(&catalog_path).unwrap_or_else(|e| {
            panic!("failed to read {}: {e}", catalog_path.display());
        });
        let entries: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_else(|e| {
            panic!("failed to parse {}: {e}", catalog_path.display());
        });
        entries
            .iter()
            .filter_map(|e| e.get("canonical_key").and_then(|v| v.as_str()))
            .map(String::from)
            .collect()
    } else {
        HashSet::new()
    };

    let entries = std::fs::read_dir(&providers_dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", providers_dir.display()));

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let provider_name = entry.file_name().to_string_lossy().to_string();
        let models_path = entry.path().join("models.json");
        if !models_path.exists() {
            continue;
        }

        let raw = match std::fs::read_to_string(&models_path) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("[{provider_name}] failed to read models.json: {e}"));
                continue;
            }
        };
        let models: Vec<serde_json::Value> = match serde_json::from_str(&raw) {
            Ok(m) => m,
            Err(e) => {
                errors.push(format!("[{provider_name}] invalid models.json: {e}"));
                continue;
            }
        };

        let lenient = AUTO_SYNCED_PROVIDERS.contains(&provider_name.as_str());

        for model in &models {
            let Some(key) = model.get("canonical_model").and_then(|v| v.as_str()) else {
                continue;
            };
            if catalog_keys.contains(key) {
                continue;
            }
            let slug = model
                .get("slug")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown slug>");
            let msg = format!(
                "[{provider_name}] {slug}: canonical_model '{key}' not found in \
                 providers/model_catalog.json"
            );
            if lenient {
                warnings.push(msg);
            } else {
                errors.push(msg);
            }
        }
    }

    for w in &warnings {
        println!("cargo:warning={w}");
    }

    if !errors.is_empty() {
        panic!(
            "\n\ncanonical_model validation failed ({} broken reference{}):\n  {}\n\n\
             Fix the typo in models.json, or add the missing canonical_key to \
             providers/model_catalog.json.\n",
            errors.len(),
            if errors.len() == 1 { "" } else { "s" },
            errors.join("\n  "),
        );
    }
}
