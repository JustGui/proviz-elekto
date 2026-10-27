//! Integration test for POST /complete.
//!
//! Spins up two in-process servers:
//!   1. a mock OpenAI-compatible provider (returns a fixed chat-completion + usage + rate-limit headers)
//!   2. the proviz-server router backed by an in-memory SQLite catalog whose single brand points at
//!      the mock provider's base_url
//!
//! Then POSTs /complete and asserts the server selected a model, called the provider, parsed the
//! text + usage + cost, and reported success (verified by hitting /complete a second time and
//! observing the request still succeeds, plus checking the returned token/cost values).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{routing::post, Json, Router};
use chrono::Utc;
use proviz_elekto_core::{
    models::{Brand, Model, SelectionRule},
    selector::Selector,
    storage::CatalogStorage,
};
use proviz_elekto_storage_sqlite::SqliteStorage;
use proviz_server::{batch, build_router, AppState};
use serde_json::{json, Value};
use uuid::Uuid;

const API_KEY_ENV: &str = "PROVIZ_TEST_COMPLETE_KEY";

/// Mock provider: responds to POST /v1/chat/completions with a fixed assistant message,
/// token usage, and OpenAI-style rate-limit headers.
async fn mock_chat_completions() -> impl axum::response::IntoResponse {
    let body = json!({
        "id": "chatcmpl-test",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hello from mock" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18 }
    });
    (
        [
            ("x-ratelimit-remaining-requests", "42"),
            ("x-ratelimit-remaining-tokens", "9000"),
        ],
        Json(body),
    )
}

async fn spawn_mock_provider() -> String {
    let app = Router::new().route("/v1/chat/completions", post(mock_chat_completions));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/v1")
}

fn seed_catalog(base_url: String) -> Arc<Selector> {
    seed_typed_catalog(base_url, "mockbrand", None)
}

fn seed_typed_catalog(base_url: String, slug: &str, category: Option<&str>) -> Arc<Selector> {
    let storage = SqliteStorage::open_in_memory().expect("in-memory db");
    let brand = Brand {
        id: Uuid::new_v4(),
        slug: slug.into(),
        name: "Mock Brand".into(),
        base_url: Some(base_url),
        is_active: true,
        priority: 0,
        created_at: Utc::now(),
        traffic_weight: 1.0,
        endpoints: if slug == "typesafe" {
            Some(json!({"chat":"/systemone"}))
        } else {
            None
        },
        price_currency: "USD".into(),
    };
    let model = Model {
        max_in_flight: None,
        id: Uuid::new_v4(),
        brand_id: brand.id,
        slug: "mock-7b".into(),
        display_name: "Mock 7B".into(),
        max_context_tokens: 32_000,
        max_output_tokens: None,
        supports_function_calling: true,
        supports_json_mode: true,
        reasoning_effort_value: None,
        price_input_per_1m: Some(1.0),
        price_output_per_1m: Some(2.0),
        tpm_limit: None,
        rpm_limit: None,
        rpd_limit: None,
        tpd_limit: None,
        tpm_limit_month: None,
        rps_limit: None,
        quality_score: Some(0.8),
        avg_latency_ms: None,
        is_enabled: true,
        notes: None,
        category: category.map(str::to_string),
        created_at: Utc::now(),
        batch_price_multiplier: None,
        diarization: None,
        streaming: None,
        http_batch: None,
        word_timestamps: None,
        base_url: None,
        supported_languages: None,
        canonical_key: None,
        price_synced_at: None,
        trains_on_data: None,
        retains_data: None,
        price_cached_input_per_1m: None,
    };
    let rule = SelectionRule {
        id: Uuid::new_v4(),
        step: "chat".into(),
        model_id: model.id,
        priority: 0,
        max_ctx_tokens: None,
        requires_fn_call: false,
        is_enabled: true,
    };
    storage.insert_brand(&brand).unwrap();
    storage.insert_model(&model).unwrap();
    storage.insert_rule(&rule).unwrap();
    let selector = Arc::new(Selector::new(Arc::new(storage)));
    selector.reload().unwrap();
    selector
}

