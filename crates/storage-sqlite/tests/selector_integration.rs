use chrono::Utc;
use proviz_elekto_core::{
    error::ProvizError,
    models::{Brand, Model, RateLimitErrorType, SelectRequest, SelectionRule},
    selector::Selector,
};
use proviz_elekto_storage_sqlite::SqliteStorage;
use std::sync::Arc;
use uuid::Uuid;

// ── helpers ──────────────────────────────────────────────────────────────────

fn make_brand(slug: &str, priority: i16) -> Brand {
    Brand {
        id: Uuid::new_v4(),
        slug: slug.to_string(),
        name: slug.to_string(),
        base_url: None,
        is_active: true,
        priority,
        created_at: Utc::now(),
        traffic_weight: 1.0,
        endpoints: None,
        price_currency: "USD".to_string(),
    }
}

fn make_model(brand_id: Uuid, slug: &str, ctx: u32) -> Model {
    Model {
        id: Uuid::new_v4(),
        brand_id,
        slug: slug.to_string(),
        display_name: slug.to_string(),
        max_context_tokens: ctx,
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
        quality_score: Some(0.80),
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
        price_cached_input_per_1m: None,
    }
}

fn make_rule(step: &str, model_id: Uuid, priority: i16) -> SelectionRule {
    SelectionRule {
        id: Uuid::new_v4(),
        step: step.to_string(),
        model_id,
        priority,
        max_ctx_tokens: None,
        requires_fn_call: false,
        is_enabled: true,
    }
}

/// One brand "acme", one model "acme-7b" (ctx=32k, fn_call+json_mode, quality=0.80),
/// one rule for step "chat" at priority 0. Returns (storage, brand_id, model_id, rule_id).
fn make_world() -> (SqliteStorage, Uuid, Uuid, Uuid) {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().expect("in-memory db");
    let brand = make_brand("acme", 0);
    let model = make_model(brand.id, "acme-7b", 32_000);
    let rule = make_rule("chat", model.id, 0);
    let (bid, mid, rid) = (brand.id, model.id, rule.id);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    (db, bid, mid, rid)
}

fn base_req() -> SelectRequest {
    SelectRequest {
        step: "chat".to_string(),
        estimated_tokens: 1_000,
        requires_fn_call: false,
        requires_json_mode: false,
        requires_streaming: None,
        quality_min: 0.0,
        exclude_ids: vec![],
        categories: vec![],
        languages: vec![],
        group_id: None,
        group_name: None,
        use_member_priority: true,
        max_wait_ms: None,
        require_no_training: false,
        cost_weight: None,
        latency_weight: None,
        quality_weight: None,
        pin_model: None,
    }
}

fn selector(db: SqliteStorage) -> Selector {
    Selector::new(Arc::new(db))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[test]
fn basic_select() {
    let (db, _, _, _) = make_world();
    let sel = selector(db);
    let c = sel.select(&base_req()).unwrap();
    assert_eq!(c.model_slug, "acme-7b");
    assert_eq!(c.brand_slug, "acme");
}

#[test]
fn disabled_rule_exhausted() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = make_model(brand.id, "acme-7b", 32_000);
    let rule = SelectionRule {
        is_enabled: false,
        ..make_rule("chat", model.id, 0)
    };
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    let err = selector(db).select(&base_req()).unwrap_err();
    assert!(matches!(
        err,
        ProvizError::AllModelsExhausted { tried: 0, .. }
    ));
}

#[test]
fn disabled_model_exhausted() {
    use proviz_elekto_core::storage::CatalogStorage;
    let (db, _, mid, _) = make_world();
    db.set_model_enabled(mid, false).unwrap();
    let sel = selector(db);
    sel.reload().unwrap();
    let err = sel.select(&base_req()).unwrap_err();
    assert!(matches!(err, ProvizError::AllModelsExhausted { .. }));
}

#[test]
fn inactive_brand_exhausted() {
    use proviz_elekto_core::storage::CatalogStorage;
    let (db, bid, _, _) = make_world();
    db.set_brand_active(bid, false).unwrap();
    let sel = selector(db);
    sel.reload().unwrap();
    let err = sel.select(&base_req()).unwrap_err();
    assert!(matches!(err, ProvizError::AllModelsExhausted { .. }));
}

