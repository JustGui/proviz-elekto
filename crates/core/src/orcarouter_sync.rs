//! Fetches OrcaRouter's public model catalog (`GET {base_url}/models`) and upserts it into the
//! local provider directory + DB via `builtin_providers::load_from_dir` — same generated-`models.json`
//! shape as `openrouter_sync`, because OrcaRouter's `/v1/models` response is OpenRouter-shaped for
//! the basics: `data[]` of `{id, name, context_length, top_provider.max_completion_tokens,
//! pricing.prompt/completion}` (dollar-per-token strings).
//!
//! **Where OrcaRouter is NOT an OpenRouter fork** — and why this is its own module, not a
//! parameterisation of `openrouter_sync`:
//!
//!   1. **No `supported_parameters`, no `hugging_face_id`, no cache pricing in `/v1/models`.**
//!      The API only carries base-tier input/output price. Capability flags (`tools` /
//!      `response_format`) and the prompt-cache-hit price live on the SEO-rendered model detail
//!      page (`https://www.orcarouter.ai/models/{provider}/{slug}`), server-rendered, no JS. So
//!      newly-seen models are **enriched once** by scraping that page (`enrich_from_web`) — capped
//!      per run, fail-soft to the API defaults. Models already in the on-disk `models.json` are
//!      never re-scraped: enrichment is a one-time cost per model, paid when it first appears.
//!   2. **Mixed catalog.** `/v1/models` lists chat, embeddings, image-generation, video, and
//!      per-call ("dub") models together, distinguishable only by `supported_endpoint_types` +
//!      pricing shape + id. `classify` keeps chat models (`category: "text"`, routable) and
//!      embeddings (`category: "embedding"`, catalog-only), drops everything else.
//!   3. **`trains_on_data` / `retains_data`.** OrcaRouter has account-wide Zero Data Retention on
//!      its own servers, but routes to upstream providers under *their* retention/training terms,
//!      which the API never names. We can't know the upstream per request, so every row is stamped
//!      `trains_on_data: true` / `retains_data: true` (assume the worst) — this keeps OrcaRouter
//!      out of `require_no_training` steps. Flip a row by hand if you know its upstream is safe
//!      (won't survive the next sync — same caveat as any hand-edit to a generated `models.json`).
//!
//! The real per-request cost (`usage.cost_usd`, opt-in via the `X-OrcaRouter-Include-Cost: true`
//! header) and the `reasoning_effort: "minimal"` default are handled in `server/src/complete.rs`,
//! not here.

use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::{builtin_providers::load_from_dir, storage::CatalogStorage};

pub const DEFAULT_BASE_URL: &str = "https://api.orcarouter.ai/v1";

/// SEO-rendered model detail pages live under this host (not the API host). `enrich_from_web`
/// fetches `{MODEL_PAGE_BASE}/{provider}/{slug}` for the fields `/v1/models` omits.
const MODEL_PAGE_BASE: &str = "https://www.orcarouter.ai/models";

/// Default cap on how many newly-seen models to enrich (scrape the detail page for) in a single
/// sync run. The server task uses this; `PROVIZ_ORCAROUTER_ENRICH_MAX` overrides it. Keeps a
/// first-ever seed from firing ~150 page requests at once — the remainder are picked up over the
/// next few sync cycles as they stay "new" until written. The CLI `sync-orcarouter` passes its
/// own (large) `--enrich-max` so a one-time manual seed enriches everything in one go.
pub const DEFAULT_ENRICH_MAX: usize = 30;

/// Below this many models, a fetch is treated as suspect (truncated/empty response) and the whole
/// sync is skipped rather than upserted — protects against `disable_missing` mass-disabling the
/// catalog on a bad response. OrcaRouter has consistently listed 190+ models.
const MIN_SANE_MODEL_COUNT: usize = 100;

