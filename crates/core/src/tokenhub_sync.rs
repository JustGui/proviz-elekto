//! Fetches TokenHub's model roster and upserts it into the local provider directory + DB via
//! `builtin_providers::load_from_dir` — same generated-`models.json` workflow as the other
//! aggregators (`openrouter_sync` / `orcarouter_sync`).
//!
//! **Why this is its own module, not a parameterisation of `openrouter_sync`:**
//!
//!   1. **The roster endpoint is nearly data-free.** `GET {base_url}/models` (which needs a
//!      `Authorization: Bearer $TOKENHUB_API_KEY` — the first sync that does) returns only
//!      `{id, object, created, owned_by, supported_endpoint_types}` per model — **no pricing, no
//!      context window, no capability flags**. Everything the selector needs lives on the
//!      server-rendered model detail page `https://tokenhub.com/models/{slug}` (slug = the id
//!      lower-cased with `.` → `-`), embedded as a JSON blob in the Next.js RSC payload. So a
//!      newly-seen model is **scraped once** (`scrape_model`) for its full data — capped per run
//!      (`PROVIZ_TOKENHUB_ENRICH_MAX`, default 30), fail-soft (skipped and retried next sync).
//!      Models already in the on-disk `models.json` are carried forward verbatim and never
//!      re-scraped — the roster endpoint carries nothing to refresh them with. A price refresh is
//!      a manual `proviz providers sync-tokenhub --refresh` (re-scrapes everything).
//!   2. **Prices are one-api-style ratios, not dollar strings.** The detail-page object gives
//!      `model_ratio` / `completion_ratio` / `cache_ratio`. TokenHub's base unit is
//!      `$2.00 / 1M tokens` per ratio point (verified against their public pricing page — GLM
//!      `model_ratio 0.05714` → $0.1143/M in, GPT-4o `1.25` → $2.50/M in, ×`completion_ratio`
//!      for output, ×`cache_ratio` for cached input).
//!   3. **Mixed catalog.** The roster lists chat, image-generation, and `generation-task` models
//!      together; `keep_for_sync` keeps chat models (support the `openai` endpoint, id carries no
//!      image/audio/video marker) and drops the rest.
//!
//! TokenHub is an OpenRouter-style gateway on the **response** side — chat responses carry
//! `usage.cost` (real per-request cost), `usage.cost_details.upstream_inference_cost`, and
//! `usage.prompt_tokens_details.cached_tokens`. `server/src/complete.rs` already reads all three
//! generically (the `usage.cost` fast-path is only excluded for `nousportal`), so no brand-gated
//! code is needed there. On the **request** side it is a plain OpenAI-compatible provider: unlike
//! OpenRouter/OrcaRouter it gets **no** reasoning-disable injection — `reasoning: {enabled:false}`
//! is translated to `reasoning_effort: "none"` upstream, which OpenAI o-series models reject with
//! a 400. If a specific TokenHub model shows runaway hidden-trace behaviour, set
//! `reasoning_effort_value` on its catalog row by hand (does not survive a re-seed — same caveat
//! as any hand-edit to a generated `models.json`).

use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::{builtin_providers::load_from_dir, storage::CatalogStorage};

/// `{DEFAULT_BASE_URL}/models` is the roster endpoint; it is also the brand's chat `base_url`.
pub const DEFAULT_BASE_URL: &str = "https://us-api.tokenhub.com/v1";

/// Server-rendered model detail pages live on the marketing host (not the API host).
/// `scrape_model` fetches `{MODEL_PAGE_BASE}/{slug}` for everything the roster omits.
const MODEL_PAGE_BASE: &str = "https://tokenhub.com/models";

/// Env var holding the API key sent as `Authorization: Bearer` on the roster fetch. Matches
/// `providers/tokenhub/brand.json`'s `api_key_env`.
const API_KEY_ENV: &str = "TOKENHUB_API_KEY";

