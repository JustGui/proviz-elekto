use chrono::{DateTime, Utc};
use proviz_elekto_core::models::{Brand, Group, GroupMember, Model, SelectionRule};
use uuid::Uuid;

// Base SELECT queries — append WHERE / ORDER BY in each adapter.
pub const Q_BRANDS: &str =
    "SELECT id,slug,name,api_key_env,base_url,is_active,priority,created_at,traffic_weight \
     FROM pz_brands";

pub const Q_MODELS: &str =
    "SELECT id,brand_id,slug,display_name,max_context_tokens,max_output_tokens,\
     supports_function_calling,supports_json_mode,price_input_per_1m,price_output_per_1m,\
     tpm_limit,rpm_limit,rpd_limit,tpd_limit,tpm_limit_month,rps_limit,quality_score,\
     avg_latency_ms,is_enabled,notes,category,created_at \
     FROM pz_models";

pub const Q_RULES: &str =
    "SELECT id,step,model_id,priority,max_ctx_tokens,requires_fn_call,is_enabled \
     FROM pz_selection_rules";

pub const Q_GROUPS: &str = "SELECT id,slug,name,description,is_active,created_at \
     FROM pz_groups";

pub const Q_GROUP_MEMBERS: &str = "SELECT id,group_id,model_id,priority,is_enabled \
     FROM pz_group_members";

/// Uniform read interface over a single result row.
/// Implementations must match column indices to the constants above.
/// Methods panic on schema mismatch — these are programming errors, not runtime errors.
pub trait RowReader {
    fn uuid(&self, idx: usize) -> Uuid;
    fn string(&self, idx: usize) -> String;
    fn opt_string(&self, idx: usize) -> Option<String>;
    fn bool_val(&self, idx: usize) -> bool;
    fn i16_val(&self, idx: usize) -> i16;
    fn i32_val(&self, idx: usize) -> i32;
    fn opt_i32(&self, idx: usize) -> Option<i32>;
    fn opt_i64(&self, idx: usize) -> Option<i64>;
    fn opt_f64(&self, idx: usize) -> Option<f64>;
    fn datetime(&self, idx: usize) -> DateTime<Utc>;
}

pub fn brand_from_row(row: &impl RowReader) -> Brand {
    Brand {
        id: row.uuid(0),
        slug: row.string(1),
        name: row.string(2),
        api_key_env: row.opt_string(3),
        base_url: row.opt_string(4),
        is_active: row.bool_val(5),
        priority: row.i16_val(6),
        created_at: row.datetime(7),
        traffic_weight: row.opt_f64(8).unwrap_or(1.0),
    }
}

pub fn model_from_row(row: &impl RowReader) -> Model {
    Model {
        id: row.uuid(0),
        brand_id: row.uuid(1),
        slug: row.string(2),
        display_name: row.string(3),
        max_context_tokens: row.i32_val(4) as u32,
        max_output_tokens: row.opt_i32(5).map(|v| v as u32),
        supports_function_calling: row.bool_val(6),
        supports_json_mode: row.bool_val(7),
        price_input_per_1m: row.opt_f64(8),
        price_output_per_1m: row.opt_f64(9),
        tpm_limit: row.opt_i32(10).map(|v| v as u32),
        rpm_limit: row.opt_i32(11).map(|v| v as u32),
        rpd_limit: row.opt_i32(12).map(|v| v as u32),
        tpd_limit: row.opt_i64(13).map(|v| v as u64),
        tpm_limit_month: row.opt_i64(14).map(|v| v as u64),
        rps_limit: row.opt_f64(15),
        quality_score: row.opt_f64(16),
        avg_latency_ms: row.opt_i32(17).map(|v| v as u32),
        is_enabled: row.bool_val(18),
        notes: row.opt_string(19),
        category: row.opt_string(20),
        created_at: row.datetime(21),
    }
}

pub fn rule_from_row(row: &impl RowReader) -> SelectionRule {
    SelectionRule {
        id: row.uuid(0),
        step: row.string(1),
        model_id: row.uuid(2),
        priority: row.i16_val(3),
        max_ctx_tokens: row.opt_i32(4).map(|v| v as u32),
        requires_fn_call: row.bool_val(5),
        is_enabled: row.bool_val(6),
    }
}

pub fn group_from_row(row: &impl RowReader) -> Group {
    Group {
        id: row.uuid(0),
        slug: row.string(1),
        name: row.string(2),
        description: row.opt_string(3),
        is_active: row.bool_val(4),
        created_at: row.datetime(5),
    }
}

pub fn group_member_from_row(row: &impl RowReader) -> GroupMember {
    GroupMember {
        id: row.uuid(0),
        group_id: row.uuid(1),
        model_id: row.uuid(2),
        priority: row.i16_val(3),
        is_enabled: row.bool_val(4),
    }
}