async fn spawn_proviz_server(selector: Arc<Selector>) -> String {
    let http = reqwest::Client::new();
    let batch_queue = Arc::new(batch::BatchQueue::new(60, 100, "http://localhost".into()));
    let state = Arc::new(AppState {
        selector,
        batch_queue,
        started_at: Instant::now(),
        providers_dir: ".".into(),
        http,
    });
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn complete_selects_calls_and_reports() {
    std::env::set_var(API_KEY_ENV, "secret-test-key");

    // We must register the brand's api_key_env. The seed uses brand.api_key_env via brand_api_keys?
    // Legacy single-key brands resolve api_key_env from the brand row — but Brand has no api_key_env
    // field in this catalog; the candidate's api_key_env comes from pz_brand_api_keys. Register one.
    let base_url = spawn_mock_provider().await;
    let selector = seed_catalog(base_url);

    // Attach an API key to the brand so the candidate carries api_key_env.
    {
        use proviz_elekto_core::models::BrandApiKey;
        let storage = selector.storage();
        let brands = storage.load_brands().unwrap();
        let brand = brands.iter().find(|b| b.slug == "mockbrand").unwrap();
        storage
            .insert_brand_api_key(&BrandApiKey {
                id: Uuid::new_v4(),
                brand_id: brand.id,
                api_key_env: API_KEY_ENV.into(),
                priority: 0,
                is_active: true,
                created_at: Utc::now(),
            })
            .unwrap();
        selector.reload().unwrap();
    }

    let server_url = spawn_proviz_server(selector).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{server_url}/complete"))
        .json(&json!({
            "step": "chat",
            "estimated_tokens": 50,
            "messages": [
                { "role": "user", "content": "say hello" }
            ]
        }))
        .send()
        .await
        .expect("request sent");

    assert_eq!(resp.status(), reqwest::StatusCode::OK, "expected 200");
    let body: Value = resp.json().await.unwrap();

    assert_eq!(body["text"], "hello from mock");
    assert_eq!(body["model"], "mock-7b");
    assert_eq!(body["brand"], "mockbrand");
    assert_eq!(body["prompt_tokens"], 11);
    assert_eq!(body["completion_tokens"], 7);
    // cost = (1.0 * 11 + 2.0 * 7) / 1e6 = 25 / 1e6
    let cost = body["cost_usd"].as_f64().expect("cost present");
    assert!((cost - 25.0 / 1_000_000.0).abs() < 1e-12, "cost was {cost}");
    assert!(body["tool_calls"].is_null());
}

/// Mock provider that reports a prompt-cache hit the way Nous Portal / OpenAI do:
/// `usage.prompt_tokens_details.cached_tokens`, and no `usage.cost` (so the server must fall back
/// to the cached-aware catalog computation).
async fn mock_chat_completions_cached() -> impl axum::response::IntoResponse {
    Json(json!({
        "id": "chatcmpl-cached",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "cached hello" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 10,
            "total_tokens": 1010,
            "prompt_tokens_details": { "cached_tokens": 800 }
        }
    }))
}