#[test]
fn context_too_small() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = make_model(brand.id, "small-model", 4_000);
    let rule = make_rule("chat", model.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    let req = SelectRequest {
        estimated_tokens: 8_000,
        ..base_req()
    };
    let err = selector(db).select(&req).unwrap_err();
    assert!(matches!(
        err,
        ProvizError::AllModelsExhausted { tried: 0, .. }
    ));
}

#[test]
fn rule_max_ctx_upper_bound_exceeded() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = make_model(brand.id, "acme-7b", 32_000);
    let rule = SelectionRule {
        max_ctx_tokens: Some(2_000),
        ..make_rule("chat", model.id, 0)
    };
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    let req = SelectRequest {
        estimated_tokens: 3_000,
        ..base_req()
    };
    let err = selector(db).select(&req).unwrap_err();
    assert!(matches!(err, ProvizError::AllModelsExhausted { .. }));
}

#[test]
fn rule_max_ctx_upper_bound_fits() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = make_model(brand.id, "acme-7b", 32_000);
    let rule = SelectionRule {
        max_ctx_tokens: Some(2_000),
        ..make_rule("chat", model.id, 0)
    };
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    let req = SelectRequest {
        estimated_tokens: 1_000,
        ..base_req()
    };
    assert!(selector(db).select(&req).is_ok());
}

#[test]
fn fn_call_required_missing() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = Model {
        supports_function_calling: false,
        ..make_model(brand.id, "acme-7b", 32_000)
    };
    let rule = make_rule("chat", model.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    let req = SelectRequest {
        requires_fn_call: true,
        ..base_req()
    };
    let err = selector(db).select(&req).unwrap_err();
    assert!(matches!(err, ProvizError::AllModelsExhausted { .. }));
}

#[test]
fn fn_call_required_present() {
    let (db, _, _, _) = make_world(); // model has fn_call=true
    let req = SelectRequest {
        requires_fn_call: true,
        ..base_req()
    };
    assert!(selector(db).select(&req).is_ok());
}

#[test]
fn json_mode_required_missing() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = Model {
        supports_json_mode: false,
        ..make_model(brand.id, "acme-7b", 32_000)
    };
    let rule = make_rule("chat", model.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    let req = SelectRequest {
        requires_json_mode: true,
        ..base_req()
    };
    let err = selector(db).select(&req).unwrap_err();
    assert!(matches!(err, ProvizError::AllModelsExhausted { .. }));
}

#[test]
fn quality_too_low() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = Model {
        quality_score: Some(0.50),
        ..make_model(brand.id, "acme-7b", 32_000)
    };
    let rule = make_rule("chat", model.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    let req = SelectRequest {
        quality_min: 0.80,
        ..base_req()
    };
    let err = selector(db).select(&req).unwrap_err();
    assert!(matches!(err, ProvizError::AllModelsExhausted { .. }));
}

#[test]
fn quality_zero_min_allows_none() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = Model {
        quality_score: None,
        ..make_model(brand.id, "acme-7b", 32_000)
    };
    let rule = make_rule("chat", model.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    // quality_min=0.0 skips the quality filter entirely → model eligible despite None score
    assert!(selector(db).select(&base_req()).is_ok());
}

#[test]
fn quality_set_min_rejects_none() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = Model {
        quality_score: None,
        ..make_model(brand.id, "acme-7b", 32_000)
    };
    let rule = make_rule("chat", model.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    let req = SelectRequest {
        quality_min: 0.5,
        ..base_req()
    };
    let err = selector(db).select(&req).unwrap_err();
    assert!(matches!(err, ProvizError::AllModelsExhausted { .. }));
}

#[test]
fn exclude_ids_increments_tried() {
    let (db, _, mid, _) = make_world();
    let req = SelectRequest {
        exclude_ids: vec![mid],
        ..base_req()
    };
    let err = selector(db).select(&req).unwrap_err();
    assert!(matches!(
        err,
        ProvizError::AllModelsExhausted { tried: 1, .. }
    ));
}

