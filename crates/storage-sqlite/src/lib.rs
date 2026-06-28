use chrono::{DateTime, Utc};
use proviz_elekto_core::{
    error::StorageError,
    models::{Brand, BrandApiKey, Group, GroupMember, Model, RateLimitErrorType, SelectionRule},
    storage::{CatalogStorage, StorageResult},
};
use proviz_elekto_storage_common::{
    brand_api_key_from_row, brand_from_row, group_from_row, group_member_from_row, model_from_row,
    rule_from_row, RowReader, Q_BRANDS, Q_BRAND_API_KEYS, Q_GROUPS, Q_GROUP_MEMBERS, Q_MODELS,
    Q_RULES,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;
use uuid::Uuid;

pub struct SqliteStorage {
    conn: Mutex<Connection>,
}

impl SqliteStorage {
    pub fn open(path: &str) -> Result<Self, StorageError> {
        Self::open_with_providers(path, "./providers")
    }

    pub fn open_with_providers(path: &str, providers_dir: &str) -> Result<Self, StorageError> {
        let s = Self::open_bare(path)?;
        proviz_elekto_core::builtin_providers::seed_if_empty(&s, providers_dir)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(s)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::open_bare(":memory:")
    }

    fn open_bare(path: &str) -> Result<Self, StorageError> {
        let conn = Connection::open(path).map_err(|e| StorageError::Database(e.to_string()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let s = Self {
            conn: Mutex::new(conn),
        };
        s.init_schema()?;
        s.migrate_brand_api_keys()?;
        s.migrate_stt_fields()?;
        s.migrate_endpoints()?;
        Ok(s)
    }

    fn migrate_brand_api_keys(&self) -> Result<(), StorageError> {
        let conn = self.conn.lock().unwrap();
        let has_column: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pz_brands') WHERE name='api_key_env'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);

        if !has_column {
            return Ok(());
        }

        let rows: Vec<(String, String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT id, api_key_env, created_at FROM pz_brands WHERE api_key_env IS NOT NULL",
                )
                .map_err(|e| StorageError::Database(e.to_string()))?;
            let iter = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| StorageError::Database(e.to_string()))?;
            iter.collect::<Result<Vec<_>, _>>()
                .map_err(|e| StorageError::Database(e.to_string()))?
        };

        for (brand_id, api_key_env, created_at) in rows {
            let key_id = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO pz_brand_api_keys (id,brand_id,api_key_env,priority,is_active,created_at)
                 VALUES (?1,?2,?3,0,1,?4)
                 ON CONFLICT(brand_id,api_key_env) DO NOTHING",
                params![key_id, brand_id, api_key_env, created_at],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        }

        conn.execute_batch("ALTER TABLE pz_brands DROP COLUMN api_key_env;")
            .map_err(|e| StorageError::Database(e.to_string()))?;

        Ok(())
    }

    fn migrate_endpoints(&self) -> Result<(), StorageError> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pz_brands') WHERE name='endpoints'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if !exists {
            conn.execute_batch("ALTER TABLE pz_brands ADD COLUMN endpoints TEXT;")
                .map_err(|e| StorageError::Database(e.to_string()))?;
        }
        Ok(())
    }

    fn migrate_stt_fields(&self) -> Result<(), StorageError> {
        let conn = self.conn.lock().unwrap();
        for col in &["diarization", "streaming", "http_batch", "word_timestamps"] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('pz_models') WHERE name=?1",
                    [col],
                    |row| row.get::<_, i64>(0),
                )
                .map(|n| n > 0)
                .unwrap_or(false);
            if !exists {
                conn.execute_batch(&format!("ALTER TABLE pz_models ADD COLUMN {col} INTEGER;"))
                    .map_err(|e| StorageError::Database(e.to_string()))?;
            }
        }
        Ok(())
    }
}

struct SqliteRow<'a>(&'a rusqlite::Row<'a>);

