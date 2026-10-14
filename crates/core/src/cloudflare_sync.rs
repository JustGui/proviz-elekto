//! Account-scoped Workers AI roster + marketplace capability metadata. Jev is documented
//! separately and currently omitted by model search; retain that explicit native supplement.
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

use crate::{builtin_providers, env_expand::provider_env, storage::CatalogStorage};

pub const DEFAULT_BASE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai";
const MIN_SANE_MODEL_COUNT: usize = 10;

pub struct CloudflareSyncSummary {
    pub fetched: usize,
    pub models_added: usize,
    pub models_updated: usize,
    pub models_disabled: usize,
}

#[derive(Deserialize)]
struct Page {
    success: bool,
    result: Vec<CloudflareModel>,
}

#[derive(Deserialize)]
struct CloudflareModel {
    name: String,
    task: Task,
    #[serde(default)]
    properties: Vec<Property>,
}
#[derive(Deserialize)]
struct Task {
    name: String,
}
#[derive(Deserialize)]
struct Property {
    property_id: String,
    value: Value,
}
#[derive(Deserialize)]
struct Marketplace {
    data: Vec<ChatModel>,
}
#[derive(Deserialize)]
struct ChatModel {
    id: String,
    #[serde(default)]
    supported_features: Vec<String>,
    max_output_length: Option<u32>,
}

impl CloudflareModel {
    fn property(&self, key: &str) -> Option<&Value> {
        self.properties
            .iter()
            .find(|p| p.property_id == key)
            .map(|p| &p.value)
    }
    fn context(&self) -> Option<u32> {
        let v = self.property("context_window")?;
        v.as_u64()
            .and_then(|n| n.try_into().ok())
            .or_else(|| v.as_str()?.parse().ok())
    }
    fn price(&self, unit: &str) -> Option<f64> {
        self.property("price")?
            .as_array()?
            .iter()
            .find(|p| p["unit"] == unit && p["currency"] == "USD")?
            .get("price")?
            .as_f64()
            .filter(|p| p.is_finite() && *p >= 0.0)
    }
}

fn map_entry(m: &CloudflareModel, chat: Option<&ChatModel>) -> Value {
    let category = match m.task.name.as_str() {
        "Text Generation" if chat.is_some() => "text",
        "Text Embeddings" => "embedding",
        "Text-to-Image" => "image",
        "Automatic Speech Recognition" => "stt",
        "Text-to-Speech" => "tts",
        // Unverified/non-chat endpoints stay catalog-only, outside ordinary chat selection.
        _ => "unsupported",
    };
    let features = chat
        .map(|m| m.supported_features.as_slice())
        .unwrap_or_default();
    json!({
        "slug": m.name,
        "display_name": m.name,
        "category": category,
        "max_context_tokens": m.context(),
        "max_output_tokens": chat.and_then(|m| m.max_output_length),
        "supports_function_calling": features.iter().any(|f| f == "tools"),
        "supports_json_mode": features.iter().any(|f| f == "json_mode"),
        "price_input_per_1m": m.price("per M input tokens"),
        "price_output_per_1m": m.price("per M output tokens"),
        // Workers AI does not train models or improve services using customer content:
        // https://developers.cloudflare.com/workers-ai/platform/privacy/
        "trains_on_data": false,
        // No model-specific retention commitment inferred from the no-training statement.
        "retains_data": null,
        "price_synced_at": Utc::now().to_rfc3339(),
        "notes": format!("Cloudflare task: {}. Reported features: {}. Non-chat tasks are catalog-only; no native audio/image adapter. JSON schema support is model-specific; supports_json_mode does not promise json_schema.", m.task.name, features.join(", ")),
    })
}