#[test]
fn rate_limit_skips_model() {
    let (db, _, mid, _) = make_world();
    let sel = selector(db);
    sel.report_rate_limit(
        mid,
        None,
        RateLimitErrorType::Tpm,
        0,
        None,
        None,
        None,
        None,
    );
    let err = sel.select(&base_req()).unwrap_err();
    assert!(matches!(
        err,
        ProvizError::AllModelsExhausted { tried: 1, .. }
    ));
}

#[test]
fn report_success_clears_limit() {
    let (db, _, mid, _) = make_world();
    let sel = selector(db);
    sel.report_rate_limit(
        mid,
        None,
        RateLimitErrorType::Tpm,
        0,
        None,
        None,
        None,
        None,
    );
    sel.report_success(mid, None, 0, None, None, None, None, None, None, None, None);
    assert!(sel.select(&base_req()).is_ok());
}

#[test]
fn priority_ordering() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let m_first = make_model(brand.id, "model-first", 32_000);
    let m_second = make_model(brand.id, "model-second", 32_000);
    let r_first = make_rule("chat", m_first.id, 0);
    let r_second = make_rule("chat", m_second.id, 1);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&m_first).unwrap();
    db.insert_model(&m_second).unwrap();
    db.insert_rule(&r_first).unwrap();
    db.insert_rule(&r_second).unwrap();
    let c = selector(db).select(&base_req()).unwrap();
    assert_eq!(c.model_slug, "model-first");
}

#[test]
fn step_not_found_falls_back_to_brand_priority() {
    // Unknown steps now fall back to brand-priority synthetic rules instead of erroring.
    let (db, _, _, _) = make_world();
    let req = SelectRequest {
        step: "summarize".to_string(),
        ..base_req()
    };
    let c = selector(db).select(&req).unwrap();
    assert_eq!(c.model_slug, "acme-7b");
}

#[test]
fn category_audio_skipped_by_default() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = Model {
        category: Some("audio".to_string()),
        ..make_model(brand.id, "acme-audio", 32_000)
    };
    let rule = make_rule("chat", model.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    // no categories in request → audio model skipped
    let err = selector(db).select(&base_req()).unwrap_err();
    assert!(matches!(err, ProvizError::AllModelsExhausted { .. }));
}

#[test]
fn category_audio_opted_in() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let model = Model {
        category: Some("audio".to_string()),
        ..make_model(brand.id, "acme-audio", 32_000)
    };
    let rule = make_rule("chat", model.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&rule).unwrap();
    let req = SelectRequest {
        categories: vec!["audio".to_string()],
        ..base_req()
    };
    assert!(selector(db).select(&req).is_ok());
}

#[test]
fn select_returns_estimated_tokens() {
    let (db, _, _, _) = make_world();
    let c = selector(db).select(&base_req()).unwrap();
    assert_eq!(c.estimated_tokens, base_req().estimated_tokens as u64);
}

#[test]
fn scoring_prefers_cheaper_model() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let cheap = Model {
        price_input_per_1m: Some(1.0),
        quality_score: Some(0.80),
        ..make_model(brand.id, "cheap", 32_000)
    };
    let expensive = Model {
        price_input_per_1m: Some(10.0),
        quality_score: Some(0.80),
        ..make_model(brand.id, "expensive", 32_000)
    };
    // Same priority — scoring should pick the cheaper one
    let r_cheap = make_rule("chat", cheap.id, 0);
    let r_expensive = make_rule("chat", expensive.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&cheap).unwrap();
    db.insert_model(&expensive).unwrap();
    db.insert_rule(&r_cheap).unwrap();
    db.insert_rule(&r_expensive).unwrap();
    let c = selector(db).select(&base_req()).unwrap();
    assert_eq!(c.model_slug, "cheap");
}