/// TokenHub bills `model_ratio` × this many USD per 1M tokens (one-api convention, `$0.002/1K`).
const PRICE_PER_RATIO_UNIT_USD: f64 = 2.0;

/// Default cap on how many newly-seen models to scrape per sync run. `PROVIZ_TOKENHUB_ENRICH_MAX`
/// overrides it; the CLI passes a large value for a one-shot manual seed.
pub const DEFAULT_ENRICH_MAX: usize = 30;

/// Below this many models the roster fetch is treated as suspect (truncated/empty/partial-auth)
/// and the whole sync is skipped rather than upserted — protects `disable_missing` from
/// mass-disabling the catalog. TokenHub has consistently listed 90+ models.
const MIN_SANE_MODEL_COUNT: usize = 40;

/// Fallback `max_context_tokens` when the detail page yields none — without it a row lands at `0`
/// and the selector's context-fit filter drops it from every pool. Deferred to the moment a real
/// value appears.
const FALLBACK_CONTEXT_TOKENS: u64 = 16_384;

/// id fragments marking a non-chat model (image / audio / video generation).
const NON_CHAT_ID_MARKERS: &[&str] = &[
    "image", "imagen", "dall-e", "tts", "whisper", "sora", "veo", "seedance", "-video", "-dub",
];

/// Reads `PROVIZ_TOKENHUB_ENRICH_MAX`, falling back to `DEFAULT_ENRICH_MAX`.
pub fn enrich_max_from_env() -> usize {
    std::env::var("PROVIZ_TOKENHUB_ENRICH_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_ENRICH_MAX)
}

pub struct TokenHubSyncSummary {
    pub fetched: usize,
    pub brands_added: usize,
    pub models_added: usize,
    pub models_updated: usize,
    pub models_disabled: usize,
    /// How many newly-seen (or, under `--refresh`, all) models were scraped this run.
    pub enriched: usize,
    pub skipped_suspicious: bool,
}

#[derive(Deserialize)]
struct RosterResponse {
    data: Vec<RosterEntry>,
}

#[derive(Deserialize)]
struct RosterEntry {
    id: String,
    /// An explicit `null` appears on some rows, so `Option<Vec<_>>` not `#[serde(default)] Vec<_>`.
    supported_endpoint_types: Option<Vec<String>>,
}

impl RosterEntry {
    fn endpoint_types(&self) -> &[String] {
        self.supported_endpoint_types.as_deref().unwrap_or(&[])
    }
}

/// True for a roster entry the selector can route to: reachable over the OpenAI-compatible
/// `/v1/chat/completions` path and not an image/audio/video model.
fn keep_for_sync(e: &RosterEntry) -> bool {
    if !e.endpoint_types().iter().any(|t| t == "openai") {
        return false;
    }
    let id = e.id.to_ascii_lowercase();
    !NON_CHAT_ID_MARKERS.iter().any(|m| id.contains(m))
}

/// id (`gpt-5.6-sol`, `MiniMax-M3`) → detail-page slug (`gpt-5-6-sol`, `minimax-m3`).
fn detail_slug(id: &str) -> String {
    id.to_ascii_lowercase().replace('.', "-")
}

/// TokenHub quotes ratios as imprecise floats (`3.50000013`), so a derived price like
/// `0.1142857 × 3.5` lands at `0.399999964…`. Round to 6 decimals ($1e-6/1M granularity) to keep
/// the committed `models.json` readable without affecting any real cost figure.
fn round_price(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

fn nonzero_u64(v: Option<u64>) -> Option<u64> {
    v.filter(|n| *n > 0)
}

/// Maps one scraped detail-page `model` object to the on-disk `models.json` entry shape.
fn map_model(m: &serde_json::Value, synced_at: &str) -> serde_json::Value {
    let base = &m["extra_model_base"];
    // Derive output/cached from the *unrounded* input rate so their own rounding lands cleanly
    // (e.g. `0.1142857 × 3.5` → `0.4`, not `0.400001` off a pre-rounded input).
    let input_raw = m["model_ratio"]
        .as_f64()
        .filter(|r| *r > 0.0)
        .map(|r| r * PRICE_PER_RATIO_UNIT_USD);
    let input = input_raw.map(round_price);
    let output = match (input_raw, m["completion_ratio"].as_f64()) {
        (Some(i), Some(c)) if c > 0.0 => Some(round_price(i * c)),
        _ => None,
    };
    let cached = match (input_raw, m["cache_ratio"].as_f64()) {
        (Some(i), Some(c)) if c > 0.0 => Some(round_price(i * c)),
        _ => None,
    };
    let ctx = nonzero_u64(m["total_context"].as_u64())
        .or_else(|| nonzero_u64(base["limit"]["context"].as_u64()))
        .unwrap_or(FALLBACK_CONTEXT_TOKENS);
    let max_out = nonzero_u64(m["max_output"].as_u64())
        .or_else(|| nonzero_u64(base["limit"]["output"].as_u64()));
    // OpenAI-compatible gateway — `tool_call` is reported per model; assume supported when the key
    // is absent. No JSON-mode signal exists, so default `true` for every chat model (mirrors
    // orcarouter_sync).
    let tool_call = base["tool_call"].as_bool().unwrap_or(true);

    json!({
        "slug": m["model_name"].as_str().unwrap_or_default(),
        "display_name": m["display_name"].as_str(),
        "max_context_tokens": ctx,
        "max_output_tokens": max_out,
        "supports_function_calling": tool_call,
        "supports_json_mode": true,
        "price_input_per_1m": input,
        "price_output_per_1m": output,
        "price_cached_input_per_1m": cached,
        "category": "text",
        // TokenHub routes to upstream providers under *their* training/retention terms, which the
        // API never names — assume the worst, keeping TokenHub out of `require_no_training` steps.
        "trains_on_data": true,
        "retains_data": true,
        "price_synced_at": synced_at,
        "enriched_at": synced_at,
    })
}

/// Un-escapes one level of JS/JSON string escaping from an RSC `self.__next_f.push([1,"…"])`
/// chunk: `\"` `\\` `\/` `\n` `\t` `\r` `\b` `\f` `\uXXXX`. A truncated trailing escape is dropped.
fn unescape_rsc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => break,
        }
    }
    out
}

