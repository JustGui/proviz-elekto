use chrono::{DateTime, Utc};
use proviz_elekto_core::{
    error::StorageError,
    models::{Brand, Model, RateLimitErrorType, SelectionRule},
    storage::{CatalogStorage, StorageResult},
};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;
use uuid::Uuid;

pub struct SqliteStorage {
    conn: Mutex<Connection>,
}

impl SqliteStorage {
    pub fn open(path: &str) -> Result<Self, StorageError> {
        let conn = Connection::open(path).map_err(|e| StorageError::Database(e.to_string()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let s = Self {
            conn: Mutex::new(conn),
        };
        s.init_schema()?;
        proviz_elekto_core::builtin_providers::seed_if_empty(&s)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(s)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::open(":memory:")
    }
}

impl CatalogStorage for SqliteStorage {
    fn init_schema(&self) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(SCHEMA)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn load_brands(&self) -> StorageResult<Vec<Brand>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id,slug,name,api_key_env,base_url,is_active,plan,priority,created_at FROM pz_brands",
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Brand {
                    id: row.get::<_, String>(0)?.parse::<Uuid>().unwrap(),
                    slug: row.get(1)?,
                    name: row.get(2)?,
                    api_key_env: row.get(3)?,
                    base_url: row.get(4)?,
                    is_active: row.get(5)?,
                    plan: row.get(6)?,
                    priority: row.get::<_, i64>(7)? as i16,
                    created_at: row
                        .get::<_, String>(8)?
                        .parse::<DateTime<Utc>>()
                        .unwrap_or_else(|_| Utc::now()),
                })
            })
            .map_err(|e| StorageError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Database(e.to_string()))
    }

    fn load_models(&self) -> StorageResult<Vec<Model>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,brand_id,slug,display_name,max_context_tokens,max_output_tokens,
             supports_function_calling,supports_json_mode,price_input_per_1m,price_output_per_1m,
             tpm_limit,rpm_limit,rpd_limit,tpd_limit,tpm_limit_month,rps_limit,quality_score,avg_latency_ms,
             is_enabled,notes,category,plan,created_at FROM pz_models",
        ).map_err(|e| StorageError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(Model {
                    id: row.get::<_, String>(0)?.parse::<Uuid>().unwrap(),
                    brand_id: row.get::<_, String>(1)?.parse::<Uuid>().unwrap(),
                    slug: row.get(2)?,
                    display_name: row.get(3)?,
                    max_context_tokens: row.get::<_, i64>(4)? as u32,
                    max_output_tokens: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
                    supports_function_calling: row.get(6)?,
                    supports_json_mode: row.get(7)?,
                    price_input_per_1m: row.get(8)?,
                    price_output_per_1m: row.get(9)?,
                    tpm_limit: row.get::<_, Option<i64>>(10)?.map(|v| v as u32),
                    rpm_limit: row.get::<_, Option<i64>>(11)?.map(|v| v as u32),
                    rpd_limit: row.get::<_, Option<i64>>(12)?.map(|v| v as u32),
                    tpd_limit: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
                    tpm_limit_month: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
                    rps_limit: row.get::<_, Option<f64>>(15)?.map(|v| v as f32),
                    quality_score: row.get::<_, Option<f64>>(16)?.map(|v| v as f32),
                    avg_latency_ms: row.get::<_, Option<i64>>(17)?.map(|v| v as u32),
                    is_enabled: row.get(18)?,
                    notes: row.get(19)?,
                    category: row.get(20)?,
                    plan: row.get(21)?,
                    created_at: row
                        .get::<_, String>(22)?
                        .parse::<DateTime<Utc>>()
                        .unwrap_or_else(|_| Utc::now()),
                })
            })
            .map_err(|e| StorageError::Database(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Database(e.to_string()))
    }

    fn load_selection_rules(&self, step: &str) -> StorageResult<Vec<SelectionRule>> {
        let conn = self.conn.lock().unwrap();
        let (query, use_param) = if step == "*" {
            (
                "SELECT id,step,model_id,priority,max_ctx_tokens,requires_fn_call,is_enabled \
                 FROM pz_selection_rules ORDER BY priority ASC",
                false,
            )
        } else {
            (
                "SELECT id,step,model_id,priority,max_ctx_tokens,requires_fn_call,is_enabled \
                 FROM pz_selection_rules WHERE step=?1 ORDER BY priority ASC",
                true,
            )
        };

        let mut stmt = conn
            .prepare(query)
            .map_err(|e| StorageError::Database(e.to_string()))?;

        let mapper = |row: &rusqlite::Row<'_>| {
            Ok(SelectionRule {
                id: row.get::<_, String>(0)?.parse::<Uuid>().unwrap(),
                step: row.get(1)?,
                model_id: row.get::<_, String>(2)?.parse::<Uuid>().unwrap(),
                priority: row.get::<_, i64>(3)? as i16,
                max_ctx_tokens: row.get::<_, Option<i64>>(4)?.map(|v| v as u32),
                requires_fn_call: row.get(5)?,
                is_enabled: row.get(6)?,
            })
        };

        let rows = if use_param {
            stmt.query_map(params![step], mapper)
        } else {
            stmt.query_map([], mapper)
        }
        .map_err(|e| StorageError::Database(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Database(e.to_string()))
    }

    fn load_model(&self, model_id: Uuid) -> StorageResult<Option<Model>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id,brand_id,slug,display_name,max_context_tokens,max_output_tokens,
             supports_function_calling,supports_json_mode,price_input_per_1m,price_output_per_1m,
             tpm_limit,rpm_limit,rpd_limit,tpd_limit,tpm_limit_month,rps_limit,quality_score,avg_latency_ms,
             is_enabled,notes,category,plan,created_at FROM pz_models WHERE id=?1",
            params![model_id.to_string()],
            |row| {
                Ok(Model {
                    id: row.get::<_, String>(0)?.parse::<Uuid>().unwrap(),
                    brand_id: row.get::<_, String>(1)?.parse::<Uuid>().unwrap(),
                    slug: row.get(2)?,
                    display_name: row.get(3)?,
                    max_context_tokens: row.get::<_, i64>(4)? as u32,
                    max_output_tokens: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
                    supports_function_calling: row.get(6)?,
                    supports_json_mode: row.get(7)?,
                    price_input_per_1m: row.get(8)?,
                    price_output_per_1m: row.get(9)?,
                    tpm_limit: row.get::<_, Option<i64>>(10)?.map(|v| v as u32),
                    rpm_limit: row.get::<_, Option<i64>>(11)?.map(|v| v as u32),
                    rpd_limit: row.get::<_, Option<i64>>(12)?.map(|v| v as u32),
                    tpd_limit: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
                    tpm_limit_month: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
                    rps_limit: row.get::<_, Option<f64>>(15)?.map(|v| v as f32),
                    quality_score: row.get::<_, Option<f64>>(16)?.map(|v| v as f32),
                    avg_latency_ms: row.get::<_, Option<i64>>(17)?.map(|v| v as u32),
                    is_enabled: row.get(18)?,
                    notes: row.get(19)?,
                    category: row.get(20)?,
                    plan: row.get(21)?,
                    created_at: row
                        .get::<_, String>(22)?
                        .parse::<DateTime<Utc>>()
                        .unwrap_or_else(|_| Utc::now()),
                })
            },
        )
        .optional()
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(result)
    }

    fn load_brand(&self, brand_id: Uuid) -> StorageResult<Option<Brand>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id,slug,name,api_key_env,base_url,is_active,plan,priority,created_at FROM pz_brands WHERE id=?1",
            params![brand_id.to_string()],
            |row| {
                Ok(Brand {
                    id: row.get::<_, String>(0)?.parse::<Uuid>().unwrap(),
                    slug: row.get(1)?,
                    name: row.get(2)?,
                    api_key_env: row.get(3)?,
                    base_url: row.get(4)?,
                    is_active: row.get(5)?,
                    plan: row.get(6)?,
                    priority: row.get::<_, i64>(7)? as i16,
                    created_at: row
                        .get::<_, String>(8)?
                        .parse::<DateTime<Utc>>()
                        .unwrap_or_else(|_| Utc::now()),
                })
            },
        )
        .optional()
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(result)
    }

    fn insert_brand(&self, brand: &Brand) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO pz_brands (id,slug,name,api_key_env,base_url,is_active,plan,priority,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                brand.id.to_string(),
                brand.slug,
                brand.name,
                brand.api_key_env,
                brand.base_url,
                brand.is_active,
                brand.plan,
                brand.priority as i64,
                brand.created_at.to_rfc3339(),
            ],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn insert_model(&self, model: &Model) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO pz_models
             (id,brand_id,slug,display_name,max_context_tokens,max_output_tokens,
              supports_function_calling,supports_json_mode,price_input_per_1m,price_output_per_1m,
              tpm_limit,rpm_limit,rpd_limit,tpd_limit,tpm_limit_month,rps_limit,quality_score,avg_latency_ms,
              is_enabled,notes,category,plan,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
            params![
                model.id.to_string(),
                model.brand_id.to_string(),
                model.slug,
                model.display_name,
                model.max_context_tokens as i64,
                model.max_output_tokens.map(|v| v as i64),
                model.supports_function_calling,
                model.supports_json_mode,
                model.price_input_per_1m,
                model.price_output_per_1m,
                model.tpm_limit.map(|v| v as i64),
                model.rpm_limit.map(|v| v as i64),
                model.rpd_limit.map(|v| v as i64),
                model.tpd_limit.map(|v| v as i64),
                model.tpm_limit_month.map(|v| v as i64),
                model.rps_limit.map(|v| v as f64),
                model.quality_score.map(|v| v as f64),
                model.avg_latency_ms.map(|v| v as i64),
                model.is_enabled,
                model.notes,
                model.category,
                model.plan,
                model.created_at.to_rfc3339(),
            ],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn insert_rule(&self, rule: &SelectionRule) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO pz_selection_rules
             (id,step,model_id,priority,max_ctx_tokens,requires_fn_call,is_enabled)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                rule.id.to_string(),
                rule.step,
                rule.model_id.to_string(),
                rule.priority as i64,
                rule.max_ctx_tokens.map(|v| v as i64),
                rule.requires_fn_call,
                rule.is_enabled,
            ],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn delete_rule(&self, rule_id: Uuid) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM pz_selection_rules WHERE id=?1",
            params![rule_id.to_string()],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn set_model_enabled(&self, model_id: Uuid, enabled: bool) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pz_models SET is_enabled=?1 WHERE id=?2",
            params![enabled, model_id.to_string()],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn set_brand_active(&self, brand_id: Uuid, active: bool) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pz_brands SET is_active=?1 WHERE id=?2",
            params![active, brand_id.to_string()],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn log_rate_event(&self, model_id: Uuid, error_type: &RateLimitErrorType) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pz_rate_events (id, model_id, occurred_at, error_type) VALUES (?1,?2,?3,?4)",
            params![
                Uuid::new_v4().to_string(),
                model_id.to_string(),
                Utc::now().to_rfc3339(),
                error_type.to_string(),
            ],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn recent_rate_events(
        &self,
        model_id: Uuid,
        window_secs: u64,
    ) -> StorageResult<Vec<(DateTime<Utc>, RateLimitErrorType)>> {
        let conn = self.conn.lock().unwrap();
        let since = Utc::now() - chrono::Duration::seconds(window_secs as i64);
        let mut stmt = conn
            .prepare(
                "SELECT occurred_at, error_type FROM pz_rate_events
                 WHERE model_id=?1 AND occurred_at>=?2 ORDER BY occurred_at DESC",
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let rows = stmt
            .query_map(params![model_id.to_string(), since.to_rfc3339()], |row| {
                let ts: String = row.get(0)?;
                let et: String = row.get(1)?;
                Ok((ts, et))
            })
            .map_err(|e| StorageError::Database(e.to_string()))?;

        let mut result = Vec::new();
        for row in rows {
            let (ts, et) = row.map_err(|e| StorageError::Database(e.to_string()))?;
            let ts = ts.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now());
            let et = et
                .parse::<RateLimitErrorType>()
                .unwrap_or(RateLimitErrorType::Other);
            result.push((ts, et));
        }
        Ok(result)
    }
}


