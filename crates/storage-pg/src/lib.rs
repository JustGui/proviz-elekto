use chrono::{DateTime, Utc};
use postgres::{Client, NoTls};
use proviz_elekto_core::{
    error::StorageError,
    models::{
        Brand, BrandApiKey, Group, GroupMember, Model, ModelCatalogEntry, RateLimitErrorType,
        SelectionRule,
    },
    storage::{CatalogStorage, StorageResult},
};
use proviz_elekto_storage_common::{
    brand_api_key_from_row, brand_from_row, group_from_row, group_member_from_row,
    model_catalog_from_row, model_from_row, rule_from_row, RowReader, Q_BRANDS, Q_BRAND_API_KEYS,
    Q_GROUPS, Q_GROUP_MEMBERS, Q_MODELS, Q_MODEL_CATALOG, Q_RULES,
};
use std::sync::Mutex;
use uuid::Uuid;

pub struct PostgresStorage {
    client: Mutex<Client>,
    /// Kept so a dead connection can be transparently re-established (see `connected_client()`)
    /// instead of permanently failing every query with "connection closed" until the whole
    /// process is restarted by hand — the same class of bug already fixed in RTFC's listener
    /// (`SessionStore::get_pg()`, see rtfc CLAUDE.md "Postgres crash-restart") and still open in
    /// rtfc's worker-rs `Pg::connect()`. `PostgresStorage::connect()` previously dialed ONCE at
    /// startup with no retry — any transient postgres restart/network blip after that point (not
    /// just at boot) permanently 500'd every `/select` and `/complete` call for every caller
    /// (detector, worker, elector) until proviz-elekto itself was restarted.
    database_url: String,
}

impl PostgresStorage {
    pub fn connect(database_url: &str) -> Result<Self, StorageError> {
        Self::connect_with_providers(database_url, "./providers")
    }

    pub fn connect_with_providers(
        database_url: &str,
        providers_dir: &str,
    ) -> Result<Self, StorageError> {
        let client = Client::connect(database_url, NoTls)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let s = Self {
            client: Mutex::new(client),
            database_url: database_url.to_string(),
        };
        s.init_schema()?;
        s.migrate_brand_api_keys()?;
        s.migrate_stt_fields()?;
        s.migrate_endpoints()?;
        s.migrate_supported_languages()?;
        s.migrate_streaming_variant_index()?;
        s.migrate_reasoning_effort()?;
        s.migrate_model_catalog_fields()?;
        s.migrate_privacy_fields()?;
        proviz_elekto_core::builtin_providers::seed_if_empty(&s, providers_dir)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(s)
    }

