//! Run against a disposable database: PROVIZ_TEST_PG_URL=postgres://... cargo test -p proviz-elekto-storage-pg
use chrono::Utc;
use proviz_elekto_core::{
    latency::{LatencyHistory, LatencySample},
    models::{Brand, Group, Model},
    storage::CatalogStorage,
};
use proviz_elekto_storage_pg::PostgresStorage;
use uuid::Uuid;
#[test]
fn postgres_speed_columns_and_history_round_trip_and_migrate() {
    let Ok(url) = std::env::var("PROVIZ_TEST_PG_URL") else {
        return;
    };
    let db = PostgresStorage::connect(&url).unwrap();
    let brand = Brand {
        id: Uuid::new_v4(),
        slug: format!("speed-{}", Uuid::new_v4()),
        name: "Speed".into(),
        base_url: None,
        is_active: true,
        priority: 0,
        created_at: Utc::now(),
        traffic_weight: 1.0,
        endpoints: None,
        price_currency: "USD".into(),
    };
    db.insert_brand(&brand).unwrap();
    let model:Model=serde_json::from_value(serde_json::json!({"id":Uuid::new_v4(),"brand_id":brand.id,"slug":"dedicated","display_name":"Dedicated","max_context_tokens":32000,"supports_function_calling":true,"supports_json_mode":true,"is_enabled":true,"created_at":Utc::now(),"max_in_flight":2})).unwrap();
    db.insert_model(&model).unwrap();
    let group = Group {
        id: Uuid::new_v4(),
        slug: format!("speed-{}", Uuid::new_v4()),
        name: "Speed".into(),
        description: None,
        is_active: true,
        created_at: Utc::now(),
        cost_weight_override: None,
        latency_weight_override: None,
        quality_weight_override: None,
        sticky_model: false,
        max_latency_ms: Some(5000),
        max_latency_ratio: Some(2.0),
    };
    db.insert_group(&group).unwrap();
    db.set_group_latency(group.id, Some(4000), Some(1.5))
        .unwrap();
    let history = LatencyHistory {
        model_id: model.id,
        key_id: None,
        samples: vec![LatencySample {
            at: Utc::now().timestamp(),
            input: 1000,
            output: 100,
            elapsed_ms: 1500,
        }],
    };
    db.save_latency_samples(&history).unwrap();
    assert_eq!(
        db.load_model(model.id).unwrap().unwrap().max_in_flight,
        Some(2)
    );
    assert_eq!(
        db.load_groups()
            .unwrap()
            .iter()
            .find(|g| g.id == group.id)
            .unwrap()
            .max_latency_ratio,
        Some(1.5)
    );
    assert!(db
        .load_latency_samples()
        .unwrap()
        .iter()
        .any(|h| h.model_id == model.id && h.samples[0].elapsed_ms == 1500));
    drop(db);
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client.batch_execute("ALTER TABLE pz_models DROP COLUMN max_in_flight; ALTER TABLE pz_groups DROP COLUMN max_latency_ms; ALTER TABLE pz_groups DROP COLUMN max_latency_ratio;").unwrap();
    drop(client);
    for _ in 0..2 {
        let db = PostgresStorage::connect(&url).unwrap();
        assert_eq!(
            db.load_model(model.id).unwrap().unwrap().max_in_flight,
            None
        );
    }
}
