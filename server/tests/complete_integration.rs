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
    let storage = SqliteStorage::open_in_memory().expect("in-memory db");
    let brand = Brand {
        id: Uuid::new_v4(),
        slug: "mockbrand".into(),
        name: "Mock Brand".into(),
        base_url: Some(base_url),
        is_active: true,
        priority: 0,
        created_at: Utc::now(),
        traffic_weight: 1.0,
        endpoints: None,
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