const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS pz_brands (
    id          TEXT PRIMARY KEY,
    slug        TEXT UNIQUE NOT NULL,
    name        TEXT NOT NULL,
    api_key_env TEXT,
    base_url    TEXT,
    is_active   INTEGER NOT NULL DEFAULT 1,
    plan        TEXT,
    priority    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS pz_models (
    id                        TEXT PRIMARY KEY,
    brand_id                  TEXT NOT NULL REFERENCES pz_brands(id),
    slug                      TEXT NOT NULL,
    display_name              TEXT NOT NULL,
    max_context_tokens        INTEGER NOT NULL,
    max_output_tokens         INTEGER,
    supports_function_calling INTEGER NOT NULL DEFAULT 0,
    supports_json_mode        INTEGER NOT NULL DEFAULT 0,
    price_input_per_1m        REAL,
    price_output_per_1m       REAL,
    tpm_limit                 INTEGER,
    rpm_limit                 INTEGER,
    rpd_limit                 INTEGER,
    tpd_limit                 INTEGER,
    tpm_limit_month           INTEGER,
    rps_limit                 REAL,
    quality_score             REAL,
    avg_latency_ms            INTEGER,
    is_enabled                INTEGER NOT NULL DEFAULT 1,
    notes                     TEXT,
    category                  TEXT,
    plan                      TEXT,
    created_at                TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS pz_selection_rules (
    id               TEXT PRIMARY KEY,
    step             TEXT NOT NULL,
    model_id         TEXT NOT NULL REFERENCES pz_models(id) ON DELETE CASCADE,
    priority         INTEGER NOT NULL,
    max_ctx_tokens   INTEGER,
    requires_fn_call INTEGER NOT NULL DEFAULT 0,
    is_enabled       INTEGER NOT NULL DEFAULT 1,
    UNIQUE(step, model_id)
);

CREATE TABLE IF NOT EXISTS pz_rate_events (
    id          TEXT PRIMARY KEY,
    model_id    TEXT NOT NULL REFERENCES pz_models(id) ON DELETE CASCADE,
    occurred_at TEXT NOT NULL,
    error_type  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pz_rate_events_model_time
    ON pz_rate_events(model_id, occurred_at);

CREATE UNIQUE INDEX IF NOT EXISTS idx_pz_models_slug_plan
    ON pz_models(slug, COALESCE(plan, ''));
";

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use proviz_elekto_core::storage::CatalogStorage;
    use uuid::Uuid;

    fn db() -> SqliteStorage {
        SqliteStorage::open_in_memory().expect("in-memory db")
    }

    fn make_brand(slug: &str) -> Brand {
        Brand {
            id: Uuid::new_v4(),
            slug: slug.to_string(),
            name: slug.to_string(),
            api_key_env: None,
            base_url: None,
            is_active: true,
            plan: None,
            priority: 0,
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
            quality_score: Some(0.8),
            avg_latency_ms: None,
            is_enabled: true,
            notes: None,
            category: None,
            plan: None,
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

    #[test]
    fn insert_and_load_brand() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let brands = db.load_brands().unwrap();
        assert_eq!(brands.len(), 1);
        assert_eq!(brands[0].slug, "acme");
        assert_eq!(brands[0].id, brand.id);
    }

    #[test]
    fn insert_and_load_model() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let model = make_model(brand.id, "acme-7b", 32_000);
        db.insert_model(&model).unwrap();
        let models = db.load_models().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "acme-7b");
        assert_eq!(models[0].max_context_tokens, 32_000);
        assert!(models[0].is_enabled);
    }

    #[test]
    fn insert_and_load_rule() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let model = make_model(brand.id, "acme-7b", 32_000);
        db.insert_model(&model).unwrap();
        let rule = make_rule("chat", model.id, 0);
        db.insert_rule(&rule).unwrap();
        let rules = db.load_selection_rules("chat").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].step, "chat");
        assert_eq!(rules[0].priority, 0);
    }

    #[test]
    fn load_rules_star_returns_all_steps() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let m1 = make_model(brand.id, "acme-7b", 32_000);
        let m2 = make_model(brand.id, "acme-13b", 64_000);
        db.insert_model(&m1).unwrap();
        db.insert_model(&m2).unwrap();
        db.insert_rule(&make_rule("chat", m1.id, 0)).unwrap();
        db.insert_rule(&make_rule("code", m2.id, 0)).unwrap();
        let all = db.load_selection_rules("*").unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn load_rules_by_step_filters() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let m1 = make_model(brand.id, "acme-7b", 32_000);
        let m2 = make_model(brand.id, "acme-13b", 64_000);
        db.insert_model(&m1).unwrap();
        db.insert_model(&m2).unwrap();
        db.insert_rule(&make_rule("chat", m1.id, 0)).unwrap();
        db.insert_rule(&make_rule("code", m2.id, 0)).unwrap();
        let chat_rules = db.load_selection_rules("chat").unwrap();
        assert_eq!(chat_rules.len(), 1);
        assert_eq!(chat_rules[0].step, "chat");
    }

    #[test]
    fn set_model_disabled() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let model = make_model(brand.id, "acme-7b", 32_000);
        db.insert_model(&model).unwrap();
        db.set_model_enabled(model.id, false).unwrap();
        let models = db.load_models().unwrap();
        assert!(!models[0].is_enabled);
    }

    #[test]
    fn set_brand_inactive() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        db.set_brand_active(brand.id, false).unwrap();
        let brands = db.load_brands().unwrap();
        assert!(!brands[0].is_active);
    }

    #[test]
    fn delete_rule() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let model = make_model(brand.id, "acme-7b", 32_000);
        db.insert_model(&model).unwrap();
        let rule = make_rule("chat", model.id, 0);
        db.insert_rule(&rule).unwrap();
        assert_eq!(db.load_selection_rules("*").unwrap().len(), 1);
        db.delete_rule(rule.id).unwrap();
        assert_eq!(db.load_selection_rules("*").unwrap().len(), 0);
    }

    #[test]
    fn log_and_read_rate_event() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let model = make_model(brand.id, "acme-7b", 32_000);
        db.insert_model(&model).unwrap();
        db.log_rate_event(model.id, &RateLimitErrorType::Tpm)
            .unwrap();
        let events = db.recent_rate_events(model.id, 60).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, RateLimitErrorType::Tpm);
    }

    #[test]
    fn rate_events_empty() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let model = make_model(brand.id, "acme-7b", 32_000);
        db.insert_model(&model).unwrap();
        let events = db.recent_rate_events(model.id, 60).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn upsert_brand() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let updated = Brand {
            name: "Acme Corp".to_string(),
            ..brand.clone()
        };
        db.insert_brand(&updated).unwrap();
        let brands = db.load_brands().unwrap();
        assert_eq!(brands.len(), 1);
        assert_eq!(brands[0].name, "Acme Corp");
    }

    #[test]
    fn load_model_by_id() {
        let db = db();
        let brand = make_brand("acme");
        db.insert_brand(&brand).unwrap();
        let model = make_model(brand.id, "acme-7b", 32_000);
        db.insert_model(&model).unwrap();
        assert!(db.load_model(model.id).unwrap().is_some());
        assert!(db.load_model(Uuid::new_v4()).unwrap().is_none());
    }
}
