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
        api_key_env: Some(format!("{}_API_KEY", slug.to_uppercase())),
        base_url: None,
        is_active: true,
        priority,
        created_at: Utc::now(),
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
        quality_min: 0.0,
        exclude_ids: vec![],
        categories: vec![],
        group_id: None,
        group_name: None,
        use_member_priority: true,
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
    sel.report_rate_limit(mid, RateLimitErrorType::Tpm, 0, None);
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
    sel.report_rate_limit(mid, RateLimitErrorType::Tpm, 0, None);
    sel.report_success(mid, 0, None);
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
fn rpm_limit_single_model_exhausted() {
    use proviz_elekto_core::storage::CatalogStorage;
    let db = SqliteStorage::open_in_memory().unwrap();
    let brand = make_brand("acme", 0);
    // rpm_limit=1: last slot is eligible (headroom=0 ≥ 0), then in_flight=1 makes
    // projected=2 → headroom=-1.0 → filtered.
    let tight = Model {
        rpm_limit: Some(1),
        ..make_model(brand.id, "tight", 32_000)
    };
    db.insert_brand(&brand).unwrap();
    db.insert_model(&tight).unwrap();
    db.insert_rule(&make_rule("chat", tight.id, 0)).unwrap();

    let sel = selector(db);
    // First select: last slot (headroom=0.0), not filtered (< 0.0 check)
    let c1 = sel.select(&base_req()).unwrap();
    assert_eq!(c1.model_slug, "tight");

    // Second select: in_flight=1 → projected=2/1 → headroom=-1.0 → AllModelsExhausted
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
    // Before load: scores tie (quality offsets headroom gap), priority breaks tie → primary.
    // After 1 reserve: primary headroom drops to 0.0, backup (headroom=1.0) wins by score.
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
    // Primary and backup tie on score (0.625 each); primary wins via rule priority.
    let c1 = sel.select(&base_req()).unwrap();
    assert_eq!(c1.model_slug, "primary");

    // After reserve: primary headroom=0.0, backup headroom=1.0 → backup wins by score.
    let c2 = sel.select(&base_req()).unwrap();
    assert_eq!(c2.model_slug, "backup");
}