/// Fallback `max_context_tokens` when neither the API nor the detail page gives one. Without it a
/// row would land as `0` (`load_from_dir`'s `unwrap_or(0)`) and the selector's context-fit filter
/// (`max_context_tokens < estimated_tokens`) would drop it from every pool. `16384` keeps such a
/// model routable for ordinary requests while still deferring to any real value the moment one
/// appears (API or enrichment).
const FALLBACK_CONTEXT_TOKENS: u32 = 16_384;

/// Reads `PROVIZ_ORCAROUTER_ENRICH_MAX`, falling back to `DEFAULT_ENRICH_MAX`.
pub fn enrich_max_from_env() -> usize {
    std::env::var("PROVIZ_ORCAROUTER_ENRICH_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_ENRICH_MAX)
}

pub struct OrcaRouterSyncSummary {
    pub fetched: usize,
    pub brands_added: usize,
    pub models_added: usize,
    pub models_updated: usize,
    pub models_disabled: usize,
    /// How many newly-seen models were scraped for capability flags + cache price this run.
    pub enriched: usize,
    pub skipped_suspicious: bool,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<OrcaModel>,
}

#[derive(Deserialize)]
struct OrcaModel {
    id: String,
    name: Option<String>,
    context_length: Option<u32>,
    max_completion_tokens: Option<u32>,
    /// `Option<Vec<_>>` rather than `#[serde(default)] Vec<_>`: some rows send the key with an
    /// explicit `null`, which a plain `Vec` deserializer rejects outright.
    supported_endpoint_types: Option<Vec<String>>,
    pricing: Option<OrcaPricing>,
    top_provider: Option<OrcaTopProvider>,
}

#[derive(Deserialize)]
struct OrcaPricing {
    prompt: Option<String>,
    completion: Option<String>,
    /// Present (and `prompt`/`completion` absent) on per-call models — image, video, "dub".
    request: Option<String>,
}

#[derive(Deserialize)]
struct OrcaTopProvider {
    context_length: Option<u32>,
    max_completion_tokens: Option<u32>,
}

impl OrcaModel {
    fn endpoint_types(&self) -> &[String] {
        self.supported_endpoint_types.as_deref().unwrap_or(&[])
    }
}

/// Dollar-per-token string (e.g. `"0.0000000300"`) → dollar-per-million-tokens. Non-positive /
/// unparseable values map to `None` (same guard as `nousportal_sync`; a `"0"` completion price on
/// an embedding row means "n/a", not "free").
fn price_per_1m(raw: &Option<String>) -> Option<f64> {
    raw.as_ref()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|per_token| *per_token > 0.0)
        .map(|per_token| per_token * 1_000_000.0)
}

/// id fragments that mark a non-chat model whose pricing is still token-shaped (so the
/// pricing-shape check alone wouldn't catch it) — image generators billed per-token, TTS, etc.
const NON_CHAT_ID_MARKERS: &[&str] = &[
    "image", "imagen", "imagine", "dall-e", "tts", "whisper", "seedance", "sora", "veo", "/dub",
    "-dub", "-video",
];

/// Decides what (if anything) to write for one `/v1/models` entry:
///   - `Some("text")`  — routable chat model
///   - `Some("embedding")` — embedding model, written for the catalog but not selector-routable
///   - `None` — skip (meta-routers, `-free` tier, image / video / audio / per-call models)
fn classify(m: &OrcaModel) -> Option<&'static str> {
    let id = m.id.to_ascii_lowercase();
    // OrcaRouter's own meta-routers (orcarouter/auto, orcarouter/fusion*, orcarouter/free) have
    // no fixed pricing and pick a model themselves — proviz does its own selection.
    if id.starts_with("orcarouter/") {
        return None;
    }
    // The `-free` tier is rate-limited by request rate with its own distinct 429 semantics
    // (see OrcaRouter docs "Free Models") — not a normal routable candidate.
    if id.ends_with("-free") {
        return None;
    }
    if m.endpoint_types().iter().any(|e| e == "embeddings") {
        return Some("embedding");
    }
    let pricing = m.pricing.as_ref();
    // Per-call models (image / video / "dub") quote `pricing.request` and no `prompt`/`completion`.
    let token_priced = pricing.map(|p| p.prompt.is_some()).unwrap_or(false);
    if !token_priced && pricing.map(|p| p.request.is_some()).unwrap_or(false) {
        return None;
    }
    if NON_CHAT_ID_MARKERS.iter().any(|marker| id.contains(marker)) {
        return None;
    }
    Some("text")
}