    /// Every query goes through this instead of locking `self.client` directly. `is_closed()` is
    /// a cheap local check (no round-trip) reflecting whether a PREVIOUS operation already
    /// observed the connection drop — cheaper than pinging on every call, and matches the
    /// pattern already validated in rtfc's listener. Reconnect is attempted inline and the lock
    /// is held across it, so concurrent callers block briefly rather than each independently
    /// racing to redial.
    fn connected_client(&self) -> Result<std::sync::MutexGuard<'_, Client>, StorageError> {
        let mut guard = self.client.lock().expect("client mutex poisoned");
        if guard.is_closed() {
            let fresh = Client::connect(&self.database_url, NoTls)
                .map_err(|e| StorageError::Database(e.to_string()))?;
            *guard = fresh;
        }
        Ok(guard)
    }

    fn migrate_brand_api_keys(&self) -> Result<(), StorageError> {
        let mut client = self.connected_client()?;
        let has_column = client
            .query_one(
                "SELECT COUNT(*) FROM information_schema.columns \
                 WHERE table_name='pz_brands' AND column_name='api_key_env'",
                &[],
            )
            .map(|row| row.get::<_, i64>(0) > 0)
            .unwrap_or(false);

        if !has_column {
            return Ok(());
        }

        client
            .batch_execute(
                "INSERT INTO pz_brand_api_keys (brand_id,api_key_env,priority,is_active,created_at)
                 SELECT id, api_key_env, 0, TRUE, created_at
                 FROM pz_brands
                 WHERE api_key_env IS NOT NULL
                 ON CONFLICT (brand_id,api_key_env) DO NOTHING;

                 ALTER TABLE pz_brands DROP COLUMN api_key_env;",
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;

        Ok(())
    }

    fn migrate_endpoints(&self) -> Result<(), StorageError> {
        let mut client = self.connected_client()?;
        client
            .batch_execute("ALTER TABLE pz_brands ADD COLUMN IF NOT EXISTS endpoints TEXT;")
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn migrate_stt_fields(&self) -> Result<(), StorageError> {
        let mut client = self.connected_client()?;
        client
            .batch_execute(
                "ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS diarization BOOLEAN;\
                 ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS streaming BOOLEAN;\
                 ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS http_batch BOOLEAN;\
                 ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS word_timestamps BOOLEAN;\
                 ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS base_url VARCHAR(255);",
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn migrate_supported_languages(&self) -> Result<(), StorageError> {
        let mut client = self.connected_client()?;
        client
            .batch_execute(
                "ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS supported_languages TEXT;",
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    /// Widens the (brand_id, slug) uniqueness to (brand_id, slug, streaming, http_batch) so
    /// STT models can have a distinct row per call mode (e.g. streaming vs HTTP batch) with
    /// their own base_url/rpm_limit/price. Backfills NULL streaming/http_batch to FALSE first
    /// and adds the NOT NULL/DEFAULT constraints so future rows can't collide via NULL != NULL.
    fn migrate_streaming_variant_index(&self) -> Result<(), StorageError> {
        let mut client = self.connected_client()?;
        client
            .batch_execute(
                "UPDATE pz_models SET streaming=FALSE WHERE streaming IS NULL;
                 UPDATE pz_models SET http_batch=FALSE WHERE http_batch IS NULL;
                 ALTER TABLE pz_models ALTER COLUMN streaming SET DEFAULT FALSE;
                 ALTER TABLE pz_models ALTER COLUMN streaming SET NOT NULL;
                 ALTER TABLE pz_models ALTER COLUMN http_batch SET DEFAULT FALSE;
                 ALTER TABLE pz_models ALTER COLUMN http_batch SET NOT NULL;
                 DROP INDEX IF EXISTS idx_pz_models_brand_slug;
                 CREATE UNIQUE INDEX idx_pz_models_brand_slug
                     ON pz_models(brand_id, slug, streaming, http_batch);",
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    /// Replaces the old bool `supports_reasoning_effort` column with `reasoning_effort_value TEXT`:
    /// acceptance of the param isn't the same as it being effective (some models accept every
    /// literal without erroring while only one actually reduces reasoning), so a bool can't carry
    /// which value to send — only the exact literal can. NULL means never send the param.
    fn migrate_reasoning_effort(&self) -> Result<(), StorageError> {
        let mut client = self.connected_client()?;
        client
            .batch_execute(
                "ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS reasoning_effort_value TEXT;
                 ALTER TABLE pz_models DROP COLUMN IF EXISTS supports_reasoning_effort;",
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    /// Adds `canonical_key` (shared model-catalog lookup key) and `price_synced_at` (freshness
    /// timestamp for API-synced providers like OpenRouter) to `pz_models`. Both nullable — no
    /// backfill needed, existing rows simply have neither set.
    fn migrate_model_catalog_fields(&self) -> Result<(), StorageError> {
        let mut client = self.connected_client()?;
        client
            .batch_execute(
                "ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS canonical_key TEXT;\
                 ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS price_synced_at TIMESTAMPTZ;",
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    /// Adds `trains_on_data`/`retains_data` (provider-reported privacy facts, e.g. Requesty's
    /// `data_used_for_training`/`data_retention`) to `pz_models`. Both nullable — `NULL` means
    /// the source doesn't report it, the common case for most providers.
    fn migrate_privacy_fields(&self) -> Result<(), StorageError> {
        let mut client = self.connected_client()?;
        client
            .batch_execute(
                "ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS trains_on_data BOOLEAN;\
                 ALTER TABLE pz_models ADD COLUMN IF NOT EXISTS retains_data BOOLEAN;",
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }
}

struct PgRow<'a>(&'a postgres::Row);

impl RowReader for PgRow<'_> {
    fn uuid(&self, idx: usize) -> Uuid {
        self.0.get(idx)
    }
    fn string(&self, idx: usize) -> String {
        self.0.get(idx)
    }
    fn opt_string(&self, idx: usize) -> Option<String> {
        self.0.get(idx)
    }
    fn bool_val(&self, idx: usize) -> bool {
        self.0.get(idx)
    }
    fn opt_bool(&self, idx: usize) -> Option<bool> {
        self.0.get(idx)
    }
    fn i16_val(&self, idx: usize) -> i16 {
        self.0.get(idx)
    }
    fn i32_val(&self, idx: usize) -> i32 {
        self.0.get(idx)
    }
    fn opt_i32(&self, idx: usize) -> Option<i32> {
        self.0.get(idx)
    }
    fn opt_i64(&self, idx: usize) -> Option<i64> {
        self.0.get(idx)
    }
    fn opt_f64(&self, idx: usize) -> Option<f64> {
        self.0.get(idx)
    }
    fn datetime(&self, idx: usize) -> DateTime<Utc> {
        self.0.get(idx)
    }
    fn opt_datetime(&self, idx: usize) -> Option<DateTime<Utc>> {
        self.0.get(idx)
    }
}

impl CatalogStorage for PostgresStorage {
    fn init_schema(&self) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .batch_execute(SCHEMA)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn load_brands(&self) -> StorageResult<Vec<Brand>> {
        let mut client = self.connected_client()?;
        let rows = client
            .query(Q_BRANDS, &[])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(rows.iter().map(|row| brand_from_row(&PgRow(row))).collect())
    }

    fn load_models(&self) -> StorageResult<Vec<Model>> {
        let mut client = self.connected_client()?;
        let rows = client
            .query(Q_MODELS, &[])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(rows.iter().map(|row| model_from_row(&PgRow(row))).collect())
    }

    fn load_selection_rules(&self, step: &str) -> StorageResult<Vec<SelectionRule>> {
        let mut client = self.connected_client()?;
        let rows = if step == "*" {
            client.query(&format!("{Q_RULES} ORDER BY priority ASC"), &[])
        } else {
            client.query(
                &format!("{Q_RULES} WHERE step=$1 ORDER BY priority ASC"),
                &[&step],
            )
        }
        .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(rows.iter().map(|row| rule_from_row(&PgRow(row))).collect())
    }

    fn load_model(&self, model_id: Uuid) -> StorageResult<Option<Model>> {
        let mut client = self.connected_client()?;
        let row = client
            .query_opt(&format!("{Q_MODELS} WHERE id=$1"), &[&model_id])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(row.map(|row| model_from_row(&PgRow(&row))))
    }

    fn load_brand(&self, brand_id: Uuid) -> StorageResult<Option<Brand>> {
        let mut client = self.connected_client()?;
        let row = client
            .query_opt(&format!("{Q_BRANDS} WHERE id=$1"), &[&brand_id])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(row.map(|row| brand_from_row(&PgRow(&row))))
    }

    fn insert_brand(&self, brand: &Brand) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        let endpoints_json: Option<String> = brand
            .endpoints
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        client.execute(
            "INSERT INTO pz_brands (id,slug,name,base_url,is_active,priority,created_at,traffic_weight,endpoints)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
             ON CONFLICT (slug) DO UPDATE SET
               name=EXCLUDED.name, base_url=EXCLUDED.base_url,
               is_active=EXCLUDED.is_active, priority=EXCLUDED.priority,
               traffic_weight=EXCLUDED.traffic_weight,
               endpoints=EXCLUDED.endpoints",
            &[&brand.id, &brand.slug, &brand.name, &brand.base_url, &brand.is_active, &brand.priority, &brand.created_at, &brand.traffic_weight, &endpoints_json],
        ).map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn insert_model(&self, model: &Model) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client.execute(
            "INSERT INTO pz_models
             (id,brand_id,slug,display_name,max_context_tokens,max_output_tokens,
              supports_function_calling,supports_json_mode,price_input_per_1m,price_output_per_1m,
              tpm_limit,rpm_limit,rpd_limit,tpd_limit,tpm_limit_month,rps_limit,quality_score,avg_latency_ms,
              is_enabled,notes,category,created_at,batch_price_multiplier,
              diarization,streaming,http_batch,word_timestamps, base_url, supported_languages,
              reasoning_effort_value,canonical_key,price_synced_at,trains_on_data,retains_data)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$30,$31,$32,$33,$34)
             ON CONFLICT (id) DO UPDATE SET
               slug=EXCLUDED.slug, display_name=EXCLUDED.display_name,
               max_context_tokens=EXCLUDED.max_context_tokens,
               supports_function_calling=EXCLUDED.supports_function_calling,
               supports_json_mode=EXCLUDED.supports_json_mode,
               price_input_per_1m=EXCLUDED.price_input_per_1m,
               price_output_per_1m=EXCLUDED.price_output_per_1m,
               tpm_limit=EXCLUDED.tpm_limit, rpm_limit=EXCLUDED.rpm_limit, rpd_limit=EXCLUDED.rpd_limit,
               quality_score=EXCLUDED.quality_score, is_enabled=EXCLUDED.is_enabled,
               category=EXCLUDED.category,
               batch_price_multiplier=EXCLUDED.batch_price_multiplier,
               diarization=EXCLUDED.diarization, streaming=EXCLUDED.streaming,
               http_batch=EXCLUDED.http_batch, word_timestamps=EXCLUDED.word_timestamps, base_url=EXCLUDED.base_url,
               supported_languages=EXCLUDED.supported_languages,
               reasoning_effort_value=EXCLUDED.reasoning_effort_value,
               canonical_key=EXCLUDED.canonical_key,
               price_synced_at=EXCLUDED.price_synced_at,
               trains_on_data=EXCLUDED.trains_on_data,
               retains_data=EXCLUDED.retains_data",
            &[
                &model.id, &model.brand_id, &model.slug, &model.display_name,
                &(model.max_context_tokens as i32),
                &model.max_output_tokens.map(|v| v as i32),
                &model.supports_function_calling, &model.supports_json_mode,
                &model.price_input_per_1m, &model.price_output_per_1m,
                &model.tpm_limit.map(|v| v as i32),
                &model.rpm_limit.map(|v| v as i32),
                &model.rpd_limit.map(|v| v as i32),
                &model.tpd_limit.map(|v| v as i64),
                &model.tpm_limit_month.map(|v| v as i64),
                &model.rps_limit.map(|v| v as f64),
                &model.quality_score.map(|v| v as f64),
                &model.avg_latency_ms.map(|v| v as i32),
                &model.is_enabled, &model.notes, &model.category, &model.created_at,
                &model.batch_price_multiplier,
                &model.diarization, &model.streaming.unwrap_or(false), &model.http_batch.unwrap_or(false), &model.word_timestamps, &model.base_url,
                &model
                    .supported_languages
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default()),
                &model.reasoning_effort_value,
                &model.canonical_key,
                &model.price_synced_at,
                &model.trains_on_data,
                &model.retains_data,
            ],
        ).map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn load_model_catalog(&self) -> StorageResult<Vec<ModelCatalogEntry>> {
        let mut client = self.connected_client()?;
        let rows = client
            .query(Q_MODEL_CATALOG, &[])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(rows
            .iter()
            .map(|row| model_catalog_from_row(&PgRow(row)))
            .collect())
    }

    fn insert_model_catalog_entry(&self, entry: &ModelCatalogEntry) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client.execute(
            "INSERT INTO pz_model_catalog
             (id,canonical_key,display_name,category,max_context_tokens,
              supports_function_calling,supports_json_mode,quality_score,knowledge_cutoff,created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
             ON CONFLICT (canonical_key) DO UPDATE SET
               display_name=EXCLUDED.display_name, category=EXCLUDED.category,
               max_context_tokens=EXCLUDED.max_context_tokens,
               supports_function_calling=EXCLUDED.supports_function_calling,
               supports_json_mode=EXCLUDED.supports_json_mode,
               quality_score=EXCLUDED.quality_score,
               knowledge_cutoff=EXCLUDED.knowledge_cutoff",
            &[
                &entry.id,
                &entry.canonical_key,
                &entry.display_name,
                &entry.category,
                &entry.max_context_tokens.map(|v| v as i32),
                &entry.supports_function_calling,
                &entry.supports_json_mode,
                &entry.quality_score,
                &entry.knowledge_cutoff,
                &entry.created_at,
            ],
        ).map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn insert_rule(&self, rule: &SelectionRule) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client.execute(
            "INSERT INTO pz_selection_rules (id,step,model_id,priority,max_ctx_tokens,requires_fn_call,is_enabled)
             VALUES ($1,$2,$3,$4,$5,$6,$7)
             ON CONFLICT (step, model_id) DO UPDATE SET
               priority=EXCLUDED.priority, max_ctx_tokens=EXCLUDED.max_ctx_tokens,
               requires_fn_call=EXCLUDED.requires_fn_call, is_enabled=EXCLUDED.is_enabled",
            &[
                &rule.id, &rule.step, &rule.model_id,
                &rule.priority,
                &rule.max_ctx_tokens.map(|v| v as i32),
                &rule.requires_fn_call, &rule.is_enabled,
            ],
        ).map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn delete_rule(&self, rule_id: Uuid) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .execute("DELETE FROM pz_selection_rules WHERE id=$1", &[&rule_id])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn set_model_enabled(&self, model_id: Uuid, enabled: bool) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .execute(
                "UPDATE pz_models SET is_enabled=$1 WHERE id=$2",
                &[&enabled, &model_id],
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
        let mut client = self.connected_client()?;
        let rpm_i = rpm.map(|v| v as i32);
        let tpm_i = tpm.map(|v| v as i32);
        client
            .execute(
                "UPDATE pz_models \
                 SET rpm_limit = COALESCE($1, rpm_limit), \
                     tpm_limit = COALESCE($2, tpm_limit) \
                 WHERE id = $3",
                &[&rpm_i, &tpm_i, &model_id],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn set_brand_active(&self, brand_id: Uuid, active: bool) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .execute(
                "UPDATE pz_brands SET is_active=$1 WHERE id=$2",
                &[&active, &brand_id],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn load_groups(&self) -> StorageResult<Vec<Group>> {
        let mut client = self.connected_client()?;
        let rows = client
            .query(Q_GROUPS, &[])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(rows.iter().map(|row| group_from_row(&PgRow(row))).collect())
    }

    fn load_all_group_members(&self) -> StorageResult<Vec<GroupMember>> {
        let mut client = self.connected_client()?;
        let rows = client
            .query(Q_GROUP_MEMBERS, &[])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(rows
            .iter()
            .map(|row| group_member_from_row(&PgRow(row)))
            .collect())
    }

    fn insert_group(&self, group: &Group) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .execute(
                "INSERT INTO pz_groups (id,slug,name,description,is_active,created_at)
                 VALUES ($1,$2,$3,$4,$5,$6)
                 ON CONFLICT (slug) DO UPDATE SET
                   name=EXCLUDED.name, description=EXCLUDED.description,
                   is_active=EXCLUDED.is_active",
                &[
                    &group.id,
                    &group.slug,
                    &group.name,
                    &group.description,
                    &group.is_active,
                    &group.created_at,
                ],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn delete_group(&self, group_id: Uuid) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .execute("DELETE FROM pz_groups WHERE id=$1", &[&group_id])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn set_group_active(&self, group_id: Uuid, active: bool) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .execute(
                "UPDATE pz_groups SET is_active=$1 WHERE id=$2",
                &[&active, &group_id],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn insert_group_member(&self, member: &GroupMember) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .execute(
                "INSERT INTO pz_group_members (id,group_id,model_id,priority,is_enabled)
                 VALUES ($1,$2,$3,$4,$5)
                 ON CONFLICT (group_id, model_id) DO UPDATE SET
                   priority=EXCLUDED.priority, is_enabled=EXCLUDED.is_enabled",
                &[
                    &member.id,
                    &member.group_id,
                    &member.model_id,
                    &member.priority,
                    &member.is_enabled,
                ],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn remove_group_member(&self, group_id: Uuid, model_id: Uuid) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .execute(
                "DELETE FROM pz_group_members WHERE group_id=$1 AND model_id=$2",
                &[&group_id, &model_id],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn insert_brand_api_key(&self, key: &BrandApiKey) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        let env = key.api_key_env.clone();
        client
            .execute(
                "INSERT INTO pz_brand_api_keys (id,brand_id,api_key_env,priority,is_active,created_at)
                 VALUES ($1,$2,$3,$4,$5,$6)
                 ON CONFLICT (brand_id,api_key_env) DO UPDATE SET
                   priority=EXCLUDED.priority, is_active=EXCLUDED.is_active",
                &[
                    &key.id,
                    &key.brand_id,
                    &env,
                    &key.priority,
                    &key.is_active,
                    &key.created_at,
                ],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn load_all_brand_api_keys(&self) -> StorageResult<Vec<BrandApiKey>> {
        let mut client = self.connected_client()?;
        let rows = client
            .query(Q_BRAND_API_KEYS, &[])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(rows
            .iter()
            .map(|row| brand_api_key_from_row(&PgRow(row)))
            .collect())
    }

    fn delete_brand_api_key(&self, key_id: Uuid) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        client
            .execute("DELETE FROM pz_brand_api_keys WHERE id=$1", &[&key_id])
            .map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn log_rate_event(&self, model_id: Uuid, error_type: &RateLimitErrorType) -> StorageResult<()> {
        let mut client = self.connected_client()?;
        let id = Uuid::new_v4();
        let now = Utc::now();
        let et = error_type.to_string();
        client.execute(
            "INSERT INTO pz_rate_events (id,model_id,occurred_at,error_type) VALUES ($1,$2,$3,$4)",
            &[&id, &model_id, &now, &et],
        ).map_err(|e| StorageError::Database(e.to_string()))?;
        Ok(())
    }

    fn recent_rate_events(
        &self,
        model_id: Uuid,
        window_secs: u64,
    ) -> StorageResult<Vec<(DateTime<Utc>, RateLimitErrorType)>> {
        let mut client = self.connected_client()?;
        let since = Utc::now() - chrono::Duration::seconds(window_secs as i64);
        let rows = client
            .query(
                "SELECT occurred_at,error_type FROM pz_rate_events
             WHERE model_id=$1 AND occurred_at>=$2 ORDER BY occurred_at DESC",
                &[&model_id, &since],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;

        Ok(rows
            .iter()
            .map(|row| {
                let ts: DateTime<Utc> = row.get(0);
                let et: String = row.get(1);
                let et = et
                    .parse::<RateLimitErrorType>()
                    .unwrap_or(RateLimitErrorType::Other);
                (ts, et)
            })
            .collect())
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS pz_brands (
    id             UUID             PRIMARY KEY DEFAULT gen_random_uuid(),
    slug           VARCHAR(50)      UNIQUE NOT NULL,
    name           VARCHAR(100)     NOT NULL,
    base_url       VARCHAR(255),
    is_active      BOOLEAN          NOT NULL DEFAULT TRUE,
    priority       SMALLINT         NOT NULL DEFAULT 0,
    created_at     TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    traffic_weight DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    endpoints      TEXT
);

CREATE TABLE IF NOT EXISTS pz_models (
    id                        UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
    brand_id                  UUID         NOT NULL REFERENCES pz_brands(id) ON DELETE RESTRICT,
    slug                      VARCHAR(150) NOT NULL,
    display_name              VARCHAR(150) NOT NULL,
    max_context_tokens        INT          NOT NULL,
    max_output_tokens         INT,
    supports_function_calling BOOLEAN      NOT NULL DEFAULT FALSE,
    supports_json_mode        BOOLEAN      NOT NULL DEFAULT FALSE,
    price_input_per_1m        DOUBLE PRECISION,
    price_output_per_1m       DOUBLE PRECISION,
    tpm_limit                 INT,
    rpm_limit                 INT,
    rpd_limit                 INT,
    tpd_limit                 BIGINT,
    tpm_limit_month           BIGINT,
    rps_limit                 DOUBLE PRECISION,
    quality_score             DOUBLE PRECISION,
    avg_latency_ms            INT,
    is_enabled                BOOLEAN      NOT NULL DEFAULT TRUE,
    notes                     TEXT,
    category                  VARCHAR(50),
    created_at                TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    batch_price_multiplier    DOUBLE PRECISION,
    diarization               BOOLEAN,
    streaming                 BOOLEAN      NOT NULL DEFAULT FALSE,
    http_batch                BOOLEAN      NOT NULL DEFAULT FALSE,
    word_timestamps           BOOLEAN,
    base_url                  VARCHAR(255),
    supported_languages       TEXT,
    reasoning_effort_value    TEXT,
    canonical_key             TEXT,
    price_synced_at           TIMESTAMPTZ,
    trains_on_data            BOOLEAN,
    retains_data              BOOLEAN
);

CREATE TABLE IF NOT EXISTS pz_model_catalog (
    id                        UUID             PRIMARY KEY DEFAULT gen_random_uuid(),
    canonical_key             VARCHAR(255)     UNIQUE NOT NULL,
    display_name              VARCHAR(150),
    category                  VARCHAR(50),
    max_context_tokens        INT,
    supports_function_calling BOOLEAN,
    supports_json_mode        BOOLEAN,
    quality_score             DOUBLE PRECISION,
    knowledge_cutoff          VARCHAR(50),
    created_at                TIMESTAMPTZ      NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS pz_selection_rules (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    step             VARCHAR(50) NOT NULL,
    model_id         UUID        NOT NULL REFERENCES pz_models(id) ON DELETE CASCADE,
    priority         SMALLINT    NOT NULL,
    max_ctx_tokens   INT,
    requires_fn_call BOOLEAN     NOT NULL DEFAULT FALSE,
    is_enabled       BOOLEAN     NOT NULL DEFAULT TRUE,
    UNIQUE (step, model_id)
);

CREATE TABLE IF NOT EXISTS pz_groups (
    id          UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
    slug        VARCHAR(100) UNIQUE NOT NULL,
    name        VARCHAR(150) NOT NULL,
    description TEXT,
    is_active   BOOLEAN      NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS pz_group_members (
    id         UUID     PRIMARY KEY DEFAULT gen_random_uuid(),
    group_id   UUID     NOT NULL REFERENCES pz_groups(id) ON DELETE CASCADE,
    model_id   UUID     NOT NULL REFERENCES pz_models(id) ON DELETE CASCADE,
    priority   SMALLINT NOT NULL DEFAULT 0,
    is_enabled BOOLEAN  NOT NULL DEFAULT TRUE,
    UNIQUE (group_id, model_id)
);

CREATE INDEX IF NOT EXISTS idx_pz_group_members_group
    ON pz_group_members(group_id);

CREATE TABLE IF NOT EXISTS pz_rate_events (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    model_id    UUID        NOT NULL REFERENCES pz_models(id) ON DELETE CASCADE,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    error_type  VARCHAR(50) NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pz_rate_events_model_time
    ON pz_rate_events(model_id, occurred_at DESC);

CREATE UNIQUE INDEX IF NOT EXISTS idx_pz_models_brand_slug
    ON pz_models(brand_id, slug, streaming, http_batch);

CREATE TABLE IF NOT EXISTS pz_brand_api_keys (
    id          UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
    brand_id    UUID         NOT NULL REFERENCES pz_brands(id) ON DELETE CASCADE,
    api_key_env VARCHAR(100) NOT NULL,
    priority    SMALLINT     NOT NULL DEFAULT 0,
    is_active   BOOLEAN      NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    UNIQUE (brand_id, api_key_env)
);

CREATE INDEX IF NOT EXISTS idx_pz_brand_api_keys_brand
    ON pz_brand_api_keys(brand_id);
";