#[tokio::test]
async fn complete_discounts_cached_prompt_tokens_in_cost() {
    const KEY: &str = "PROVIZ_TEST_CACHED_KEY";
    std::env::set_var(KEY, "secret");

    // Mock provider.
    let app = Router::new().route("/v1/chat/completions", post(mock_chat_completions_cached));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base_url = format!("http://{addr}/v1");

    // Catalog: one model, input $1/M, output $2/M, cached input $0.10/M.
    let storage = SqliteStorage::open_in_memory().unwrap();
    let brand = Brand {
        id: Uuid::new_v4(),
        slug: "cachebrand".into(),
        name: "Cache Brand".into(),
        base_url: Some(base_url),
        is_active: true,
        priority: 0,
        created_at: Utc::now(),
        traffic_weight: 1.0,
        endpoints: None,
        price_currency: "USD".into(),
    };
    let model = Model {
        max_in_flight: None,
        id: Uuid::new_v4(),
        brand_id: brand.id,
        slug: "cache-7b".into(),
        display_name: "Cache 7B".into(),
        max_context_tokens: 32_000,
        max_output_tokens: None,
        supports_function_calling: true,
        supports_json_mode: true,
        reasoning_effort_value: None,
        price_input_per_1m: Some(1.0),
        price_output_per_1m: Some(2.0),
        tpm_limit: None,
        rpm_limit: None,
        rpd_limit: None,
        tpd_limit: None,
        tpm_limit_month: None,
        rps_limit: None,
        quality_score: Some(0.8),
        avg_latency_ms: None,
        is_enabled: true,
        notes: None,
        category: None,
        created_at: Utc::now(),
        batch_price_multiplier: None,
        diarization: None,
        streaming: None,
        http_batch: None,
        word_timestamps: None,
        base_url: None,
        supported_languages: None,
        canonical_key: None,
        price_synced_at: None,
        trains_on_data: None,
        retains_data: None,
        price_cached_input_per_1m: Some(0.10),
    };
    let rule = SelectionRule {
        id: Uuid::new_v4(),
        step: "chat".into(),
        model_id: model.id,
        priority: 0,
        max_ctx_tokens: None,
        requires_fn_call: false,
        is_enabled: true,
    };
    storage.insert_brand(&brand).unwrap();
    storage.insert_model(&model).unwrap();
    storage.insert_rule(&rule).unwrap();
    {
        use proviz_elekto_core::models::BrandApiKey;
        storage
            .insert_brand_api_key(&BrandApiKey {
                id: Uuid::new_v4(),
                brand_id: brand.id,
                api_key_env: KEY.into(),
                priority: 0,
                is_active: true,
                created_at: Utc::now(),
            })
            .unwrap();
    }
    let selector = Arc::new(Selector::new(Arc::new(storage)));
    selector.reload().unwrap();

    let server_url = spawn_proviz_server(selector).await;
    let resp = reqwest::Client::new()
        .post(format!("{server_url}/complete"))
        .json(&json!({
            "step": "chat",
            "estimated_tokens": 1000,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert_eq!(
        body["cached_tokens"], 800,
        "cached_tokens surfaced in response"
    );
    // 200 uncached @ $1/M + 800 cached @ $0.10/M + 10 output @ $2/M
    // = (200 + 80 + 20) / 1e6 = 300 / 1e6
    let cost = body["cost_usd"].as_f64().expect("cost present");
    assert!(
        (cost - 300.0 / 1_000_000.0).abs() < 1e-12,
        "cached discount not applied: cost was {cost}"
    );
}

/// Mock provider returning Nous Portal's degenerate `usage.cost` (rounds to `5e-05` regardless of
/// real token consumption) alongside real token counts.
async fn mock_chat_completions_nous_degenerate_cost() -> impl axum::response::IntoResponse {
    Json(json!({
        "id": "chatcmpl-nous",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "nous hi" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 10,
            "total_tokens": 1010,
            "cost": 5e-05,
            "prompt_tokens_details": { "cached_tokens": 800 }
        }
    }))
}

#[tokio::test]
async fn complete_ignores_nousportal_degenerate_usage_cost() {
    const KEY: &str = "PROVIZ_TEST_NOUS_KEY";
    std::env::set_var(KEY, "secret");

    let app = Router::new().route(
        "/v1/chat/completions",
        post(mock_chat_completions_nous_degenerate_cost),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let storage = SqliteStorage::open_in_memory().unwrap();
    // Brand slug MUST be "nousportal" — the drop-`usage.cost` rule is brand-gated.
    let brand = Brand {
        id: Uuid::new_v4(),
        slug: "nousportal".into(),
        name: "Nous Portal".into(),
        base_url: Some(format!("http://{addr}/v1")),
        is_active: true,
        priority: 0,
        created_at: Utc::now(),
        traffic_weight: 1.0,
        endpoints: None,
        price_currency: "USD".into(),
    };
    let model = Model {
        max_in_flight: None,
        id: Uuid::new_v4(),
        brand_id: brand.id,
        slug: "deepseek/deepseek-v4-flash".into(),
        display_name: "DS V4 Flash".into(),
        max_context_tokens: 32_000,
        max_output_tokens: None,
        supports_function_calling: true,
        supports_json_mode: true,
        reasoning_effort_value: None,
        price_input_per_1m: Some(0.066),
        price_output_per_1m: Some(0.132),
        tpm_limit: None,
        rpm_limit: None,
        rpd_limit: None,
        tpd_limit: None,
        tpm_limit_month: None,
        rps_limit: None,
        quality_score: Some(0.8),
        avg_latency_ms: None,
        is_enabled: true,
        notes: None,
        category: None,
        created_at: Utc::now(),
        batch_price_multiplier: None,
        diarization: None,
        streaming: None,
        http_batch: None,
        word_timestamps: None,
        base_url: None,
        supported_languages: None,
        canonical_key: None,
        price_synced_at: None,
        trains_on_data: Some(false),
        retains_data: Some(false),
        price_cached_input_per_1m: Some(0.0132),
    };
    let rule = SelectionRule {
        id: Uuid::new_v4(),
        step: "chat".into(),
        model_id: model.id,
        priority: 0,
        max_ctx_tokens: None,
        requires_fn_call: false,
        is_enabled: true,
    };
    storage.insert_brand(&brand).unwrap();
    storage.insert_model(&model).unwrap();
    storage.insert_rule(&rule).unwrap();
    {
        use proviz_elekto_core::models::BrandApiKey;
        storage
            .insert_brand_api_key(&BrandApiKey {
                id: Uuid::new_v4(),
                brand_id: brand.id,
                api_key_env: KEY.into(),
                priority: 0,
                is_active: true,
                created_at: Utc::now(),
            })
            .unwrap();
    }
    let selector = Arc::new(Selector::new(Arc::new(storage)));
    selector.reload().unwrap();

    let server_url = spawn_proviz_server(selector).await;
    let resp = reqwest::Client::new()
        .post(format!("{server_url}/complete"))
        .json(&json!({
            "step": "chat",
            "estimated_tokens": 1000,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    // Must NOT be the degenerate 5e-05 — the server drops it and computes from the catalog:
    // 200 uncached @ $0.066/M + 800 cached @ $0.0132/M + 10 output @ $0.132/M
    // = (0.0132 + 0.01056 + 0.00132) / 1e6 = 0.02508 / 1e6
    let cost = body["cost_usd"].as_f64().expect("cost present");
    let expected = (0.066 * 200.0 + 0.0132 * 800.0 + 0.132 * 10.0) / 1_000_000.0;
    assert!(
        (cost - expected).abs() < 1e-12,
        "expected catalog cost {expected}, got {cost} (degenerate usage.cost not dropped?)"
    );
}

#[tokio::test]
async fn complete_routes_jev_finite_decisions_and_excludes_it_from_chat() {
    use proviz_elekto_core::models::BrandApiKey;
    const KEY: &str = "PROVIZ_TEST_JEV_KEY";
    std::env::set_var(KEY, "synthetic-key");
    async fn mock_jev(Json(req): Json<Value>) -> Json<Value> {
        assert_eq!(req["model"], "mock-7b");
        assert!(req.get("messages").is_none());
        assert_eq!(req["questions"]["next_step"]["type"], "choice");
        assert_eq!(
            req["questions"]["next_step"]["criteria"]["option_0"]["value"],
            "OPEN:7"
        );
        Json(json!({"model":"jev-fixture","answers":{
            "next_step":{"type":"choice","choice":"option_0","confidence":0.9,"probabilities":{"option_0":0.9,"option_1":0.1}},
            "enough_evidence":{"type":"noul","noul":0.1}},
            "usage":{"input_tokens":100,"output_tokens":6}}))
    }
    let app = Router::new().route("/v1/systemone", post(mock_jev));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let selector = seed_typed_catalog(format!("http://{addr}/v1"), "typesafe", Some("decision"));
    let storage = selector.storage();
    let brand = storage.load_brands().unwrap().into_iter().next().unwrap();
    storage
        .insert_brand_api_key(&BrandApiKey {
            id: Uuid::new_v4(),
            brand_id: brand.id,
            api_key_env: KEY.into(),
            priority: 0,
            is_active: true,
            created_at: Utc::now(),
        })
        .unwrap();
    selector.reload().unwrap();
    let url = spawn_proviz_server(selector).await;
    let client = reqwest::Client::new();
    let req = json!({"step":"ricochet_decide","categories":["decision"],"requires_json_mode":true,
        "messages":[{"role":"system","content":"Follow the best citation."},{"role":"user","content":"A synthetic page."}],
        "response_format":{"type":"json_schema","json_schema":{"name":"decision","strict":true,"schema":{
            "type":"object","additionalProperties":false,"properties":{
                "next_step":{"type":"string","enum":["OPEN:7","STOP"]},"enough_evidence":{"type":"boolean"}},
            "required":["next_step","enough_evidence"]}}}});
    for _ in 0..2 {
        let resp = client
            .post(format!("{url}/complete"))
            .json(&req)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        let decision: Value = serde_json::from_str(body["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            decision,
            json!({"next_step":"OPEN:7","enough_evidence":false})
        );
        assert_eq!(body["prompt_tokens"], 100);
        assert_eq!(body["completion_tokens"], 6);
        assert!(
            body["decision_probabilities"]["next_step"]["confidence"]
                .as_f64()
                .unwrap()
                > 0.8
        );
        assert!(body["cost_usd"].as_f64().unwrap() > 0.0);
    }
    let chat = client
        .post(format!("{url}/complete"))
        .json(&json!({"step":"chat","messages":[{"role":"user","content":"Summarize"}]}))
        .send()
        .await
        .unwrap();
    assert_ne!(chat.status(), reqwest::StatusCode::OK);
    let mut invalid = req;
    invalid["response_format"]["json_schema"]["schema"]["properties"]["next_step"] =
        json!({"type":"string"});
    let resp = client
        .post(format!("{url}/complete"))
        .json(&invalid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn jev_group_falls_back_across_native_router_endpoints_on_429() {
    use axum::http::StatusCode;
    use proviz_elekto_core::models::{BrandApiKey, Group, GroupMember};
    const KEY: &str = "PROVIZ_TEST_JEV_GROUP_KEY";
    std::env::set_var(KEY, "synthetic-key");
    static THROTTLED_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    async fn throttled(Json(req): Json<Value>) -> (StatusCode, Json<Value>) {
        THROTTLED_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(req["questions"].is_object());
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"rate limited"})),
        )
    }
    async fn decision(Json(req): Json<Value>) -> Json<Value> {
        assert_eq!(req["model"], "typesafe/jev-1.13");
        assert!(req.get("messages").is_none());
        Json(
            json!({"answers":{"next_step":{"type":"choice","choice":"option_0","confidence":0.9}},
            "usage":{"input_tokens":100,"output_tokens":0}}),
        )
    }
    let app = Router::new()
        .route("/v1/systemone", post(throttled))
        .route("/api/alpha/decisions", post(decision));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let selector = seed_typed_catalog(format!("http://{addr}/v1"), "orcarouter", Some("decision"));
    let storage = selector.storage();
    let mut brand = storage.load_brands().unwrap().remove(0);
    let mut model = storage.load_models().unwrap().remove(0);
    model.slug = "typesafe/jev-1.13".into();
    model.supports_function_calling = false;
    storage.insert_model(&model).unwrap();
    let group = Group {
        max_latency_ms: None,
        max_latency_ratio: None,
        id: Uuid::new_v4(),
        slug: "jev".into(),
        name: "Jev".into(),
        description: None,
        is_active: true,
        created_at: Utc::now(),
        cost_weight_override: None,
        latency_weight_override: None,
        quality_weight_override: None,
        sticky_model: false,
    };
    storage.insert_group(&group).unwrap();
    for priority in 0..2 {
        if priority == 1 {
            brand.id = Uuid::new_v4();
            brand.slug = "openrouter".into();
            brand.base_url = Some(format!("http://{addr}/api/v1"));
            model.id = Uuid::new_v4();
            model.brand_id = brand.id;
        }
        storage.insert_brand(&brand).unwrap();
        storage.insert_model(&model).unwrap();
        storage
            .insert_brand_api_key(&BrandApiKey {
                id: Uuid::new_v4(),
                brand_id: brand.id,
                api_key_env: KEY.into(),
                priority: 0,
                is_active: true,
                created_at: Utc::now(),
            })
            .unwrap();
        storage
            .insert_group_member(&GroupMember {
                id: Uuid::new_v4(),
                group_id: group.id,
                model_id: model.id,
                priority,
                is_enabled: true,
            })
            .unwrap();
    }
    selector.reload().unwrap();
    let url = spawn_proviz_server(selector.clone()).await;
    let request = json!({"step":"ricochet_decide","group_name":"jev","categories":["decision"],
        "requires_json_mode":true,"messages":[{"role":"user","content":"Synthetic evidence"}],
        "response_format":{"type":"json_schema","json_schema":{"schema":{"type":"object",
            "properties":{"next_step":{"type":"string","enum":["STOP"]}},"required":["next_step"]}}}});
    // Verify the first member was actually attempted and entered cooldown, then reused calls
    // avoid it while preserving the same schema/probabilities on the OpenRouter route.
    for _ in 0..2 {
        let response = reqwest::Client::new()
            .post(format!("{url}/complete"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["brand"], "openrouter");
        assert_eq!(
            serde_json::from_str::<Value>(body["text"].as_str().unwrap()).unwrap(),
            json!({"next_step":"STOP"})
        );
        assert_eq!(
            body["decision_probabilities"]["next_step"]["confidence"],
            0.9
        );
    }
    assert_eq!(THROTTLED_CALLS.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pinned_wait_is_opt_in_and_metrics_expose_in_flight() {
    let selector = seed_catalog("http://localhost/v1".into());
    let model = selector.storage().load_models().unwrap().remove(0);
    selector.report_quota_details(model.id, None, Some(5000), true);
    let url = spawn_proviz_server(selector.clone()).await;
    let http = reqwest::Client::new();
    let started = Instant::now();
    let response=http.post(format!("{url}/select")).json(&json!({"step":"chat","estimated_tokens":10,"pin_model":"mockbrand/mock-7b","max_wait_ms":10000})).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    assert!(started.elapsed().as_millis() < 4000);
    // A pinned wait exceeds neither its budget nor the short provider cooldown.
    selector.report_quota_details(model.id, None, Some(50), true);
    let response=http.post(format!("{url}/select")).json(&json!({"step":"chat","estimated_tokens":10,"pin_model":"mockbrand/mock-7b","pin_wait":true,"max_wait_ms":1000})).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let metrics: Value = http
        .get(format!("{url}/metrics/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metrics["models"][0]["in_flight"], 1);
    let candidate: proviz_elekto_core::models::ModelCandidate = response.json().await.unwrap();
    selector.cancel_reservation(&candidate);
    // A wait shorter than Retry-After fails without sleeping.
    selector.report_quota_details(model.id, None, Some(5000), true);
    let response=http.post(format!("{url}/select")).json(&json!({"step":"chat","estimated_tokens":10,"pin_model":"mockbrand/mock-7b","pin_wait":true,"max_wait_ms":10})).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
}

#[tokio::test]
async fn provider_quota_body_and_retry_after_block_sibling_models() {
    use proviz_elekto_core::models::BrandApiKey;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let provider = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let count = count.clone();
            async move {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "0.2")],
                    Json(json!({"error":{"message":"INSUFFICIENT QUOTA"}})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, provider).await.unwrap();
    });
    let selector = seed_catalog(format!("http://{addr}/v1"));
    let storage = selector.storage();
    let brand = storage.load_brands().unwrap().remove(0);
    let mut sibling = storage.load_models().unwrap().remove(0);
    sibling.id = Uuid::new_v4();
    sibling.slug = "sibling".into();
    storage.insert_model(&sibling).unwrap();
    storage
        .insert_rule(&SelectionRule {
            id: Uuid::new_v4(),
            step: "chat".into(),
            model_id: sibling.id,
            priority: 1,
            max_ctx_tokens: None,
            requires_fn_call: false,
            is_enabled: true,
        })
        .unwrap();
    std::env::set_var("PROVIZ_TEST_QUOTA_KEY", "test-key");
    storage
        .insert_brand_api_key(&BrandApiKey {
            id: Uuid::new_v4(),
            brand_id: brand.id,
            api_key_env: "PROVIZ_TEST_QUOTA_KEY".into(),
            priority: 0,
            is_active: true,
            created_at: Utc::now(),
        })
        .unwrap();
    selector.reload().unwrap();
    let url = spawn_proviz_server(selector.clone()).await;
    let response=reqwest::Client::new().post(format!("{url}/complete")).json(&json!({"step":"chat","estimated_tokens":100,"messages":[{"role":"user","content":"test"}]})).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let body: Value = response.json().await.unwrap();
    assert!(body["retry_after_ms"].as_u64().unwrap() <= 200);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(selector.model_metrics()["models"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["in_flight"] == 0 && r["latency_p50_ms"].is_null()));
}

#[tokio::test]
async fn pinned_complete_retries_a_short_provider_retry_after() {
    use axum::response::IntoResponse;
    use proviz_elekto_core::models::BrandApiKey;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let count = count.clone();
            async move {
                if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "0.05")],
                        Json(json!({"error":"INSUFFICIENT QUOTA"})),
                    )
                        .into_response()
                } else {
                    mock_chat_completions().await.into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let selector = seed_catalog(format!("http://{addr}/v1"));
    let storage = selector.storage();
    let brand = storage.load_brands().unwrap().remove(0);
    std::env::set_var("PROVIZ_TEST_PIN_KEY", "test-key");
    storage
        .insert_brand_api_key(&BrandApiKey {
            id: Uuid::new_v4(),
            brand_id: brand.id,
            api_key_env: "PROVIZ_TEST_PIN_KEY".into(),
            priority: 0,
            is_active: true,
            created_at: Utc::now(),
        })
        .unwrap();
    selector.reload().unwrap();
    let url = spawn_proviz_server(selector.clone()).await;
    let response=reqwest::Client::new().post(format!("{url}/complete")).json(&json!({"step":"chat","estimated_tokens":10,"pin_model":"mockbrand/mock-7b","pin_wait":true,"max_wait_ms":1000,"messages":[{"role":"user","content":"test"}]})).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(selector.model_metrics()["models"][0]["in_flight"], 0);
}

#[tokio::test]
async fn cancelled_completion_releases_its_concurrency_slot() {
    use proviz_elekto_core::models::BrandApiKey;
    let entered = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let signal = signal.clone();
            async move {
                signal.notify_one();
                std::future::pending::<()>().await;
                Json(json!({}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let selector = seed_catalog(format!("http://{addr}/v1"));
    let storage = selector.storage();
    let model = storage.load_models().unwrap().remove(0);
    storage.set_model_cap(model.id, Some(1)).unwrap();
    std::env::set_var("PROVIZ_TEST_CANCEL_KEY", "test-key");
    storage
        .insert_brand_api_key(&BrandApiKey {
            id: Uuid::new_v4(),
            brand_id: model.brand_id,
            api_key_env: "PROVIZ_TEST_CANCEL_KEY".into(),
            priority: 0,
            is_active: true,
            created_at: Utc::now(),
        })
        .unwrap();
    selector.reload().unwrap();
    let state = Arc::new(AppState {
        selector: selector.clone(),
        batch_queue: Arc::new(batch::BatchQueue::new(60, 100, "http://localhost".into())),
        started_at: Instant::now(),
        providers_dir: ".".into(),
        http: reqwest::Client::new(),
    });
    let request = serde_json::from_value(
        json!({"step":"chat","estimated_tokens":10,"messages":[{"role":"user","content":"test"}]}),
    )
    .unwrap();
    let task = tokio::spawn(proviz_server::complete::run_complete(state, request));
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert_eq!(selector.model_metrics()["models"][0]["in_flight"], 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(selector.model_metrics()["models"][0]["in_flight"], 0);
}

#[tokio::test]
async fn complete_openrouter_unknown_training_enforces_privacy() {
    use proviz_elekto_core::models::BrandApiKey;
    const KEY: &str = "PROVIZ_TEST_OPENROUTER_PRIVACY_KEY";
    std::env::set_var(KEY, "synthetic-key");
    async fn privacy_provider(Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["provider"]["data_collection"], "deny");
        assert_eq!(body["provider"]["zdr"], true);
        Json(
            json!({"choices":[{"message":{"content":"privacy protected"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1}}),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/chat/completions", post(privacy_provider)),
        )
        .await
        .unwrap();
    });
    let selector = seed_typed_catalog(format!("http://{addr}/v1"), "openrouter", None);
    let brand = selector.storage().load_brands().unwrap().remove(0);
    selector
        .storage()
        .insert_brand_api_key(&BrandApiKey {
            id: Uuid::new_v4(),
            brand_id: brand.id,
            api_key_env: KEY.into(),
            priority: 0,
            is_active: true,
            created_at: Utc::now(),
        })
        .unwrap();
    selector.reload().unwrap();
    let url = spawn_proviz_server(selector).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{url}/select"))
        .json(&json!({
            "step":"chat","estimated_tokens":1000,"require_no_training":true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let response = client
        .post(format!("{url}/complete"))
        .json(&json!({
            "step":"chat","require_no_training":true,
            "messages":[{"role":"user","content":"Synthetic test"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["text"],
        "privacy protected"
    );
}

#[tokio::test]
async fn complete_preserves_provider_error_when_pin_is_exhausted() {
    use proviz_elekto_core::models::BrandApiKey;
    const KEY: &str = "PROVIZ_TEST_PROVIDER_FAILURE_KEY";
    std::env::set_var(KEY, "synthetic-key");
    async fn forbidden() -> impl axum::response::IntoResponse {
        (
            reqwest::StatusCode::FORBIDDEN,
            Json(json!({"error":{"message":"Regional routing requires account access"}})),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/chat/completions", post(forbidden)),
        )
        .await
        .unwrap();
    });
    let selector = seed_catalog(format!("http://{addr}/v1"));
    let brand = selector.storage().load_brands().unwrap().remove(0);
    selector
        .storage()
        .insert_brand_api_key(&BrandApiKey {
            id: Uuid::new_v4(),
            brand_id: brand.id,
            api_key_env: KEY.into(),
            priority: 0,
            is_active: true,
            created_at: Utc::now(),
        })
        .unwrap();
    selector.reload().unwrap();
    let url = spawn_proviz_server(selector).await;
    let response = reqwest::Client::new()
        .post(format!("{url}/complete"))
        .json(&json!({
            "step":"chat","pin_model":"mockbrand/mock-7b",
            "messages":[{"role":"user","content":"Synthetic test"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "all_providers_failed");
    assert!(body["detail"]
        .as_str()
        .unwrap()
        .contains("Regional routing requires account access"));
}

#[tokio::test]
async fn cloudflare_jev_group_accepts_no_training_and_preserves_native_probabilities() {
    use proviz_elekto_core::{
        builtin_providers::load_from_dir,
        models::{Group, GroupMember},
    };
    let root = std::env::temp_dir().join(format!("proviz-cloudflare-{}", Uuid::new_v4()));
    std::fs::create_dir_all(root.join("cloudflare")).unwrap();
    async fn run(Json(req): Json<Value>) -> Json<Value> {
        assert_eq!(req["model"], "typesafe/jev");
        assert!(req.get("messages").is_none());
        assert!(req.get("questions").is_none());
        assert_eq!(
            req["input"]["questions"]["decision"]["criteria"]["option_1"]["value"],
            "duplicate"
        );
        Json(
            json!({"success":true,"result":{"state":"Completed","result":{"model":"jev-1.13.0","answers":{
            "decision":{"type":"choice","choice":"option_1","confidence":0.96,
                "probabilities":{"option_0":0.02,"option_1":0.97,"option_2":0.01}}},
            "usage":{"input_tokens":200,"output_tokens":30}}}}),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/ai/run", post(run)))
            .await
            .unwrap();
    });
    const KEY: &str = "PROVIZ_TEST_CLOUDFLARE_JEV";
    std::env::set_var(KEY, "synthetic-key");
    std::fs::write(root.join("cloudflare/brand.json"), json!({"slug":"cloudflare","name":"Cloudflare", "api_key_env":KEY,"base_url":format!("http://{addr}/ai/v1")}).to_string()).unwrap();
    std::fs::write(
        root.join("cloudflare/models.json"),
        include_str!("../../providers/cloudflare/jev.json"),
    )
    .unwrap();
    let storage = Arc::new(SqliteStorage::open_in_memory().unwrap());
    load_from_dir(storage.as_ref(), root.to_str().unwrap(), false, false).unwrap();
    let model = storage.load_models().unwrap().remove(0);
    assert_eq!(model.category.as_deref(), Some("decision"));
    assert_eq!(model.trains_on_data, Some(false));
    assert_eq!(model.retains_data, Some(false));
    let group: Group = serde_json::from_value(json!({"id":Uuid::new_v4(),"slug":"cloudflare-jev", "name":"Cloudflare Jev","is_active":true,"created_at":Utc::now(),"sticky_model":false})).unwrap();
    storage.insert_group(&group).unwrap();
    storage
        .insert_group_member(&GroupMember {
            id: Uuid::new_v4(),
            group_id: group.id,
            model_id: model.id,
            priority: 0,
            is_enabled: true,
        })
        .unwrap();
    let selector = Arc::new(Selector::new(storage));
    selector.reload().unwrap();
    let url = spawn_proviz_server(selector).await;
    let client = reqwest::Client::new();
    let req = json!({"step":"decision","group_name":"cloudflare-jev","categories":["decision"],"require_no_training":true,"requires_json_mode":true,
        "messages":[{"role":"user","content":"Both synthetic statements describe the same fact."}],
        "response_format":{"type":"json_schema","json_schema":{"name":"decision","strict":true,"schema":{
            "type":"object","additionalProperties":false,"properties":{"decision":{"type":"string","enum":["distinct","duplicate","more_specific"]}},"required":["decision"]}}}});
    let response = client
        .post(format!("{url}/complete"))
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["brand"], "cloudflare");
    assert_eq!(body["model"], "typesafe/jev");
    assert_eq!(
        serde_json::from_str::<Value>(body["text"].as_str().unwrap()).unwrap(),
        json!({"decision":"duplicate"})
    );
    assert_eq!(
        body["decision_probabilities"]["decision"]["confidence"],
        0.96
    );
    assert_eq!(
        body["decision_probabilities"]["decision"]["probabilities"]["option_1"],
        0.97
    );
    assert!((body["cost_usd"].as_f64().unwrap() - 0.0000084).abs() < 1e-12);
    let chat = client
        .post(format!("{url}/complete"))
        .json(&json!({"step":"chat","group_name":"cloudflare-jev","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), reqwest::StatusCode::CONFLICT);
    std::fs::remove_dir_all(root).unwrap();
    std::env::remove_var(KEY);
}

#[tokio::test]
async fn cloudflare_chat_uses_openai_payload_and_token_accounting() {
    async fn chat(Json(req): Json<Value>) -> Json<Value> {
        assert_eq!(req["model"], "mock-7b");
        assert_eq!(req["messages"][0]["content"], "Say hello.");
        assert!(req.get("input").is_none());
        assert!(req.get("provider").is_none());
        Json(
            json!({"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/ai/v1/chat/completions", post(chat)),
        )
        .await
        .unwrap();
    });
    let selector = seed_typed_catalog(format!("http://{addr}/ai/v1"), "cloudflare", Some("text"));
    let storage = selector.storage();
    let mut model = storage.load_models().unwrap().remove(0);
    model.trains_on_data = Some(false);
    storage.insert_model(&model).unwrap();
    const KEY: &str = "PROVIZ_TEST_CLOUDFLARE_CHAT";
    storage
        .insert_brand_api_key(&proviz_elekto_core::models::BrandApiKey {
            id: Uuid::new_v4(),
            brand_id: model.brand_id,
            api_key_env: KEY.into(),
            priority: 0,
            is_active: true,
            created_at: Utc::now(),
        })
        .unwrap();
    std::env::set_var(KEY, "synthetic-key");
    selector.reload().unwrap();
    let url = spawn_proviz_server(selector).await;
    let response = reqwest::Client::new().post(format!("{url}/complete")).json(&json!({"step":"chat","require_no_training":true,"messages":[{"role":"user","content":"Say hello."}]})).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["text"], "hello");
    assert_eq!(body["prompt_tokens"], 10);
    assert_eq!(body["completion_tokens"], 2);
    assert!(body.get("decision_probabilities").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloudflare_sync_paginates_disables_missing_and_preserves_other_provider_flags() {
    use axum::{
        extract::{Query, State},
        routing::get,
    };
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicUsize, Ordering},
    };
    async fn models(
        State(phase): State<Arc<AtomicUsize>>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let n = match phase.load(Ordering::SeqCst) {
            0 => 12,
            1 => 11,
            _ => 0,
        };
        if q.contains_key("format") {
            return Json(
                json!({"data":(0..n).map(|i| json!({"id":format!("@cf/test/chat-{i}"),"supported_features":["json_mode"]})).collect::<Vec<_>>()}),
            );
        }
        // A deliberately inconsistent total_count matches the live API's account filtering.
        let page = q.get("page").unwrap().parse::<usize>().unwrap();
        let items: Vec<_> = if page == 1 {
            (0..n.min(6)).collect()
        } else if page == 2 {
            (6..n).collect()
        } else {
            vec![]
        };
        Json(
            json!({"success":true,"result":items.into_iter().map(|i| json!({"name":format!("@cf/test/chat-{i}"),"task":{"name":"Text Generation"},"properties":[{"property_id":"context_window","value":"32000"}]})).collect::<Vec<_>>(),"result_info":{"total_count":326}}),
        )
    }
    let phase = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/ai/models/search", get(models))
        .with_state(phase.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let selector = seed_catalog("http://example.test/v1".into());
    let storage = selector.storage();
    let mut other = storage.load_models().unwrap().remove(0);
    other.trains_on_data = Some(true);
    storage.insert_model(&other).unwrap();
    let root = std::env::temp_dir().join(format!("proviz-cf-sync-{}", Uuid::new_v4()));
    std::fs::create_dir_all(root.join("cloudflare")).unwrap();
    std::fs::create_dir_all(root.join("mockbrand")).unwrap();
    std::fs::write(
        root.join("cloudflare/brand.json"),
        include_str!("../../providers/cloudflare/brand.json"),
    )
    .unwrap();
    std::fs::write(
        root.join("mockbrand/brand.json"),
        json!({"slug":"mockbrand","name":"Mock"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join("mockbrand/models.json"),
        json!([{"slug":"mock-7b","trains_on_data":false}]).to_string(),
    )
    .unwrap();
    std::env::set_var("CLOUDFLARE_API_KEY", "synthetic-sync-key");
    for phase_number in 0..3 {
        phase.store(phase_number, Ordering::SeqCst);
        let s = storage.clone();
        let dir = root.to_str().unwrap().to_string();
        let result = tokio::task::spawn_blocking(move || {
            proviz_elekto_core::cloudflare_sync::sync(
                s.as_ref(),
                &dir,
                &format!("http://{addr}/ai"),
            )
        })
        .await
        .unwrap();
        match phase_number {
            0 => assert_eq!(result.unwrap().fetched, 13),
            1 => assert_eq!(result.unwrap().models_disabled, 1),
            _ => assert!(result.is_err()),
        }
    }
    let models = storage.load_models().unwrap();
    let missing = models
        .iter()
        .find(|m| m.slug == "@cf/test/chat-11")
        .unwrap();
    assert!(!missing.is_enabled);
    let chat = models.iter().find(|m| m.slug == "@cf/test/chat-0").unwrap();
    assert!(chat.is_enabled);
    assert_eq!(chat.trains_on_data, Some(false));
    assert_eq!(chat.retains_data, None);
    assert_eq!(
        models
            .iter()
            .find(|m| m.id == other.id)
            .unwrap()
            .trains_on_data,
        Some(true)
    );
    assert_eq!(
        models
            .iter()
            .find(|m| m.slug == "typesafe/jev")
            .unwrap()
            .category
            .as_deref(),
        Some("decision")
    );
    let last_good: Vec<Value> =
        serde_json::from_slice(&std::fs::read(root.join("cloudflare/models.json")).unwrap())
            .unwrap();
    assert_eq!(last_good.len(), 12);
    std::fs::remove_dir_all(root).unwrap();
    std::env::remove_var("CLOUDFLARE_API_KEY");
}

// Use the shipped Mistral catalog and an in-memory DB; no provider calls or keys.
#[tokio::test]
async fn voxtral3_batch_catalog_exposes_native_diarization_without_duplicate_v1() {
    use proviz_elekto_core::builtin_providers::load_provider_from_dir;
    let storage = SqliteStorage::open_in_memory().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../providers");
    load_provider_from_dir(&storage, root.to_str().unwrap(), "mistral").unwrap();
    let models = storage.load_models().unwrap();
    for (slug, price) in [
        ("voxtral-mini-transcribe-3", 0.0035),
        ("voxtral-small-transcribe-3", 0.004),
    ] {
        let model = models.iter().find(|m| m.slug == slug).unwrap();
        assert_eq!(model.price_input_per_1m, Some(price));
        assert_eq!(model.category.as_deref(), Some("audio"));
    }
    let selector = Arc::new(Selector::new(Arc::new(storage)));
    selector.reload().unwrap();
    let url = spawn_proviz_server(selector).await;
    for slug in ["voxtral-mini-transcribe-3", "voxtral-small-transcribe-3"] {
        let response = reqwest::Client::new()
            .get(format!("{url}/stt/model-info"))
            .query(&[("provider", format!("mistral/{slug}"))])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["base_url"], "https://api.mistral.ai");
        assert_eq!(body["stt_path"], "/v1/audio/transcriptions");
        assert_eq!(body["diarization"], true);
        assert_eq!(body["http_batch"], true);
        assert_eq!(body["streaming"], false);
        assert_eq!(body["word_timestamps"], false);
        assert_eq!(body["api_key_env"], "MISTRAL_API_KEY");
    }
}
