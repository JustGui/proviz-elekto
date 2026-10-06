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