fn jev_entry() -> Value {
    // Cloudflare model page: 32k, $0.042/M input, $0 output/cache, ZDR yes.
    // https://developers.cloudflare.com/ai/models/typesafe/jev/
    // No training also explicitly promised by TypeSafe for this third-party model:
    // https://typesafe.ai/legal/privacy-policy
    // Cloudflare's account-wide no-training statement:
    // https://developers.cloudflare.com/workers-ai/platform/privacy/
    let mut entry: Vec<Value> =
        serde_json::from_str(include_str!("../../../providers/cloudflare/jev.json"))
            .expect("built-in Jev definition");
    let mut entry = entry.remove(0);
    entry["price_synced_at"] = json!(Utc::now().to_rfc3339());
    entry
}

fn fetch(
    client: &reqwest::blocking::Client,
    url: &str,
    key: &str,
) -> Result<reqwest::blocking::Response, String> {
    // Do not include account-bearing URLs, upstream bodies or credentials in diagnostics.
    client
        .get(url)
        .bearer_auth(key)
        .send()
        .map_err(|_| "cloudflare catalog network request failed".to_string())?
        .error_for_status()
        .map_err(|e| {
            format!(
                "cloudflare catalog HTTP {}",
                e.status().map(|s| s.as_u16()).unwrap_or(0)
            )
        })
}

fn fetch_entries(base_url: &str, key: &str) -> Result<(usize, Vec<Value>), String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(crate::SYNC_USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| "cloudflare HTTP client initialization failed")?;
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    // `total_count` includes models not returned to this account (live: 326 vs 70).
    // Iterate until an empty page, not until total_count; reject repeated/nonterminating pages.
    for page in 1..=100 {
        let parsed: Page = fetch(
            &client,
            &format!("{base_url}/models/search?per_page=100&page={page}"),
            key,
        )?
        .json()
        .map_err(|_| "invalid Cloudflare model-search response")?;
        if !parsed.success {
            return Err("Cloudflare model search unsuccessful".into());
        }
        if parsed.result.is_empty() {
            break;
        }
        for m in parsed.result {
            if m.name.is_empty() || !seen.insert(m.name.clone()) {
                return Err("Cloudflare catalog contains empty/repeated model names".into());
            }
            models.push(m);
        }
        if page == 100 {
            return Err("Cloudflare pagination limit reached".into());
        }
    }
    // This documented API format supplies per-model json_mode/structured_outputs/tools flags;
    // the native roster's function_calling flag alone cannot establish JSON support.
    // https://developers.cloudflare.com/api/resources/ai/subresources/models/methods/list/
    let marketplace: Marketplace = fetch(
        &client,
        &format!("{base_url}/models/search?format=openrouter"),
        key,
    )?
    .json()
    .map_err(|_| "invalid Cloudflare marketplace response")?;
    let chats: HashMap<_, _> = marketplace
        .data
        .iter()
        .map(|m| (m.id.as_str(), m))
        .collect();
    let mut entries: Vec<_> = models
        .iter()
        .filter(|m| m.name != "typesafe/jev")
        .map(|m| map_entry(m, chats.get(m.name.as_str()).copied()))
        .collect();
    if chats.is_empty() || !entries.iter().any(|e| e["category"] == "text") {
        return Err("Cloudflare catalog has no verified chat models".into());
    }
    entries.push(jev_entry());
    entries.sort_by(|a, b| a["slug"].as_str().cmp(&b["slug"].as_str()));
    Ok((models.len(), entries))
}

pub fn fetch_preview(base_url: &str) -> Result<Vec<Value>, String> {
    let key = provider_env("CLOUDFLARE_API_KEY")
        .map_err(|_| "CLOUDFLARE_API_KEY (or CLOUDFARE_API_KEY) is required")?;
    let base_url = crate::env_expand::expand_env_placeholders(base_url);
    if base_url.contains("${") {
        return Err("CLOUDFLARE_ACCOUNT_ID (or CLOUDFARE_ACCOUNT_ID) is required".into());
    }
    fetch_entries(base_url.trim_end_matches('/'), &key).map(|(_, entries)| entries)
}