impl RowReader for SqliteRow<'_> {
    fn uuid(&self, idx: usize) -> Uuid {
        self.0.get::<_, String>(idx).unwrap().parse().unwrap()
    }
    fn string(&self, idx: usize) -> String {
        self.0.get(idx).unwrap()
    }
    fn opt_string(&self, idx: usize) -> Option<String> {
        self.0.get(idx).unwrap()
    }
    fn bool_val(&self, idx: usize) -> bool {
        self.0.get(idx).unwrap()
    }
    fn opt_bool(&self, idx: usize) -> Option<bool> {
        self.0.get::<_, Option<i64>>(idx).unwrap().map(|v| v != 0)
    }
    fn i16_val(&self, idx: usize) -> i16 {
        self.0.get::<_, i64>(idx).unwrap() as i16
    }
    fn i32_val(&self, idx: usize) -> i32 {
        self.0.get::<_, i64>(idx).unwrap() as i32
    }
    fn opt_i32(&self, idx: usize) -> Option<i32> {
        self.0.get::<_, Option<i64>>(idx).unwrap().map(|v| v as i32)
    }
    fn opt_i64(&self, idx: usize) -> Option<i64> {
        self.0.get(idx).unwrap()
    }
    fn opt_f64(&self, idx: usize) -> Option<f64> {
        self.0.get(idx).unwrap()
    }
    fn datetime(&self, idx: usize) -> DateTime<Utc> {
        self.0
            .get::<_, String>(idx)
            .unwrap()
            .parse()
            .unwrap_or_else(|_| Utc::now())
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
            .prepare(Q_BRANDS)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| Ok(brand_from_row(&SqliteRow(row))))
            .map_err(|e| StorageError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Database(e.to_string()))
    }

    fn load_models(&self) -> StorageResult<Vec<Model>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(Q_MODELS)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| Ok(model_from_row(&SqliteRow(row))))
            .map_err(|e| StorageError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Database(e.to_string()))
    }

    fn load_selection_rules(&self, step: &str) -> StorageResult<Vec<SelectionRule>> {
        let conn = self.conn.lock().unwrap();
        let (query, use_param) = if step == "*" {
            (format!("{Q_RULES} ORDER BY priority ASC"), false)
        } else {
            (
                format!("{Q_RULES} WHERE step=?1 ORDER BY priority ASC"),
                true,
            )
        };
        let mut stmt = conn
            .prepare(&query)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let mapper = |row: &rusqlite::Row<'_>| Ok(rule_from_row(&SqliteRow(row)));
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
        let query = format!("{Q_MODELS} WHERE id=?1");
        let result = conn
            .query_row(&query, params![model_id.to_string()], |row| {
                Ok(model_from_row(&SqliteRow(row)))
            })
            .optional()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(result)
    }

    fn load_brand(&self, brand_id: Uuid) -> StorageResult<Option<Brand>> {
        let conn = self.conn.lock().unwrap();
        let query = format!("{Q_BRANDS} WHERE id=?1");
        let result = conn
            .query_row(&query, params![brand_id.to_string()], |row| {
                Ok(brand_from_row(&SqliteRow(row)))
            })
            .optional()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(result)
    }

    fn insert_brand(&self, brand: &Brand) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        let endpoints_json = brand
            .endpoints
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        conn.execute(
            "INSERT INTO pz_brands (id,slug,name,base_url,is_active,priority,created_at,traffic_weight,endpoints)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(slug) DO UPDATE SET
               name=excluded.name, base_url=excluded.base_url,
               is_active=excluded.is_active, priority=excluded.priority,
               traffic_weight=excluded.traffic_weight,
               endpoints=excluded.endpoints",
            params![
                brand.id.to_string(),
                brand.slug,
                brand.name,
                brand.base_url,
                brand.is_active,
                brand.priority as i64,
                brand.created_at.to_rfc3339(),
                brand.traffic_weight,
                endpoints_json,
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
              is_enabled,notes,category,created_at,batch_price_multiplier,
              diarization,streaming,http_batch,word_timestamps)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)",
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
                model.created_at.to_rfc3339(),
                model.batch_price_multiplier,
                model.diarization.map(|v| v as i64),
                model.streaming.map(|v| v as i64),
                model.http_batch.map(|v| v as i64),
                model.word_timestamps.map(|v| v as i64),
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

    fn sync_model_limits(
        &self,
        model_id: Uuid,
        rpm: Option<u32>,
        tpm: Option<u32>,
    ) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pz_models \
             SET rpm_limit = COALESCE(?1, rpm_limit), \
                 tpm_limit = COALESCE(?2, tpm_limit) \
             WHERE id = ?3",
            params![rpm, tpm, model_id.to_string()],
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

    fn load_groups(&self) -> StorageResult<Vec<Group>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(Q_GROUPS)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| Ok(group_from_row(&SqliteRow(row))))
            .map_err(|e| StorageError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Database(e.to_string()))
    }

    fn load_all_group_members(&self) -> StorageResult<Vec<GroupMember>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(Q_GROUP_MEMBERS)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| Ok(group_member_from_row(&SqliteRow(row))))
            .map_err(|e| StorageError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Database(e.to_string()))
    }

    fn insert_group(&self, group: &Group) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pz_groups (id,slug,name,description,is_active,created_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(slug) DO UPDATE SET
               name=excluded.name, description=excluded.description, is_active=excluded.is_active",
            params![
                group.id.to_string(),
                group.slug,
                group.name,
                group.description,
                group.is_active,
                group.created_at.to_rfc3339(),
            ],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn delete_group(&self, group_id: Uuid) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM pz_groups WHERE id=?1",
            params![group_id.to_string()],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn set_group_active(&self, group_id: Uuid, active: bool) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pz_groups SET is_active=?1 WHERE id=?2",
            params![active, group_id.to_string()],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn insert_group_member(&self, member: &GroupMember) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pz_group_members (id,group_id,model_id,priority,is_enabled)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(group_id,model_id) DO UPDATE SET
               priority=excluded.priority, is_enabled=excluded.is_enabled",
            params![
                member.id.to_string(),
                member.group_id.to_string(),
                member.model_id.to_string(),
                member.priority as i64,
                member.is_enabled,
            ],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn remove_group_member(&self, group_id: Uuid, model_id: Uuid) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM pz_group_members WHERE group_id=?1 AND model_id=?2",
            params![group_id.to_string(), model_id.to_string()],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn insert_brand_api_key(&self, key: &BrandApiKey) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pz_brand_api_keys (id,brand_id,api_key_env,priority,is_active,created_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(brand_id,api_key_env) DO UPDATE SET
               priority=excluded.priority, is_active=excluded.is_active",
            params![
                key.id.to_string(),
                key.brand_id.to_string(),
                key.api_key_env,
                key.priority,
                key.is_active as i32,
                key.created_at.to_rfc3339(),
            ],
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn load_all_brand_api_keys(&self) -> StorageResult<Vec<BrandApiKey>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(Q_BRAND_API_KEYS)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| Ok(brand_api_key_from_row(&SqliteRow(row))))
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| StorageError::Database(e.to_string()))?);
        }
        Ok(result)
    }

    fn delete_brand_api_key(&self, key_id: Uuid) -> StorageResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM pz_brand_api_keys WHERE id=?1",
            params![key_id.to_string()],
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
    id             TEXT PRIMARY KEY,
    slug           TEXT UNIQUE NOT NULL,
    name           TEXT NOT NULL,
    base_url       TEXT,
    is_active      INTEGER NOT NULL DEFAULT 1,
    priority       INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    traffic_weight REAL NOT NULL DEFAULT 1.0,
    endpoints      TEXT
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
    created_at                TEXT NOT NULL DEFAULT (datetime('now')),
    batch_price_multiplier    REAL,
    diarization               INTEGER,
    streaming                 INTEGER,
    http_batch                INTEGER,
    word_timestamps           INTEGER
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