/// Fields scraped from a model's SEO detail page that `/v1/models` doesn't carry. Every field is
/// best-effort — a page redesign silently degrades this to "no enrichment", never an error.
#[derive(Default)]
struct WebEnrichment {
    supports_function_calling: Option<bool>,
    supports_json_mode: Option<bool>,
    /// Base-tier prompt-cache-read price, per 1M tokens.
    price_cached_input_per_1m: Option<f64>,
    /// From the page's ld+json — only used to fill an API gap.
    max_context_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
}

/// Flattens HTML to spaced *visible* text: `<script>` / `<style>` blocks are dropped whole (the
/// page ships a large i18n string bundle inline that would otherwise leak label text like
/// `"Cache read / 1M"` into the parse), then remaining tags are stripped and whitespace collapsed.
fn flatten_html(html: &str) -> String {
    let mut cleaned = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        cleaned.push_str(&rest[..open]);
        let tail = &rest[open..];
        let lower = tail
            .get(..8)
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let skip_to = if lower.starts_with("<script") {
            tail.to_ascii_lowercase()
                .find("</script>")
                .map(|e| e + "</script>".len())
        } else if lower.starts_with("<style") {
            tail.to_ascii_lowercase()
                .find("</style>")
                .map(|e| e + "</style>".len())
        } else {
            None
        };
        match skip_to {
            Some(end) => {
                cleaned.push(' ');
                rest = &tail[end..];
            }
            None => match tail.find('>') {
                Some(close) => {
                    cleaned.push(' ');
                    rest = &tail[close + 1..];
                }
                None => {
                    rest = "";
                    break;
                }
            },
        }
    }
    cleaned.push_str(rest);
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `$`-prefixed numbers appearing after `anchor`, in order, up to `max`. A trailing `.` / `,`
/// (sentence punctuation, version strings) is trimmed; malformed tokens are skipped.
fn dollar_values_after(text: &str, anchor: &str, max: usize) -> Vec<f64> {
    let Some(start) = text.find(anchor) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for tok in text[start + anchor.len()..].split_whitespace() {
        if out.len() >= max {
            break;
        }
        if let Some(num) = tok.strip_prefix('$') {
            let num = num.trim_end_matches(['.', ',']);
            if let Ok(v) = num.parse::<f64>() {
                if v.is_finite() && v >= 0.0 {
                    out.push(v);
                }
            }
        }
    }
    out
}

/// Base-tier prompt-cache-read price from either detail-page layout:
///   - **flat**  `Input / 1M tokens $10.00 Output / 1M tokens $50.00 Cache read / 1M $1.00 ...`
///     — the value sits right after its own label, so the first `$` after "Cache read / 1M" is it.
///   - **tiered** `... Cache read / 1M Cache write / 1M ≤ 32K $0.030 $0.130 $0.0060 $0.038 ...`
///     — the label is immediately followed by "Cache write / 1M", then a 4-value row
///     (input, output, cache-read, cache-write); we want index 2.
fn parse_cache_read_price(text: &str) -> Option<f64> {
    let anchor = "Cache read / 1M";
    let ri = text.find(anchor)?;
    let after = &text[ri + anchor.len()..];
    let first_dollar = after.find('$')?;
    let tiered = after[..first_dollar].contains("Cache write");
    let vals = dollar_values_after(text, anchor, 4);
    let price = if tiered { vals.get(2) } else { vals.first() };
    price.copied().filter(|v| *v > 0.0)
}