#[test]
fn scoring_prefers_higher_quality() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let high_q = Model {
        price_input_per_1m: Some(5.0),
        quality_score: Some(0.95),
        ..make_model(brand.id, "high-quality", 32_000)
    };
    let low_q = Model {
        price_input_per_1m: Some(5.0),
        quality_score: Some(0.40),
        ..make_model(brand.id, "low-quality", 32_000)
    };
    let r_hq = make_rule("chat", high_q.id, 0);
    let r_lq = make_rule("chat", low_q.id, 0);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&high_q).unwrap();
    db.insert_model(&low_q).unwrap();
    db.insert_rule(&r_hq).unwrap();
    db.insert_rule(&r_lq).unwrap();
    let c = selector(db).select(&base_req()).unwrap();
    assert_eq!(c.model_slug, "high-quality");
}

#[test]
fn scoring_prefers_live_reported_low_latency() {
    // Neither model has a static catalog avg_latency_ms — scoring should treat them as tied
    // (latency_score=0.5 for both) until real completions are reported via report_success's
    // response_time_ms, at which point the live EWMA should flip the winner toward the
    // consistently faster one — this is the mechanism that lets a slow-routing aggregator
    // (e.g. OpenRouter/Requesty) get scored down based on observed behaviour, not a guess.
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let slow = make_model(brand.id, "slow-live", 32_000);
    let fast = make_model(brand.id, "fast-live", 32_000);
    // Priority favors "slow" on a tie, so a post-report win by "fast" proves live latency moved it.
    let r_slow = make_rule("chat", slow.id, 0);
    let r_fast = make_rule("chat", fast.id, 1);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&slow).unwrap();
    db.insert_model(&fast).unwrap();
    db.insert_rule(&r_slow).unwrap();
    db.insert_rule(&r_fast).unwrap();

    let sel = selector(db);
    // No live samples yet — both untouched, rule_priority tiebreak picks "slow".
    let c = sel.select(&base_req()).unwrap();
    assert_eq!(c.model_slug, "slow-live");

    sel.report_success(
        slow.id,
        None,
        0,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(20_000),
    );
    sel.report_success(
        fast.id,
        None,
        0,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(100),
    );

    let c = sel.select(&base_req()).unwrap();
    assert_eq!(c.model_slug, "fast-live");
}

#[test]
fn rpm_limit_single_model_exhausted() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    // rpm_limit=1: headroom is NOT a hard filter — an over-quota model remains eligible.
    // AllModelsExhausted only triggers after the model is reactively rate-limited.
    let tight = Model {
        rpm_limit: Some(1),
        ..make_model(brand.id, "tight", 32_000)
    };
    db.insert_brand(&brand).unwrap();
    db.insert_model(&tight).unwrap();
    db.insert_rule(&make_rule("chat", tight.id, 0)).unwrap();

    let sel = selector(db);
    // First select: headroom=0.0 (last slot), model is eligible.
    let c1 = sel.select(&base_req()).unwrap();
    assert_eq!(c1.model_slug, "tight");

    // Second select: in_flight=1 → headroom=-1.0, but still the only candidate (soft filter).
    let c2 = sel.select(&base_req()).unwrap();
    assert_eq!(c2.model_slug, "tight");

    // After the provider returns 429, reactive RateLimitState blocks the model.
    sel.report_rate_limit(
        tight.id,
        None,
        proviz_elekto_core::models::RateLimitErrorType::Rpm,
        0,
        None,
        None,
        None,
        None,
    );

    // Now AllModelsExhausted because the only model is reactively blocked.
    let err = sel.select(&base_req()).unwrap_err();
    assert!(matches!(
        err,
        ProvizError::AllModelsExhausted { tried: 1, .. }
    ));
}