/// Returns the balanced `{…}` slice starting at byte `start` (which must index a `{`), tracking
/// JSON string state so a brace inside a string value doesn't miscount.
fn balanced_object(s: &str, start: usize) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return s.get(start..=i);
                }
            }
            _ => {}
        }
    }
    None
}

/// How much raw HTML to un-escape after the `\"model\":{` anchor. The largest observed model
/// object (with its `extra_model_benchmark` array) is ~14 KB un-escaped; 96 KB of still-escaped
/// source is a generous margin.
const SCRAPE_WINDOW_BYTES: usize = 96 * 1024;

/// Fetches `{MODEL_PAGE_BASE}/{slug}` and extracts the embedded `"model": {…}` object. `None` on a
/// network error, or when the page came back without the anchor (a pre-hydration shell) — the
/// caller then leaves the model un-scraped and retries it on a later sync.
fn scrape_model(client: &reqwest::blocking::Client, slug: &str) -> Option<serde_json::Value> {
    let url = format!("{MODEL_PAGE_BASE}/{slug}");
    let body = client
        .get(&url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|e| tracing::debug!(slug, %e, "tokenhub scrape: page fetch failed"))
        .ok()?;

    // The RSC payload contains `...,{\"model\":{\"id\":N,\"model_name\":...`.
    let anchor = "\\\"model\\\":{";
    let anchor_at = body.find(anchor).or_else(|| {
        tracing::debug!(
            slug,
            "tokenhub scrape: no model anchor (SSR shell) — retry next sync"
        );
        None
    })?;
    let brace_at = anchor_at + anchor.len() - 1; // index of the '{'
    let window_end = (brace_at + SCRAPE_WINDOW_BYTES).min(body.len());
    let window = body.get(brace_at..window_end)?;
    let decoded = unescape_rsc(window);
    let obj = balanced_object(&decoded, 0)?;
    match serde_json::from_str::<serde_json::Value>(obj) {
        Ok(v) if v.get("model_name").and_then(|n| n.as_str()).is_some() => Some(v),
        Ok(_) => {
            tracing::debug!(slug, "tokenhub scrape: model object missing model_name");
            None
        }
        Err(e) => {
            tracing::debug!(slug, %e, "tokenhub scrape: model object parse failed");
            None
        }
    }
}