/// First integer value of a `{"@type":"PropertyValue","name":"<key>","value":N}` ld+json entry.
fn ld_json_property(text: &str, key: &str) -> Option<u64> {
    let needle = format!("\"name\":\"{key}\"");
    let i = text.find(&needle)?;
    let tail = &text[i..];
    let vi = tail.find("\"value\":")? + "\"value\":".len();
    let digits: String = tail[vi..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Fetches `{MODEL_PAGE_BASE}/{slug}` and parses the "Supported parameters" list, the cache-read
/// price, and the ld+json context/output sizes. Returns `None` on a network error **or** when the
/// page came back as the pre-hydration shell (no "Supported parameters" and no "Pricing" — the
/// CDN serves this intermittently); the caller then leaves the model un-enriched and retries it on
/// a later sync. `Some` means the SSR page rendered and at least the flags were read.
fn enrich_from_web(client: &reqwest::blocking::Client, slug: &str) -> Option<WebEnrichment> {
    let url = format!("{MODEL_PAGE_BASE}/{slug}");
    let body = client
        .get(&url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|e| tracing::debug!(slug, %e, "orcarouter enrich: page fetch failed"))
        .ok()?;
    let text = flatten_html(&body);

    let params_start = text.find("Supported parameters");
    if params_start.is_none() && !text.contains("Cache read / 1M") {
        tracing::debug!(
            slug,
            "orcarouter enrich: SSR shell (no content) — will retry next sync"
        );
        return None;
    }

    let mut out = WebEnrichment::default();
    if let Some(ps) = params_start {
        let end = text[ps..]
            .find("Pricing")
            .map(|e| ps + e)
            .unwrap_or((ps + 400).min(text.len()));
        let params = &text[ps..end];
        out.supports_function_calling =
            Some(params.contains(" tools") || params.contains(" tool_choice"));
        out.supports_json_mode = Some(params.contains("response_format"));
    }
    out.price_cached_input_per_1m = parse_cache_read_price(&text);
    out.max_context_tokens = ld_json_property(&text, "contextWindow");
    out.max_output_tokens = ld_json_property(&text, "maxOutputTokens");
    Some(out)
}

/// Maps one `/v1/models` entry to the on-disk `models.json` shape using **only** API data.
/// Capability flags default to `true` for chat models (OrcaRouter's docs state `tools` /
/// `tool_choice` / `response_format` work across every chat-capable upstream); embeddings get
/// neither. Web enrichment (applied afterwards in `build_entries`) overrides these and adds the
/// cache price.
fn map_entry(m: &OrcaModel, category: &str, synced_at: chrono::DateTime<Utc>) -> serde_json::Value {
    let pricing = m.pricing.as_ref();
    let top = m.top_provider.as_ref();
    let is_chat = category == "text";
    // The API sometimes sends `0` (not just omitting the key) for an unknown size — treat both the
    // same.
    let nonzero = |v: Option<u32>| v.filter(|n| *n > 0);
    json!({
        "slug": m.id,
        "display_name": m.name,
        // Left null when the API omits it (or sends 0); `build_entries` applies
        // FALLBACK_CONTEXT_TOKENS only after enrichment/carry-forward have had a chance to supply
        // a real value.
        "max_context_tokens": nonzero(m.context_length).or_else(|| nonzero(top.and_then(|t| t.context_length))),
        "max_output_tokens": nonzero(m.max_completion_tokens).or_else(|| nonzero(top.and_then(|t| t.max_completion_tokens))),
        "supports_function_calling": is_chat,
        "supports_json_mode": is_chat,
        "price_input_per_1m": pricing.and_then(|p| price_per_1m(&p.prompt)),
        "price_output_per_1m": pricing.and_then(|p| price_per_1m(&p.completion)),
        "price_cached_input_per_1m": serde_json::Value::Null,
        "category": category,
        // Upstream provider is unknown per request — assume it may train on / retain data, keeping
        // OrcaRouter out of `require_no_training` steps. See module docs.
        "trains_on_data": true,
        "retains_data": true,
        "price_synced_at": synced_at.to_rfc3339(),
    })
}

/// Overlays scraped fields onto an API-only entry. `enriched_at` marks the entry as done so later
/// syncs neither re-scrape it nor let its scraped fields revert to the API defaults.
fn apply_enrichment(entry: &mut serde_json::Value, enr: &WebEnrichment, synced_at: &str) {
    let obj = entry.as_object_mut().expect("entry is object");
    if let Some(v) = enr.supports_function_calling {
        obj.insert("supports_function_calling".into(), json!(v));
    }
    if let Some(v) = enr.supports_json_mode {
        obj.insert("supports_json_mode".into(), json!(v));
    }
    if let Some(v) = enr.price_cached_input_per_1m {
        // A cache-read price is always a discount on the input price — reject anything ≥ it as a
        // misparse (e.g. a higher pricing tier's number picked up from a malformed table).
        let input = obj.get("price_input_per_1m").and_then(|p| p.as_f64());
        if input.map(|i| v < i).unwrap_or(true) {
            obj.insert("price_cached_input_per_1m".into(), json!(v));
        } else {
            tracing::debug!(
                cache = v,
                input,
                "orcarouter enrich: implausible cache price, ignoring"
            );
        }
    }
    // Only fill context / output when the API left them blank (null, absent, or a `0` sentinel).
    if is_missing_size(obj.get("max_context_tokens")) {
        if let Some(v) = enr.max_context_tokens {
            obj.insert("max_context_tokens".into(), json!(v));
        }
    }
    if is_missing_size(obj.get("max_output_tokens")) {
        if let Some(v) = enr.max_output_tokens {
            obj.insert("max_output_tokens".into(), json!(v));
        }
    }
    obj.insert("enriched_at".into(), json!(synced_at));
}

/// A `max_*_tokens` field that carries no usable value: absent, `null`, or `0`.
fn is_missing_size(v: Option<&serde_json::Value>) -> bool {
    match v {
        None | Some(serde_json::Value::Null) => true,
        Some(n) => n.as_u64() == Some(0),
    }
}

/// Carries the scraped fields of a previously-enriched entry forward onto a fresh API-only entry,
/// so a re-sync doesn't wipe them (the whole file is regenerated from the API each run).
fn carry_forward_enrichment(entry: &mut serde_json::Value, prior: &serde_json::Value) {
    let obj = entry.as_object_mut().expect("entry is object");
    for key in [
        "supports_function_calling",
        "supports_json_mode",
        "price_cached_input_per_1m",
        "enriched_at",
    ] {
        if let Some(v) = prior.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    for key in ["max_context_tokens", "max_output_tokens"] {
        if is_missing_size(obj.get(key)) && !is_missing_size(prior.get(key)) {
            obj.insert(key.into(), prior[key].clone());
        }
    }
}

/// The previous `{providers_dir}/orcarouter/models.json` as a slug → entry map.
fn prior_entries(providers_dir: &str) -> std::collections::HashMap<String, serde_json::Value> {
    let path = std::path::Path::new(providers_dir)
        .join("orcarouter")
        .join("models.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
        .map(|entries| {
            entries
                .into_iter()
                .filter_map(|e| {
                    e.get("slug")
                        .and_then(|s| s.as_str())
                        .map(|s| (s.to_string(), e.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn is_enriched(entry: &serde_json::Value) -> bool {
    entry
        .get("enriched_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
}

/// Maps the catalog to the `models.json` shape. Entries already marked `enriched_at` in `prior`
/// keep their scraped fields (carried forward). Entries not yet enriched are scraped now, up to
/// `enrich_max` per run — a scrape that hits the CDN's empty SSR shell leaves the entry
/// un-enriched so a later run retries it. Shared by `sync` and `fetch_preview`.
fn build_entries(
    data: &[OrcaModel],
    prior: &std::collections::HashMap<String, serde_json::Value>,
    enrich_max: usize,
) -> (Vec<serde_json::Value>, usize) {
    let synced_at = Utc::now();
    let synced_at_str = synced_at.to_rfc3339();
    let page_client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .user_agent("proviz-elekto-sync")
        .build()
        .ok();

    let mut entries = Vec::new();
    let mut enriched = 0usize;
    for m in data {
        let Some(category) = classify(m) else {
            continue;
        };
        let mut entry = map_entry(m, category, synced_at);
        match prior.get(&m.id) {
            Some(p) if is_enriched(p) => carry_forward_enrichment(&mut entry, p),
            _ if enriched < enrich_max => {
                if let Some(enr) = page_client.as_ref().and_then(|c| enrich_from_web(c, &m.id)) {
                    apply_enrichment(&mut entry, &enr, &synced_at_str);
                    enriched += 1;
                }
            }
            _ => {}
        }
        if is_missing_size(entry.get("max_context_tokens")) {
            entry["max_context_tokens"] = json!(FALLBACK_CONTEXT_TOKENS);
        }
        entries.push(entry);
    }
    (entries, enriched)
}

/// Fetches the live OrcaRouter catalog, writes `{providers_dir}/orcarouter/models.json`, and
/// upserts it via `load_from_dir`. `disable_missing` is always `true` for the orcarouter brand —
/// staleness is the norm for an aggregator, same as openrouter/requesty/nousportal.
pub fn sync(
    storage: &dyn CatalogStorage,
    providers_dir: &str,
    base_url: &str,
    enrich_max: usize,
) -> Result<OrcaRouterSyncSummary, String> {
    let url = format!("{base_url}/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("orcarouter fetch failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("orcarouter fetch returned error status: {e}"))?;
    let parsed: ModelsResponse = resp
        .json()
        .map_err(|e| format!("failed to parse orcarouter response: {e}"))?;

    let fetched = parsed.data.len();
    if fetched < MIN_SANE_MODEL_COUNT {
        tracing::warn!(
            fetched,
            min = MIN_SANE_MODEL_COUNT,
            "orcarouter sync: suspiciously few models returned, skipping upsert"
        );
        return Ok(OrcaRouterSyncSummary {
            fetched,
            brands_added: 0,
            models_added: 0,
            models_updated: 0,
            models_disabled: 0,
            enriched: 0,
            skipped_suspicious: true,
        });
    }

    let prior = prior_entries(providers_dir);
    let (entries, enriched) = build_entries(&parsed.data, &prior, enrich_max);

    let dir = std::path::Path::new(providers_dir).join("orcarouter");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let models_path = dir.join("models.json");
    let body = serde_json::to_string_pretty(&entries)
        .map_err(|e| format!("failed to serialize models.json: {e}"))?;
    std::fs::write(&models_path, body)
        .map_err(|e| format!("failed to write {}: {e}", models_path.display()))?;

    let summary = load_from_dir(storage, providers_dir, true, true).map_err(|e| e.to_string())?;

    Ok(OrcaRouterSyncSummary {
        fetched,
        brands_added: summary.brands_added,
        models_added: summary.models_added,
        models_updated: summary.models_updated,
        models_disabled: summary.models_disabled,
        enriched,
        skipped_suspicious: false,
    })
}

/// Fetches and maps the catalog without touching disk or the DB — used by `--dry-run`. Still
/// performs web enrichment (up to `enrich_max`) so the previewed JSON matches what a real sync
/// would write; pass `enrich_max = 0` to skip it for a fast preview.
pub fn fetch_preview(base_url: &str, enrich_max: usize) -> Result<Vec<serde_json::Value>, String> {
    let url = format!("{base_url}/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("orcarouter fetch failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("orcarouter fetch returned error status: {e}"))?;
    let parsed: ModelsResponse = resp
        .json()
        .map_err(|e| format!("failed to parse orcarouter response: {e}"))?;
    let (entries, _) = build_entries(&parsed.data, &Default::default(), enrich_max);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(json: serde_json::Value) -> OrcaModel {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn classify_keeps_chat_and_embeddings_drops_the_rest() {
        assert_eq!(
            classify(&model(json!({
                "id": "qwen/qwen3.7-flash",
                "supported_endpoint_types": ["openai", "openai-response"],
                "pricing": {"prompt": "0.00000003", "completion": "0.00000013"}
            }))),
            Some("text")
        );
        // Empty endpoint types but token-priced → still a chat model.
        assert_eq!(
            classify(&model(json!({
                "id": "openai/gpt-oss-120b",
                "supported_endpoint_types": [],
                "pricing": {"prompt": "0.00000003", "completion": "0.00000017"}
            }))),
            Some("text")
        );
        assert_eq!(
            classify(&model(json!({
                "id": "openai/text-embedding-3-large",
                "supported_endpoint_types": ["embeddings"],
                "pricing": {"prompt": "0.00000013", "completion": "0.00000013"}
            }))),
            Some("embedding")
        );
        // Per-call video model.
        assert_eq!(
            classify(&model(json!({
                "id": "kling/kling-3-turbo",
                "supported_endpoint_types": [],
                "pricing": {"request": "0.112000"}
            }))),
            None
        );
        // Token-priced image model — caught by the id marker, not the pricing shape.
        assert_eq!(
            classify(&model(json!({
                "id": "openai/gpt-image-2",
                "supported_endpoint_types": [],
                "pricing": {"prompt": "0.000008", "completion": "0.00003"}
            }))),
            None
        );
        assert_eq!(classify(&model(json!({ "id": "orcarouter/auto" }))), None);
        assert_eq!(
            classify(&model(json!({
                "id": "deepseek/deepseek-v4-flash-free",
                "pricing": {"request": "0.000000"}
            }))),
            None
        );
    }

    #[test]
    fn map_entry_scales_price_and_defaults_capabilities_for_chat() {
        let m = model(json!({
            "id": "qwen/qwen3.7-flash",
            "name": "Qwen: Qwen3.7 Flash",
            "context_length": 1_000_000,
            "max_completion_tokens": 65_536,
            "pricing": {"prompt": "0.0000000300", "completion": "0.0000001300"}
        }));
        let out = map_entry(&m, "text", Utc::now());
        assert_eq!(out["slug"], "qwen/qwen3.7-flash");
        assert!((out["price_input_per_1m"].as_f64().unwrap() - 0.03).abs() < 1e-9);
        assert!((out["price_output_per_1m"].as_f64().unwrap() - 0.13).abs() < 1e-9);
        assert_eq!(out["max_context_tokens"], 1_000_000);
        assert_eq!(out["max_output_tokens"], 65_536);
        assert_eq!(out["supports_function_calling"], true);
        assert_eq!(out["supports_json_mode"], true);
        assert!(out["price_cached_input_per_1m"].is_null());
        assert_eq!(out["trains_on_data"], true);
        assert_eq!(out["retains_data"], true);
    }

    #[test]
    fn map_entry_treats_zero_context_as_missing() {
        let m = model(json!({
            "id": "openai/gpt-5.5",
            "context_length": 0,
            "max_completion_tokens": 0,
            "pricing": {"prompt": "0.000001", "completion": "0.000002"}
        }));
        let out = map_entry(&m, "text", Utc::now());
        assert!(out["max_context_tokens"].is_null());
        assert!(out["max_output_tokens"].is_null());
    }

    #[test]
    fn apply_enrichment_overrides_flags_and_adds_cache_price() {
        let m = model(json!({
            "id": "x/y",
            "pricing": {"prompt": "0.000001", "completion": "0.000002"}
        }));
        let mut out = map_entry(&m, "text", Utc::now());
        let enr = WebEnrichment {
            supports_function_calling: Some(false),
            supports_json_mode: Some(true),
            price_cached_input_per_1m: Some(0.006),
            max_context_tokens: Some(128_000),
            max_output_tokens: None,
        };
        apply_enrichment(&mut out, &enr, "2026-09-03T00:00:00+00:00");
        assert_eq!(out["supports_function_calling"], false);
        assert_eq!(out["supports_json_mode"], true);
        assert!((out["price_cached_input_per_1m"].as_f64().unwrap() - 0.006).abs() < 1e-9);
        assert_eq!(out["max_context_tokens"], 128_000);
        assert_eq!(out["enriched_at"], "2026-09-03T00:00:00+00:00");
    }

    #[test]
    fn carry_forward_keeps_scraped_fields_on_resync() {
        let m = model(json!({
            "id": "x/y",
            "pricing": {"prompt": "0.000001", "completion": "0.000002"}
        }));
        let mut fresh = map_entry(&m, "text", Utc::now());
        let prior = json!({
            "slug": "x/y",
            "supports_function_calling": false,
            "supports_json_mode": false,
            "price_cached_input_per_1m": 0.02,
            "enriched_at": "2026-09-01T00:00:00+00:00",
        });
        assert!(is_enriched(&prior));
        carry_forward_enrichment(&mut fresh, &prior);
        assert_eq!(fresh["supports_function_calling"], false);
        assert_eq!(fresh["price_cached_input_per_1m"], 0.02);
        assert_eq!(fresh["enriched_at"], "2026-09-01T00:00:00+00:00");
        // API price still wins over any stale prior value.
        assert!((fresh["price_input_per_1m"].as_f64().unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn embeddings_get_no_capability_flags() {
        let m = model(json!({
            "id": "openai/text-embedding-3-large",
            "pricing": {"prompt": "0.00000013", "completion": "0.00000013"}
        }));
        let out = map_entry(&m, "embedding", Utc::now());
        assert_eq!(out["supports_function_calling"], false);
        assert_eq!(out["supports_json_mode"], false);
        assert_eq!(out["category"], "embedding");
    }

    /// Tiered detail-page layout: label pair, then a 4-value row — cache read is index 2.
    #[test]
    fn parses_cache_price_from_tiered_page() {
        let text = "Copy Supported parameters include_reasoning logprobs reasoning \
            response_format seed temperature tool_choice tools top_p Pricing Tier \
            Input / 1M tokens Output / 1M tokens Cache read / 1M Cache write / 1M \
            ≤ 32K $0.030 $0.130 $0.0060 $0.038 ≤ 256K $0.100 $0.400 $0.020 $0.125";
        assert_eq!(parse_cache_read_price(text), Some(0.0060));

        let ps = text.find("Supported parameters").unwrap();
        let end = ps + text[ps..].find("Pricing").unwrap();
        let params = &text[ps..end];
        assert!(params.contains(" tools"));
        assert!(params.contains("response_format"));
    }

    /// Flat detail-page layout: each value sits directly after its own label.
    #[test]
    fn parses_cache_price_from_flat_page() {
        let text = "Pricing Input / 1M tokens $10.00 Output / 1M tokens $50.00 \
            Cache read / 1M $1.00 Cache write / 1M $12.50 Currency USD Cost calculator \
            Estimated / month $220 With prompt caching ≈ $189";
        assert_eq!(parse_cache_read_price(text), Some(1.00));
    }

    #[test]
    fn parses_context_from_ld_json() {
        let text = r#"{"@type":"PropertyValue","name":"contextWindow","value":1000000},{"@type":"PropertyValue","name":"maxOutputTokens","value":65536}"#;
        assert_eq!(ld_json_property(text, "contextWindow"), Some(1_000_000));
        assert_eq!(ld_json_property(text, "maxOutputTokens"), Some(65_536));
    }

    #[test]
    fn flatten_html_strips_tags_scripts_and_collapses_space() {
        assert_eq!(flatten_html("<div>  a <span>b</span>\n  c </div>"), "a b c");
        // The inline i18n bundle (which defines the label text we grep for) must not leak.
        let html = r#"<head><script>var t={"cache_read_per_million":"Cache read / 1M"};</script></head>
            <body><h2>Supported parameters</h2> tools response_format <div>Pricing</div>
            <style>.x{content:"Cache read / 1M"}</style></body>"#;
        let flat = flatten_html(html);
        assert!(!flat.contains("cache_read_per_million"));
        assert_eq!(flat.matches("Cache read / 1M").count(), 0);
        assert!(flat.contains("Supported parameters tools response_format Pricing"));
    }
}