#[test]
fn headroom_scoring_causes_fallback_when_loaded() {
    use proviz_elekto_core::storage::CatalogStorage;
    // "primary" has high quality (1.0) but rpm_limit=2.
    // "backup" has low quality (0.0) but is unlimited.
    // Headroom is a soft signal: primary's quality advantage keeps it winning until its
    // fast_headroom reaches -1.0 (severely over quota), at which point backup wins.
    //
    // Scoring (no group):
    //   score = 0.25*fast_hr_norm + 0.20*slow_hr_norm + 0.20*quality + 0.15*cost + 0.10*latency + 0.10*traffic
    // primary with fast_headroom=-1.0: fast_hr_norm=0.0 → score=0.575
    // backup (unlimited, quality=0.0): fast_hr_norm=1.0 → score=0.625 → backup wins
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let primary = Model {
        quality_score: Some(1.0),
        rpm_limit: Some(2),
        ..make_model(brand.id, "primary", 32_000)
    };
    let backup = Model {
        quality_score: Some(0.0),
        ..make_model(brand.id, "backup", 32_000)
    };
    db.insert_brand(&brand).unwrap();
    db.insert_model(&primary).unwrap();
    db.insert_model(&backup).unwrap();
    db.insert_rule(&make_rule("chat", primary.id, 0)).unwrap();
    db.insert_rule(&make_rule("chat", backup.id, 1)).unwrap();

    let sel = selector(db);
    // Selects 1-3: primary still preferred despite dropping headroom (quality advantage holds).
    // in_flight=0→1: fast_headroom=0.0, score≈0.70 vs backup 0.625 → primary wins
    // in_flight=1→2: fast_headroom=-0.5, score≈0.638 vs backup 0.625 → primary wins (tiebreak)
    // in_flight=2→3: fast_headroom=-1.0, score=0.575 vs backup 0.625 → backup wins
    let c1 = sel.select(&base_req()).unwrap();
    assert_eq!(c1.model_slug, "primary");
    let c2 = sel.select(&base_req()).unwrap();
    assert_eq!(c2.model_slug, "primary");
    let c3 = sel.select(&base_req()).unwrap();
    assert_eq!(c3.model_slug, "primary");

    // 4th select: primary fast_headroom=-1.0 → score=0.575, backup 0.625 → backup wins.
    let c4 = sel.select(&base_req()).unwrap();
    assert_eq!(c4.model_slug, "backup");
}

// ── single-key, multi-model brands: model-scoped vs account-scoped errors ──
//
// Regression coverage for a real production bug: a brand with exactly one
// active API key serving several models (e.g. one OVHCloud key behind four
// Qwen variants). A timeout on one model must not lock out its siblings —
// only a genuinely account-scoped signal (quota/auth) should block the
// shared key. Before this fix, report_error/report_rate_limit keyed purely
// off "is there a brand_key_id" and always marked the shared key, so one
// flaky model repeatedly took down every sibling model behind the same key.

fn make_brand_key(brand_id: Uuid, env: &str) -> proviz_elekto_core::models::BrandApiKey {
    proviz_elekto_core::models::BrandApiKey {
        id: Uuid::new_v4(),
        brand_id,
        api_key_env: env.to_string(),
        priority: 0,
        is_active: true,
        created_at: Utc::now(),
    }
}

#[test]
fn model_scoped_timeout_does_not_block_sibling_on_shared_key() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let flaky = make_model(brand.id, "flaky", 32_000);
    let steady = make_model(brand.id, "steady", 32_000);
    let key = make_brand_key(brand.id, "ACME_API_KEY");
    db.insert_brand(&brand).unwrap();
    db.insert_model(&flaky).unwrap();
    db.insert_model(&steady).unwrap();
    db.insert_brand_api_key(&key).unwrap();
    db.insert_rule(&make_rule("chat", flaky.id, 0)).unwrap();
    db.insert_rule(&make_rule("chat", steady.id, 1)).unwrap();

    let sel = selector(db);
    // "flaky" times out; it was served by the brand's one key.
    sel.report_error(
        flaky.id,
        Some(key.id),
        RateLimitErrorType::Timeout,
        0,
        None,
        None,
        None,
        None,
    );

    // "steady" must still be selectable — the shared key must not be blocked
    // by a model-scoped error on a different model.
    let c = sel.select(&base_req()).unwrap();
    assert_eq!(c.model_slug, "steady");
}