pub fn sync(
    storage: &dyn CatalogStorage,
    providers_dir: &str,
    base_url: &str,
) -> Result<CloudflareSyncSummary, String> {
    let entries = fetch_preview(base_url)?;
    // Includes the documented Jev supplement. Never replace last-good data on a tiny response.
    if entries.len() <= MIN_SANE_MODEL_COUNT {
        return Err("Cloudflare catalog suspiciously small; keeping last-good catalog".into());
    }
    let dir = std::path::Path::new(providers_dir).join("cloudflare");
    if !dir.join("brand.json").exists() {
        return Err("Cloudflare brand.json missing".into());
    }
    let body = serde_json::to_vec_pretty(&entries).map_err(|e| e.to_string())?;
    // Atomic replacement so a crash cannot leave a half-written catalogue.
    let staging = dir.join("models.json.tmp");
    std::fs::write(&staging, body).map_err(|e| e.to_string())?;
    std::fs::rename(staging, dir.join("models.json")).map_err(|e| e.to_string())?;
    let s = builtin_providers::load_provider_from_dir(storage, providers_dir, "cloudflare")
        .map_err(|e| e.to_string())?;
    Ok(CloudflareSyncSummary {
        fetched: entries.len(),
        models_added: s.models_added,
        models_updated: s.models_updated,
        models_disabled: s.models_disabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_native_properties_prices_and_conservative_privacy() {
        let m: CloudflareModel = serde_json::from_value(json!({"name":"@cf/example/chat", "task":{"name":"Text Generation"},"properties":[
            {"property_id":"context_window","value":"32000"},
            {"property_id":"function_calling","value":"true"},
            {"property_id":"price","value":[{"unit":"per M input tokens","price":0.15,"currency":"USD"},{"unit":"per M output tokens","price":0,"currency":"USD"}]}]})).unwrap();
        let chat: ChatModel = serde_json::from_value(json!({"id":m.name,"supported_features":["tools","json_mode"],"max_output_length":4096})).unwrap();
        let entry = map_entry(&m, Some(&chat));
        assert_eq!(entry["category"], "text");
        assert_eq!(entry["max_context_tokens"], 32000);
        assert_eq!(entry["max_output_tokens"], 4096);
        assert_eq!(entry["price_input_per_1m"], 0.15);
        assert_eq!(entry["price_output_per_1m"], 0.0);
        assert_eq!(entry["supports_function_calling"], true);
        assert_eq!(entry["supports_json_mode"], true);
        assert_eq!(entry["trains_on_data"], false);
        assert!(entry["retains_data"].is_null());
        let unknown = map_entry(&m, None);
        assert_eq!(unknown["category"], "unsupported");
        assert_eq!(unknown["supports_json_mode"], false);
        assert_eq!(unknown["supports_function_calling"], false);
    }
    #[test]
    fn jev_has_decision_category_zero_prices_and_zdr() {
        let e = jev_entry();
        assert_eq!(e["slug"], "typesafe/jev");
        assert_eq!(e["category"], "decision");
        assert_eq!(e["trains_on_data"], false);
        assert_eq!(e["retains_data"], false);
        assert_eq!(e["price_input_per_1m"], 0.042);
        assert_eq!(e["price_output_per_1m"], 0);
        assert_eq!(e["price_cached_input_per_1m"], 0);
        assert_eq!(e["max_context_tokens"], 32000);
        assert_eq!(e["supports_function_calling"], false);
    }
    #[test]
    fn unknown_task_and_units_never_become_chat_or_free_tokens() {
        let m: CloudflareModel = serde_json::from_value(json!({"name":"@cf/example/image", "task":{"name":"Text-to-Image"},"properties":[{"property_id":"price","value":[{"unit":"per image","price":0.01,"currency":"USD"}]}]})).unwrap();
        let e = map_entry(&m, None);
        assert_eq!(e["category"], "image");
        assert!(e["price_input_per_1m"].is_null());
        assert!(e["max_context_tokens"].is_null());
    }
}