CREATE TABLE IF NOT EXISTS pz_groups (
    id          TEXT PRIMARY KEY,
    slug        TEXT UNIQUE NOT NULL,
    name        TEXT NOT NULL,
    description TEXT,
    is_active   INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS pz_group_members (
    id         TEXT PRIMARY KEY,
    group_id   TEXT NOT NULL REFERENCES pz_groups(id) ON DELETE CASCADE,
    model_id   TEXT NOT NULL REFERENCES pz_models(id) ON DELETE CASCADE,
    priority   INTEGER NOT NULL DEFAULT 0,
    is_enabled INTEGER NOT NULL DEFAULT 1,
    UNIQUE(group_id, model_id)
);

CREATE INDEX IF NOT EXISTS idx_pz_group_members_group
    ON pz_group_members(group_id);

CREATE TABLE IF NOT EXISTS pz_rate_events (
    id          TEXT PRIMARY KEY,
    model_id    TEXT NOT NULL REFERENCES pz_models(id) ON DELETE CASCADE,
    occurred_at TEXT NOT NULL,
    error_type  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pz_rate_events_model_time
    ON pz_rate_events(model_id, occurred_at);

CREATE UNIQUE INDEX IF NOT EXISTS idx_pz_models_brand_slug
    ON pz_models(brand_id, slug);

CREATE TABLE IF NOT EXISTS pz_brand_api_keys (
    id          TEXT PRIMARY KEY,
    brand_id    TEXT NOT NULL REFERENCES pz_brands(id) ON DELETE CASCADE,
    api_key_env TEXT NOT NULL,
    priority    INTEGER NOT NULL DEFAULT 0,
    is_active   INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(brand_id, api_key_env)
);

CREATE INDEX IF NOT EXISTS idx_pz_brand_api_keys_brand
    ON pz_brand_api_keys(brand_id);
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
            base_url: None,
            is_active: true,
            priority: 0,
            created_at: Utc::now(),
            traffic_weight: 1.0,
            endpoints: None,
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
            created_at: Utc::now(),
            batch_price_multiplier: None,
            diarization: None,
            streaming: None,
            http_batch: None,
            word_timestamps: None,
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