#[test]
fn account_scoped_error_still_blocks_whole_shared_key() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let m1 = make_model(brand.id, "m1", 32_000);
    let m2 = make_model(brand.id, "m2", 32_000);
    let key = make_brand_key(brand.id, "ACME_API_KEY");
    db.insert_brand(&brand).unwrap();
    db.insert_model(&m1).unwrap();
    db.insert_model(&m2).unwrap();
    db.insert_brand_api_key(&key).unwrap();
    db.insert_rule(&make_rule("chat", m1.id, 0)).unwrap();
    db.insert_rule(&make_rule("chat", m2.id, 1)).unwrap();

    let sel = selector(db);
    // Auth failure on m1 is a real account-level problem (bad/expired key) —
    // both models sharing that key should become unavailable.
    sel.report_error(
        m1.id,
        Some(key.id),
        RateLimitErrorType::Auth,
        0,
        None,
        None,
        None,
        None,
    );

    let err = sel.select(&base_req()).unwrap_err();
    assert!(matches!(
        err,
        ProvizError::AllModelsExhausted { tried: 2, .. }
    ));
}

// ── tunable cost/latency/quality weights + per-step measured quality ──────────

/// A cheap, low-quality model vs. an expensive, high-quality one — the default weights (quality
/// 0.20, cost 0.15) pick the expensive one; overriding `cost_weight` heavily enough on the
/// request flips the winner to the cheap one. Also proves the "omit everything" default is
/// unaffected (same winner as before any override field existed).
#[test]
fn cost_weight_request_override_flips_winner() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let cheap_low_quality = Model {
        price_input_per_1m: Some(1.0),
        quality_score: Some(0.10),
        ..make_model(brand.id, "cheap", 32_000)
    };
    let pricey_high_quality = Model {
        price_input_per_1m: Some(10.0),
        quality_score: Some(0.99),
        ..make_model(brand.id, "pricey", 32_000)
    };
    db.insert_brand(&brand).unwrap();
    db.insert_model(&cheap_low_quality).unwrap();
    db.insert_model(&pricey_high_quality).unwrap();
    db.insert_rule(&make_rule("chat", cheap_low_quality.id, 0))
        .unwrap();
    db.insert_rule(&make_rule("chat", pricey_high_quality.id, 0))
        .unwrap();

    let sel = selector(db);

    // Default: no weight overrides at all — the expensive, higher-quality model wins.
    let c = sel.select(&base_req()).unwrap();
    assert_eq!(c.model_slug, "pricey");

    // Heavily bias toward cost — the cheap model now wins instead.
    let req = SelectRequest {
        cost_weight: Some(0.7),
        ..base_req()
    };
    let c = sel.select(&req).unwrap();
    assert_eq!(c.model_slug, "cheap");
}

/// A group's own `cost_weight_override` applies when the request sends no override of its own,
/// but a request-level `cost_weight` still wins over the group's when both are set.
#[test]
fn group_weight_override_applies_and_request_takes_precedence() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let cheap_low_quality = Model {
        price_input_per_1m: Some(1.0),
        quality_score: Some(0.10),
        ..make_model(brand.id, "cheap", 32_000)
    };
    let pricey_high_quality = Model {
        price_input_per_1m: Some(10.0),
        quality_score: Some(0.99),
        ..make_model(brand.id, "pricey", 32_000)
    };
    db.insert_brand(&brand).unwrap();
    db.insert_model(&cheap_low_quality).unwrap();
    db.insert_model(&pricey_high_quality).unwrap();

    let group = proviz_elekto_core::models::Group {
        id: Uuid::new_v4(),
        slug: "detector".to_string(),
        name: "detector".to_string(),
        description: None,
        is_active: true,
        created_at: Utc::now(),
        cost_weight_override: Some(0.7),
        latency_weight_override: None,
        quality_weight_override: None,
        sticky_model: false,
    };
    db.insert_group(&group).unwrap();
    // Both members at priority 0 (== "unset", falls back to brand priority) — same brand for
    // both, so the group-priority scoring component ties and isolates the cost-weight effect.
    for model_id in [cheap_low_quality.id, pricey_high_quality.id] {
        db.insert_group_member(&proviz_elekto_core::models::GroupMember {
            id: Uuid::new_v4(),
            group_id: group.id,
            model_id,
            priority: 0,
            is_enabled: true,
        })
        .unwrap();
    }

    let sel = selector(db);

    // No request-level override — the group's own cost bias picks the cheap model.
    let req = SelectRequest {
        group_id: Some(group.id),
        ..base_req()
    };
    let c = sel.select(&req).unwrap();
    assert_eq!(c.model_slug, "cheap");

    // Request explicitly asks for the default cost weight back — overrides the group,
    // so the higher-quality (pricier) model wins again.
    let req = SelectRequest {
        group_id: Some(group.id),
        cost_weight: Some(0.15),
        ..base_req()
    };
    let c = sel.select(&req).unwrap();
    assert_eq!(c.model_slug, "pricey");
}