fn is_enriched(entry: &serde_json::Value) -> bool {
    entry
        .get("enriched_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
}

/// The previous `{providers_dir}/tokenhub/models.json` as a slug → entry map.
fn prior_entries(providers_dir: &str) -> std::collections::HashMap<String, serde_json::Value> {
    let path = std::path::Path::new(providers_dir)
        .join("tokenhub")
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

/// Builds the `models.json` entries. A model already present and enriched in `prior` is carried
/// forward verbatim (the roster carries nothing to refresh it with) unless `refresh` is set;
/// otherwise it is scraped now, up to `enrich_max` scrapes this run. Shared by `sync` and
/// `fetch_preview`.
fn build_entries(
    roster: &[RosterEntry],
    prior: &std::collections::HashMap<String, serde_json::Value>,
    enrich_max: usize,
    refresh: bool,
) -> (Vec<serde_json::Value>, usize) {
    let synced_at = Utc::now().to_rfc3339();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .user_agent("proviz-elekto-sync")
        .build()
        .ok();

    let mut entries = Vec::new();
    let mut enriched = 0usize;
    for r in roster {
        if !keep_for_sync(r) {
            continue;
        }
        let known = prior.get(&r.id).filter(|p| is_enriched(p));
        if let Some(p) = known {
            if !refresh {
                entries.push(p.clone());
                continue;
            }
        }
        if enriched < enrich_max {
            if let Some(obj) = client
                .as_ref()
                .and_then(|c| scrape_model(c, &detail_slug(&r.id)))
            {
                entries.push(map_model(&obj, &synced_at));
                enriched += 1;
                continue;
            }
            // Scrape failed — keep a known entry rather than losing it; a brand-new model is
            // skipped and retried next sync.
            if let Some(p) = known {
                entries.push(p.clone());
            }
        } else if let Some(p) = known {
            entries.push(p.clone());
        }
    }
    (entries, enriched)
}

fn fetch_roster(base_url: &str) -> Result<Vec<RosterEntry>, String> {
    let key = std::env::var(API_KEY_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| format!("{API_KEY_ENV} not set — cannot fetch TokenHub roster"))?;
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let resp = client
        .get(&url)
        .bearer_auth(key)
        .send()
        .map_err(|e| format!("tokenhub roster fetch failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("tokenhub roster fetch returned error status: {e}"))?;
    let parsed: RosterResponse = resp
        .json()
        .map_err(|e| format!("failed to parse tokenhub roster: {e}"))?;
    Ok(parsed.data)
}

/// Fetches the roster, writes `{providers_dir}/tokenhub/models.json`, and upserts via
/// `load_from_dir`. `disable_missing` is always `true` for the tokenhub brand — staleness is the
/// norm for an aggregator, same as the other synced providers.
pub fn sync(
    storage: &dyn CatalogStorage,
    providers_dir: &str,
    base_url: &str,
    enrich_max: usize,
    refresh: bool,
) -> Result<TokenHubSyncSummary, String> {
    let roster = fetch_roster(base_url)?;
    let fetched = roster.len();
    if fetched < MIN_SANE_MODEL_COUNT {
        tracing::warn!(
            fetched,
            min = MIN_SANE_MODEL_COUNT,
            "tokenhub sync: suspiciously few models returned, skipping upsert"
        );
        return Ok(TokenHubSyncSummary {
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
    let (entries, enriched) = build_entries(&roster, &prior, enrich_max, refresh);

    let dir = std::path::Path::new(providers_dir).join("tokenhub");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let models_path = dir.join("models.json");
    let body = serde_json::to_string_pretty(&entries)
        .map_err(|e| format!("failed to serialize models.json: {e}"))?;
    std::fs::write(&models_path, body)
        .map_err(|e| format!("failed to write {}: {e}", models_path.display()))?;

    let summary = load_from_dir(storage, providers_dir, true, true).map_err(|e| e.to_string())?;

    Ok(TokenHubSyncSummary {
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
/// scrapes (up to `enrich_max`) so the preview matches what a real sync would write; pass
/// `enrich_max = 0` for a fast preview of the roster filter only.
pub fn fetch_preview(base_url: &str, enrich_max: usize) -> Result<Vec<serde_json::Value>, String> {
    let roster = fetch_roster(base_url)?;
    let (entries, _) = build_entries(&roster, &Default::default(), enrich_max, true);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster(json: serde_json::Value) -> RosterEntry {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn keep_for_sync_filters_non_openai_and_media_models() {
        assert!(keep_for_sync(&roster(json!({
            "id": "gpt-5.6-sol", "supported_endpoint_types": ["openai"]
        }))));
        assert!(keep_for_sync(&roster(json!({
            "id": "claude-sonnet-5", "supported_endpoint_types": ["openai", "anthropic"]
        }))));
        // Anthropic-only endpoint → not reachable over /v1/chat/completions.
        assert!(!keep_for_sync(&roster(json!({
            "id": "claude-fable-5.1", "supported_endpoint_types": ["anthropic"]
        }))));
        // Image model, even though it lists the openai endpoint.
        assert!(!keep_for_sync(&roster(json!({
            "id": "gpt-image-2", "supported_endpoint_types": ["openai"]
        }))));
        assert!(!keep_for_sync(&roster(json!({
            "id": "gpt-5.4-image-2", "supported_endpoint_types": ["openai"]
        }))));
        // Explicit null endpoint list must not panic.
        assert!(!keep_for_sync(&roster(json!({
            "id": "x", "supported_endpoint_types": null
        }))));
    }

    #[test]
    fn detail_slug_lowercases_and_dashes() {
        assert_eq!(detail_slug("gpt-5.6-sol"), "gpt-5-6-sol");
        assert_eq!(detail_slug("claude-fable-5.1"), "claude-fable-5-1");
        assert_eq!(detail_slug("MiniMax-M3"), "minimax-m3");
        assert_eq!(detail_slug("qwen3.8-max"), "qwen3-8-max");
    }

    #[test]
    fn map_model_scales_ratio_pricing() {
        // GPT-4o: model_ratio 1.25 → $2.50/M in, ×4 → $10/M out, ×0.5 → $1.25/M cached.
        let m = json!({
            "model_name": "gpt-4o",
            "display_name": "GPT-4o",
            "total_context": 128000,
            "max_output": 16384,
            "model_ratio": 1.25,
            "completion_ratio": 4,
            "cache_ratio": 0.5,
            "extra_model_base": { "tool_call": true, "limit": { "context": 128000, "output": 16384 } }
        });
        let out = map_model(&m, "2026-09-04T00:00:00+00:00");
        assert_eq!(out["slug"], "gpt-4o");
        assert!((out["price_input_per_1m"].as_f64().unwrap() - 2.5).abs() < 1e-9);
        assert!((out["price_output_per_1m"].as_f64().unwrap() - 10.0).abs() < 1e-9);
        assert!((out["price_cached_input_per_1m"].as_f64().unwrap() - 1.25).abs() < 1e-9);
        assert_eq!(out["max_context_tokens"], 128000);
        assert_eq!(out["max_output_tokens"], 16384);
        assert_eq!(out["supports_function_calling"], true);
        assert_eq!(out["supports_json_mode"], true);
        assert_eq!(out["category"], "text");
        assert_eq!(out["trains_on_data"], true);
        assert_eq!(out["retains_data"], true);
    }

    #[test]
    fn map_model_handles_missing_ratios_and_context() {
        let m = json!({
            "model_name": "mystery",
            "model_ratio": 0,
            "completion_ratio": 0,
            "cache_ratio": 0,
            "extra_model_base": {}
        });
        let out = map_model(&m, "t");
        assert!(out["price_input_per_1m"].is_null());
        assert!(out["price_output_per_1m"].is_null());
        assert!(out["price_cached_input_per_1m"].is_null());
        assert!(out["max_output_tokens"].is_null());
        assert_eq!(out["max_context_tokens"], FALLBACK_CONTEXT_TOKENS);
        // tool_call absent → assumed supported.
        assert_eq!(out["supports_function_calling"], true);
    }

    #[test]
    fn map_model_falls_back_to_extra_model_base_limits() {
        let m = json!({
            "model_name": "x",
            "model_ratio": 1,
            "completion_ratio": 2,
            "extra_model_base": { "limit": { "context": 200000, "output": 64000 }, "tool_call": false }
        });
        let out = map_model(&m, "t");
        assert_eq!(out["max_context_tokens"], 200000);
        assert_eq!(out["max_output_tokens"], 64000);
        assert_eq!(out["supports_function_calling"], false);
    }

    #[test]
    fn unescape_rsc_handles_common_escapes() {
        assert_eq!(unescape_rsc(r#"\"a\": 1"#), r#""a": 1"#);
        assert_eq!(unescape_rsc(r#"a\\b\/c"#), r"a\b/c");
        assert_eq!(unescape_rsc(r#"x&y"#), "x&y");
        // Truncated trailing escape is dropped, not panicked on.
        assert_eq!(unescape_rsc(r"done\"), "done");
    }

    #[test]
    fn balanced_object_respects_strings() {
        let s = r#"{"a":{"b":"}{"},"c":1} trailing"#;
        assert_eq!(balanced_object(s, 0).unwrap(), r#"{"a":{"b":"}{"},"c":1}"#);
    }

    #[test]
    fn scrape_pipeline_extracts_model_object_from_rsc_fragment() {
        // A minimal stand-in for the real page's `self.__next_f.push([1,"…\"model\":{…}…"])`.
        let raw = r#"prefix,{\"model\":{\"id\":47,\"model_name\":\"gpt-4o\",\"display_name\":\"GPT-4o\",\"model_ratio\":1.25,\"completion_ratio\":4,\"cache_ratio\":0.5,\"extra_model_base\":{\"tool_call\":true,\"limit\":{\"context\":128000,\"output\":16384}}},\"other\":1}"#;
        let anchor = "\\\"model\\\":{";
        let brace_at = raw.find(anchor).unwrap() + anchor.len() - 1;
        let decoded = unescape_rsc(&raw[brace_at..]);
        let obj = balanced_object(&decoded, 0).unwrap();
        let v: serde_json::Value = serde_json::from_str(obj).unwrap();
        assert_eq!(v["model_name"], "gpt-4o");
        let mapped = map_model(&v, "t");
        assert!((mapped["price_input_per_1m"].as_f64().unwrap() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn build_entries_carries_known_models_forward_without_refresh() {
        let roster = vec![roster(json!({
            "id": "gpt-4o", "supported_endpoint_types": ["openai"]
        }))];
        let mut prior = std::collections::HashMap::new();
        prior.insert(
            "gpt-4o".to_string(),
            json!({ "slug": "gpt-4o", "price_input_per_1m": 2.5, "enriched_at": "2026-01-01T00:00:00+00:00" }),
        );
        let (entries, enriched) = build_entries(&roster, &prior, 30, false);
        assert_eq!(enriched, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["enriched_at"], "2026-01-01T00:00:00+00:00");
    }
}