/// `Group.sticky_model` keeps consecutive calls on the same model for prompt-cache warmth, but
/// yields immediately when that model is rate-limited.
#[test]
fn sticky_model_prefers_last_pick_but_yields_on_rate_limit() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    // Two identical models — base scores tie; member priority is the only differentiator.
    let a = make_model(brand.id, "model-a", 32_000);
    let b = make_model(brand.id, "model-b", 32_000);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&a).unwrap();
    db.insert_model(&b).unwrap();

    let group = proviz_elekto_core::models::Group {
        id: Uuid::new_v4(),
        slug: "detector".to_string(),
        name: "detector".to_string(),
        description: None,
        is_active: true,
        created_at: Utc::now(),
        cost_weight_override: None,
        latency_weight_override: None,
        quality_weight_override: None,
        sticky_model: true,
    };
    db.insert_group(&group).unwrap();
    // B has the better (lower) member priority, so without stickiness B always wins.
    for (model_id, priority) in [(a.id, 1_i16), (b.id, 0_i16)] {
        db.insert_group_member(&proviz_elekto_core::models::GroupMember {
            id: Uuid::new_v4(),
            group_id: group.id,
            model_id,
            priority,
            is_enabled: true,
        })
        .unwrap();
    }

    let sel = selector(db);
    let req = SelectRequest {
        group_id: Some(group.id),
        ..base_req()
    };

    // 1) Block B → A is forced to win, and becomes the sticky model for this group.
    sel.report_rate_limit(
        b.id,
        None,
        RateLimitErrorType::Rpm,
        0,
        None,
        None,
        None,
        None,
    );
    assert_eq!(sel.select(&req).unwrap().model_slug, "model-a");

    // 2) Unblock B. It has the priority edge, but A's sticky bonus outweighs it → A still wins.
    sel.report_success(
        b.id, None, 0, None, None, None, None, None, None, None, None,
    );
    assert_eq!(sel.select(&req).unwrap().model_slug, "model-a");

    // 3) Now block A → the selector must rotate to B despite A being sticky (rate-limited models
    //    are filtered out of the pool entirely before the bonus is ever considered).
    sel.report_rate_limit(
        a.id,
        None,
        RateLimitErrorType::Rpm,
        0,
        None,
        None,
        None,
        None,
    );
    assert_eq!(sel.select(&req).unwrap().model_slug, "model-b");
}

/// A measured per-step quality score outranks the model's global `quality_score` for the
/// step it was recorded against, and has no effect on a different step (falls back to the
/// global column there instead).
#[test]
fn step_quality_overrides_global_only_for_its_own_step() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    // Same price for both — quality is the only thing that should decide the winner here.
    let low_global_high_step = Model {
        quality_score: Some(0.10),
        ..make_model(brand.id, "measured", 32_000)
    };
    let high_global_no_step = Model {
        quality_score: Some(0.90),
        ..make_model(brand.id, "curated", 32_000)
    };
    db.insert_brand(&brand).unwrap();
    db.insert_model(&low_global_high_step).unwrap();
    db.insert_model(&high_global_no_step).unwrap();
    db.insert_rule(&make_rule("chat", low_global_high_step.id, 0))
        .unwrap();
    db.insert_rule(&make_rule("chat", high_global_no_step.id, 0))
        .unwrap();
    db.upsert_step_quality(low_global_high_step.id, "chat", 0.95, 12)
        .unwrap();

    let sel = selector(db);

    // On the measured step, the benchmark score (0.95) beats the curated global one (0.90).
    let c = sel.select(&base_req()).unwrap();
    assert_eq!(c.model_slug, "measured");

    // On a different step with no measured entry, both fall back to the global column —
    // the curated model's higher global quality_score wins instead.
    let req = SelectRequest {
        step: "other_step".to_string(),
        ..base_req()
    };
    let c = sel.select(&req).unwrap();
    assert_eq!(c.model_slug, "curated");
}

#[test]
fn pin_model_selects_only_the_named_model_bypassing_step_rules() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    let a = make_model(brand.id, "acme-7b", 32_000);
    let b = make_model(brand.id, "acme-70b", 32_000);
    db.insert_brand(&brand).unwrap();
    db.insert_model(&a).unwrap();
    db.insert_model(&b).unwrap();
    // Only `a` has a rule for the step; pin must still be able to reach `b`.
    db.insert_rule(&make_rule("chat", a.id, 0)).unwrap();

    let sel = selector(db);

    // brand/slug match reaches a model with no step rule
    let c = sel
        .select(&SelectRequest {
            pin_model: Some("acme/acme-70b".to_string()),
            ..base_req()
        })
        .unwrap();
    assert_eq!(c.model_slug, "acme-70b");

    // brand/slug match, case-insensitive
    let c = sel
        .select(&SelectRequest {
            pin_model: Some("ACME/Acme-7B".to_string()),
            ..base_req()
        })
        .unwrap();
    assert_eq!(c.model_slug, "acme-7b");

    // no match → exhausted. A BARE model slug is not accepted (ambiguous across brands).
    for miss in ["acme/nonesuch", "acme-7b", "acme-70b"] {
        let err = sel
            .select(&SelectRequest {
                pin_model: Some(miss.to_string()),
                ..base_req()
            })
            .unwrap_err();
        assert!(
            matches!(err, ProvizError::AllModelsExhausted { .. }),
            "expected no match for pin {miss:?}"
        );
    }
}

#[test]
fn report_success_discounts_cached_input_tokens() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    // input $1/M, output $2/M, cached input $0.10/M.
    let model = Model {
        price_input_per_1m: Some(1.0),
        price_output_per_1m: Some(2.0),
        price_cached_input_per_1m: Some(0.10),
        ..make_model(brand.id, "cache-model", 32_000)
    };
    let mid = model.id;
    db.insert_brand(&brand).unwrap();
    db.insert_model(&model).unwrap();
    db.insert_rule(&make_rule("chat", mid, 0)).unwrap();
    let sel = selector(db);
    sel.reload().unwrap();

    // 1000 prompt tokens, 800 cache hits, 100 completion:
    // (200*1.0 + 800*0.10 + 100*2.0) / 1e6 = (200 + 80 + 200) / 1e6 = 4.8e-4
    let cost = sel
        .report_success(
            mid,
            None,
            1000,
            None,
            Some(1000),
            Some(100),
            Some(800),
            None,
            None,
            None,
            None,
        )
        .expect("cost computed");
    assert!((cost - 4.8e-4).abs() < 1e-12, "got {cost}");

    // No cache hits → full input rate: (1000*1.0 + 100*2.0)/1e6 = 1.2e-3
    let cost = sel
        .report_success(
            mid,
            None,
            1000,
            None,
            Some(1000),
            Some(100),
            Some(0),
            None,
            None,
            None,
            None,
        )
        .expect("cost computed");
    assert!((cost - 1.2e-3).abs() < 1e-12, "got {cost}");

    // A provider-reported real cost still wins outright over the cached-aware estimate.
    let cost = sel
        .report_success(
            mid,
            None,
            1000,
            None,
            Some(1000),
            Some(100),
            Some(800),
            None,
            None,
            Some(0.42),
            None,
        )
        .expect("cost");
    assert!((cost - 0.42).abs() < 1e-12, "got {cost}");
}
