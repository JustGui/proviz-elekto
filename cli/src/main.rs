use std::sync::Arc;

use clap::{Parser, Subcommand};
use proviz_elekto_core::{
    models::{Brand, BrandApiKey, Group, GroupMember, Model, SelectRequest, SelectionRule},
    selector::Selector,
    storage::CatalogStorage,
};
use proviz_elekto_storage_pg::PostgresStorage;
use proviz_elekto_storage_sqlite::SqliteStorage;

use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "proviz",
    about = "ProvizElekto catalog management and dry-run selection"
)]
struct Cli {
    #[arg(long, env = "PROVIZ_STORAGE", default_value = "sqlite")]
    storage: String,

    #[arg(long, env = "PROVIZ_DATABASE_URL")]
    database_url: Option<String>,

    #[arg(long, env = "PROVIZ_DB_PATH", default_value = "./proviz.db")]
    db_path: String,

    /// Running server base URL for reload command (e.g. http://localhost:PORT printed on startup)
    #[arg(long, env = "PROVIZ_SERVER")]
    server: Option<String>,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    Brand {
        #[command(subcommand)]
        action: BrandCmd,
    },
    Model {
        #[command(subcommand)]
        action: ModelCmd,
    },
    Rule {
        #[command(subcommand)]
        action: RuleCmd,
    },
    /// Manage model groups
    Group {
        #[command(subcommand)]
        action: GroupCmd,
    },
    /// Dry-run selection (no state change)
    Select {
        #[arg(long)]
        step: String,
        #[arg(long)]
        tokens: u32,
        #[arg(long)]
        json_mode: bool,
        #[arg(long)]
        fn_call: bool,
        #[arg(long, default_value = "0.0")]
        quality_min: f32,
        /// Restrict selection to a group (by UUID)
        #[arg(long)]
        group_id: Option<Uuid>,
        /// Restrict selection to a group (by slug)
        #[arg(long)]
        group_name: Option<String>,
    },
    /// Hot-reload catalog in running server
    Reload,
    /// Seed catalog from providers directory (brands + models)
    Seed {
        /// Root directory containing provider subdirectories
        #[arg(long, default_value = "./providers")]
        dir: String,
        /// Update rate-limit fields of models that already exist in the DB
        #[arg(long)]
        update_limits: bool,
    },
    /// Load provider definitions from a directory of JSON files
    Providers {
        #[command(subcommand)]
        action: ProvidersCmd,
    },
}

#[derive(Subcommand)]
enum ProvidersCmd {
    /// Load all providers from a directory (each subdir = one provider)
    Load {
        /// Root directory containing provider subdirectories
        #[arg(long, default_value = "./providers")]
        dir: String,
        /// Update rate-limit fields of models that already exist in the DB
        #[arg(long)]
        update_limits: bool,
    },
    /// List provider directories found in a directory
    List {
        #[arg(long, default_value = "./providers")]
        dir: String,
    },
}

#[derive(Subcommand)]
enum BrandCmd {
    Add {
        #[arg(long)]
        slug: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        api_key_env: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        /// Selection priority — lower = tried first (default 0)
        #[arg(long, default_value = "0")]
        priority: i16,
        /// Traffic weight for load distribution (default 1.0, higher = more traffic)
        #[arg(long, default_value = "1.0")]
        traffic_weight: f64,
    },
    List,
    Disable {
        #[arg(long)]
        slug: String,
    },
    Enable {
        #[arg(long)]
        slug: String,
    },
    /// Set selection priority (lower = tried first, default 0)
    SetPriority {
        #[arg(long)]
        slug: String,
        #[arg(long)]
        priority: i16,
    },
    /// Set traffic weight for load distribution (1.0 = default, higher = more traffic)
    SetTrafficWeight {
        #[arg(long)]
        slug: String,
        #[arg(long)]
        weight: f64,
    },
    /// Manage API keys (multiple accounts) for a brand
    Key {
        #[command(subcommand)]
        action: BrandKeyCmd,
    },
}

#[derive(Subcommand)]
enum BrandKeyCmd {
    /// Add an API key (account) for a brand
    Add {
        /// Brand slug
        #[arg(long)]
        brand: String,
        /// Environment variable name holding the API key secret
        #[arg(long)]
        api_key_env: String,
        /// Selection priority — lower = tried first (default 0)
        #[arg(long, default_value = "0")]
        priority: i16,
    },
    /// List all API keys for a brand
    List {
        #[arg(long)]
        brand: String,
    },
    /// Remove an API key from a brand
    Remove {
        #[arg(long)]
        brand: String,
        /// Environment variable name of the key to remove
        #[arg(long)]
        api_key_env: String,
    },
}

#[derive(Subcommand)]
enum ModelCmd {
    Add {
        #[arg(long)]
        brand: String,
        #[arg(long)]
        slug: String,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        max_ctx: u32,
        #[arg(long)]
        max_output: Option<u32>,
        #[arg(long)]
        price_in: Option<f64>,
        #[arg(long)]
        price_out: Option<f64>,
        #[arg(long)]
        tpm: Option<u32>,
        #[arg(long)]
        rpm: Option<u32>,
        #[arg(long)]
        json_mode: bool,
        #[arg(long)]
        function_calling: bool,
        #[arg(long)]
        quality: Option<f64>,
        #[arg(long)]
        latency_ms: Option<u32>,
        #[arg(long)]
        notes: Option<String>,
        /// Category tag: text, code, embedding, vision, audio, moderation
        #[arg(long)]
        category: Option<String>,
    },
    List,
    Disable {
        #[arg(long)]
        slug: String,
    },
    Enable {
        #[arg(long)]
        slug: String,
    },
    /// Bulk import from JSON file
    Import {
        #[arg(long)]
        file: String,
    },
}

#[derive(Subcommand)]
enum RuleCmd {
    Add {
        #[arg(long)]
        step: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        priority: i16,
        #[arg(long)]
        max_ctx: Option<u32>,
        #[arg(long)]
        fn_call: bool,
    },
    List {
        #[arg(long)]
        step: Option<String>,
    },
    Remove {
        #[arg(long)]
        id: Uuid,
    },
}

#[derive(Subcommand)]
enum GroupCmd {
    /// Create a new group
    Add {
        #[arg(long)]
        slug: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        description: Option<String>,
    },
    /// List all groups
    List,
    /// Enable a group (by slug)
    Enable {
        #[arg(long)]
        slug: String,
    },
    /// Disable a group (by slug)
    Disable {
        #[arg(long)]
        slug: String,
    },
    /// Delete a group (by slug; also removes all its members)
    Delete {
        #[arg(long)]
        slug: String,
    },
    /// Manage models within a group
    Member {
        #[command(subcommand)]
        action: GroupMemberCmd,
    },
}

#[derive(Subcommand)]
enum GroupMemberCmd {
    /// Add a model to a group
    Add {
        /// Group slug or UUID
        #[arg(long)]
        group: String,
        /// Model slug
        #[arg(long)]
        model: String,
        /// Lower = tried first within group (default 0)
        #[arg(long, default_value = "0")]
        priority: i16,
    },
    /// Remove a model from a group
    Remove {
        #[arg(long)]
        group: String,
        #[arg(long)]
        model: String,
    },
    /// List members of a group
    List {
        #[arg(long)]
        group: String,
    },
}

fn open_storage(cli: &Cli) -> Arc<dyn CatalogStorage> {
    match cli.storage.as_str() {
        "postgres" | "postgresql" => {
            let url = cli
                .database_url
                .clone()
                .expect("--database-url required for postgres");
            Arc::new(PostgresStorage::connect(&url).expect("PostgreSQL connect failed"))
        }
        _ => Arc::new(SqliteStorage::open(&cli.db_path).expect("SQLite open failed")),
    }
}

fn find_brand(storage: &Arc<dyn CatalogStorage>, slug: &str) -> Brand {
    storage
        .load_brands()
        .unwrap()
        .into_iter()
        .find(|b| b.slug == slug)
        .unwrap_or_else(|| panic!("brand '{slug}' not found - add it first with: proviz brand add"))
}

fn find_model(storage: &Arc<dyn CatalogStorage>, slug: &str) -> Model {
    storage
        .load_models()
        .unwrap()
        .into_iter()
        .find(|m| m.slug == slug)
        .unwrap_or_else(|| panic!("model '{slug}' not found - add it first with: proviz model add"))
}

fn find_group(storage: &Arc<dyn CatalogStorage>, slug_or_id: &str) -> Group {
    let groups = storage.load_groups().unwrap();
    if let Ok(id) = slug_or_id.parse::<Uuid>() {
        groups
            .into_iter()
            .find(|g| g.id == id)
            .unwrap_or_else(|| panic!("group '{slug_or_id}' not found"))
    } else {
        groups
            .into_iter()
            .find(|g| g.slug == slug_or_id)
            .unwrap_or_else(|| {
                panic!("group '{slug_or_id}' not found - add it with: proviz group add")
            })
    }
}

fn main() {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let storage = open_storage(&cli);

    match cli.cmd {
        Command::Brand { action } => match action {
            BrandCmd::Add {
                slug,
                name,
                api_key_env,
                base_url,
                priority,
                traffic_weight,
            } => {
                let brand = Brand {
                    id: Uuid::new_v4(),
                    slug: slug.clone(),
                    name: name.clone(),
                    base_url,
                    is_active: true,
                    priority,
                    created_at: chrono::Utc::now(),
                    traffic_weight,
                    endpoints: None,
                };
                storage.insert_brand(&brand).unwrap();
                if let Some(env) = api_key_env {
                    storage
                        .insert_brand_api_key(&BrandApiKey {
                            id: Uuid::new_v4(),
                            brand_id: brand.id,
                            api_key_env: env.clone(),
                            priority: 0,
                            is_active: true,
                            created_at: chrono::Utc::now(),
                        })
                        .unwrap();
                    println!("brand '{slug}' added (id={}) with key '{env}'", brand.id);
                } else {
                    println!("brand '{slug}' added (id={})", brand.id);
                }
            }
            BrandCmd::List => {
                let mut brands = storage.load_brands().unwrap();
                brands.sort_by_key(|b| b.priority);
                println!(
                    "{:<36}  {:<15}  {:<20}  {:>4}  {:>7}  active",
                    "id", "slug", "name", "prio", "weight"
                );
                println!("{}", "-".repeat(98));
                for b in brands {
                    println!(
                        "{:<36}  {:<15}  {:<20}  {:>4}  {:>7.2}  {}",
                        b.id, b.slug, b.name, b.priority, b.traffic_weight, b.is_active
                    );
                }
            }
            BrandCmd::Disable { slug } => {
                let b = find_brand(&storage, &slug);
                storage.set_brand_active(b.id, false).unwrap();
                println!("brand '{slug}' disabled");
            }
            BrandCmd::SetPriority { slug, priority } => {
                let mut b = find_brand(&storage, &slug);
                b.priority = priority;
                storage.insert_brand(&b).unwrap();
                println!("brand '{slug}' priority set to {priority}");
            }
            BrandCmd::Enable { slug } => {
                let b = find_brand(&storage, &slug);
                storage.set_brand_active(b.id, true).unwrap();
                println!("brand '{slug}' enabled");
            }
            BrandCmd::SetTrafficWeight { slug, weight } => {
                let mut b = find_brand(&storage, &slug);
                b.traffic_weight = weight;
                storage.insert_brand(&b).unwrap();
                println!("brand '{slug}' traffic_weight set to {weight:.2}");
            }
            BrandCmd::Key { action } => match action {
                BrandKeyCmd::Add {
                    brand,
                    api_key_env,
                    priority,
                } => {
                    let b = find_brand(&storage, &brand);
                    let key = BrandApiKey {
                        id: Uuid::new_v4(),
                        brand_id: b.id,
                        api_key_env: api_key_env.clone(),
                        priority,
                        is_active: true,
                        created_at: chrono::Utc::now(),
                    };
                    storage.insert_brand_api_key(&key).unwrap();
                    println!(
                        "key '{api_key_env}' added for brand '{brand}' (id={}, priority={priority})",
                        key.id
                    );
                }
                BrandKeyCmd::List { brand } => {
                    let b = find_brand(&storage, &brand);
                    let mut keys = storage.load_all_brand_api_keys().unwrap();
                    keys.retain(|k| k.brand_id == b.id);
                    keys.sort_by_key(|k| k.priority);
                    println!("{:<36}  {:<30}  {:>4}  active", "id", "api_key_env", "prio");
                    println!("{}", "-".repeat(80));
                    for k in keys {
                        println!(
                            "{:<36}  {:<30}  {:>4}  {}",
                            k.id, k.api_key_env, k.priority, k.is_active
                        );
                    }
                }
                BrandKeyCmd::Remove { brand, api_key_env } => {
                    let b = find_brand(&storage, &brand);
                    let keys = storage.load_all_brand_api_keys().unwrap();
                    let key = keys
                        .into_iter()
                        .find(|k| k.brand_id == b.id && k.api_key_env == api_key_env)
                        .unwrap_or_else(|| {
                            eprintln!("no key '{api_key_env}' found for brand '{brand}'");
                            std::process::exit(1);
                        });
                    storage.delete_brand_api_key(key.id).unwrap();
                    println!("key '{api_key_env}' removed from brand '{brand}'");
                }
            },
        },

        Command::Model { action } => match action {
            ModelCmd::Add {
                brand,
                slug,
                display_name,
                max_ctx,
                max_output,
                price_in,
                price_out,
                tpm,
                rpm,
                json_mode,
                function_calling,
                quality,
                latency_ms,
                notes,
                category,
            } => {
                let brand_rec = find_brand(&storage, &brand);
                let display = display_name.unwrap_or_else(|| slug.clone());
                let model = Model {
                    id: Uuid::new_v4(),
                    brand_id: brand_rec.id,
                    slug: slug.clone(),
                    display_name: display,
                    max_context_tokens: max_ctx,
                    max_output_tokens: max_output,
                    supports_function_calling: function_calling,
                    supports_json_mode: json_mode,
                    price_input_per_1m: price_in,
                    price_output_per_1m: price_out,
                    tpm_limit: tpm,
                    rpm_limit: rpm,
                    rpd_limit: None,
                    tpd_limit: None,
                    tpm_limit_month: None,
                    rps_limit: None,
                    quality_score: quality,
                    avg_latency_ms: latency_ms,
                    is_enabled: true,
                    notes,
                    category,
                    created_at: chrono::Utc::now(),
                    batch_price_multiplier: None,
                    diarization: None,
                    streaming: None,
                    http_batch: None,
                    word_timestamps: None,
                    base_url: None,
                };
                storage.insert_model(&model).unwrap();
                println!("model '{slug}' added (id={})", model.id);
            }
            ModelCmd::List => {
                let models = storage.load_models().unwrap();
                let brands: std::collections::HashMap<Uuid, String> = storage
                    .load_brands()
                    .unwrap()
                    .into_iter()
                    .map(|b| (b.id, b.slug))
                    .collect();
                println!(
                    "{:<36}  {:<15}  {:<35}  {:>8}  {:>7}  {:>7}  en",
                    "id", "brand", "slug", "ctx_k", "q", "lat_ms"
                );
                println!("{}", "-".repeat(115));
                for m in models {
                    let brand_slug = brands.get(&m.brand_id).map(|s| s.as_str()).unwrap_or("?");
                    println!(
                        "{:<36}  {:<15}  {:<35}  {:>8}  {:>7}  {:>7}  {}",
                        m.id,
                        brand_slug,
                        m.slug,
                        m.max_context_tokens / 1000,
                        m.quality_score
                            .map(|q| format!("{:.2}", q))
                            .unwrap_or("-".into()),
                        m.avg_latency_ms
                            .map(|v| v.to_string())
                            .unwrap_or("-".into()),
                        if m.is_enabled { "✓" } else { "✗" },
                    );
                }
            }
            ModelCmd::Disable { slug } => {
                let m = find_model(&storage, &slug);
                storage.set_model_enabled(m.id, false).unwrap();
                println!("model '{slug}' disabled");
            }
            ModelCmd::Enable { slug } => {
                let m = find_model(&storage, &slug);
                storage.set_model_enabled(m.id, true).unwrap();
                println!("model '{slug}' enabled");
            }
            ModelCmd::Import { file } => {
                let content =
                    std::fs::read_to_string(&file).unwrap_or_else(|_| panic!("cannot read {file}"));
                let models: Vec<serde_json::Value> = serde_json::from_str(&content).unwrap();
                let brands: std::collections::HashMap<String, Brand> = storage
                    .load_brands()
                    .unwrap()
                    .into_iter()
                    .map(|b| (b.slug.clone(), b))
                    .collect();
                let mut count = 0;
                for v in models {
                    let brand_slug = v["brand"].as_str().unwrap();
                    let brand = brands
                        .get(brand_slug)
                        .unwrap_or_else(|| panic!("brand '{brand_slug}' not found"));
                    let slug = v["slug"].as_str().unwrap().to_string();
                    let model = Model {
                        id: Uuid::new_v4(),
                        brand_id: brand.id,
                        slug: slug.clone(),
                        display_name: v["display_name"].as_str().unwrap_or(&slug).to_string(),
                        max_context_tokens: v["max_context_tokens"].as_u64().unwrap() as u32,
                        max_output_tokens: v["max_output_tokens"].as_u64().map(|v| v as u32),
                        supports_function_calling: v["supports_function_calling"]
                            .as_bool()
                            .unwrap_or(false),
                        supports_json_mode: v["supports_json_mode"].as_bool().unwrap_or(false),
                        price_input_per_1m: v["price_input_per_1m"].as_f64(),
                        price_output_per_1m: v["price_output_per_1m"].as_f64(),
                        tpm_limit: v["tpm_limit"].as_u64().map(|v| v as u32),
                        rpm_limit: v["rpm_limit"].as_u64().map(|v| v as u32),
                        rpd_limit: v["rpd_limit"].as_u64().map(|v| v as u32),
                        tpd_limit: v["tpd_limit"].as_u64(),
                        tpm_limit_month: v["tpm_limit_month"].as_u64(),
                        rps_limit: v["rps_limit"].as_f64(),
                        quality_score: v["quality_score"].as_f64(),
                        avg_latency_ms: v["avg_latency_ms"].as_u64().map(|v| v as u32),
                        is_enabled: v["is_enabled"].as_bool().unwrap_or(true),
                        notes: v["notes"].as_str().map(|s| s.to_string()),
                        category: v["category"].as_str().map(|s| s.to_string()),
                        created_at: chrono::Utc::now(),
                        batch_price_multiplier: v["batch_price_multiplier"].as_f64(),
                        diarization: v["diarization"].as_bool(),
                        streaming: v["streaming"].as_bool(),
                        http_batch: v["http_batch"].as_bool(),
                        word_timestamps: v["word_timestamps"].as_bool(),
                        base_url: v["base_url"].as_str().map(|s| s.to_string()),
                    };
                    storage.insert_model(&model).unwrap();
                    count += 1;
                }
                println!("{count} models imported from {file}");
            }
        },

        Command::Rule { action } => match action {
            RuleCmd::Add {
                step,
                model,
                priority,
                max_ctx,
                fn_call,
            } => {
                let m = find_model(&storage, &model);
                let rule = SelectionRule {
                    id: Uuid::new_v4(),
                    step: step.clone(),
                    model_id: m.id,
                    priority,
                    max_ctx_tokens: max_ctx,
                    requires_fn_call: fn_call,
                    is_enabled: true,
                };
                storage.insert_rule(&rule).unwrap();
                println!("rule added for step='{step}' model='{model}' priority={priority}");
            }
            RuleCmd::List { step } => {
                let step_filter = step.as_deref().unwrap_or("*");
                let rules = storage.load_selection_rules(step_filter).unwrap();
                let models: std::collections::HashMap<Uuid, String> = storage
                    .load_models()
                    .unwrap()
                    .into_iter()
                    .map(|m| (m.id, m.slug))
                    .collect();
                println!(
                    "{:<36}  {:<12}  {:<35}  {:>4}  {:>8}  fn_call  en",
                    "id", "step", "model", "prio", "max_ctx"
                );
                println!("{}", "-".repeat(110));
                for r in rules {
                    let model_slug = models.get(&r.model_id).map(|s| s.as_str()).unwrap_or("?");
                    println!(
                        "{:<36}  {:<12}  {:<35}  {:>4}  {:>8}  {:<7}  {}",
                        r.id,
                        r.step,
                        model_slug,
                        r.priority,
                        r.max_ctx_tokens
                            .map(|v| v.to_string())
                            .unwrap_or("-".into()),
                        if r.requires_fn_call { "✓" } else { "✗" },
                        if r.is_enabled { "✓" } else { "✗" },
                    );
                }
            }
            RuleCmd::Remove { id } => {
                storage.delete_rule(id).unwrap();
                println!("rule {id} removed");
            }
        },

        Command::Group { action } => match action {
            GroupCmd::Add {
                slug,
                name,
                description,
            } => {
                let group = Group {
                    id: Uuid::new_v4(),
                    slug: slug.clone(),
                    name: name.clone(),
                    description,
                    is_active: true,
                    created_at: chrono::Utc::now(),
                };
                storage.insert_group(&group).unwrap();
                println!("group '{slug}' added (id={})", group.id);
            }
            GroupCmd::List => {
                let groups = storage.load_groups().unwrap();
                println!("{:<36}  {:<20}  {:<30}  active", "id", "slug", "name");
                println!("{}", "-".repeat(95));
                for g in groups {
                    println!(
                        "{:<36}  {:<20}  {:<30}  {}",
                        g.id, g.slug, g.name, g.is_active
                    );
                }
            }
            GroupCmd::Enable { slug } => {
                let g = find_group(&storage, &slug);
                storage.set_group_active(g.id, true).unwrap();
                println!("group '{slug}' enabled");
            }
            GroupCmd::Disable { slug } => {
                let g = find_group(&storage, &slug);
                storage.set_group_active(g.id, false).unwrap();
                println!("group '{slug}' disabled");
            }
            GroupCmd::Delete { slug } => {
                let g = find_group(&storage, &slug);
                storage.delete_group(g.id).unwrap();
                println!("group '{slug}' deleted");
            }
            GroupCmd::Member { action } => match action {
                GroupMemberCmd::Add {
                    group,
                    model,
                    priority,
                } => {
                    let g = find_group(&storage, &group);
                    let m = find_model(&storage, &model);
                    let member = GroupMember {
                        id: Uuid::new_v4(),
                        group_id: g.id,
                        model_id: m.id,
                        priority,
                        is_enabled: true,
                    };
                    storage.insert_group_member(&member).unwrap();
                    println!("model '{model}' added to group '{group}' (priority={priority})");
                }
                GroupMemberCmd::Remove { group, model } => {
                    let g = find_group(&storage, &group);
                    let m = find_model(&storage, &model);
                    storage.remove_group_member(g.id, m.id).unwrap();
                    println!("model '{model}' removed from group '{group}'");
                }
                GroupMemberCmd::List { group } => {
                    let g = find_group(&storage, &group);
                    let all_members = storage.load_all_group_members().unwrap();
                    let members: Vec<_> = all_members
                        .into_iter()
                        .filter(|m| m.group_id == g.id)
                        .collect();
                    let models: std::collections::HashMap<Uuid, String> = storage
                        .load_models()
                        .unwrap()
                        .into_iter()
                        .map(|m| (m.id, m.slug))
                        .collect();
                    println!(
                        "{:<36}  {:<35}  {:>4}  en",
                        "model_id", "model_slug", "prio"
                    );
                    println!("{}", "-".repeat(85));
                    for member in members {
                        let slug = models
                            .get(&member.model_id)
                            .map(|s| s.as_str())
                            .unwrap_or("?");
                        println!(
                            "{:<36}  {:<35}  {:>4}  {}",
                            member.model_id,
                            slug,
                            member.priority,
                            if member.is_enabled { "✓" } else { "✗" }
                        );
                    }
                }
            },
        },

        Command::Select {
            step,
            tokens,
            json_mode,
            fn_call,
            quality_min,
            group_id,
            group_name,
        } => {
            let selector = Selector::new(storage);
            selector.reload().unwrap();
            let req = SelectRequest {
                step: step.clone(),
                estimated_tokens: tokens,
                requires_fn_call: fn_call,
                requires_json_mode: json_mode,
                quality_min,
                exclude_ids: vec![],
                categories: vec![],
                group_id,
                group_name,
                use_member_priority: true,
                max_wait_ms: None,
            };
            match selector.select(&req) {
                Ok(c) => {
                    println!("selected:");
                    println!("  brand:      {}", c.brand_slug);
                    println!("  model:      {}", c.model_slug);
                    println!("  model_id:   {}", c.model_id);
                    println!(
                        "  api_key_env:{}",
                        c.api_key_env.as_deref().unwrap_or("(none)")
                    );
                    println!("  max_ctx:    {}", c.max_context_tokens);
                    println!("  fn_call:    {}", c.supports_function_calling);
                    println!("  json_mode:  {}", c.supports_json_mode);
                    if let Some(cost) = c.estimated_input_cost_usd {
                        println!("  est_cost:   ${:.6}", cost);
                    }
                }
                Err(e) => {
                    eprintln!("no model selected: {e}");
                    std::process::exit(1);
                }
            }
        }

        Command::Reload => {
            let base = cli.server.as_deref().unwrap_or_else(|| {
                eprintln!(
                    "error: --server required (use the port printed by proviz-server on startup)"
                );
                eprintln!("       e.g. proviz reload --server http://localhost:43912");
                std::process::exit(1);
            });
            let url = format!("{base}/catalog/reload");
            let resp = reqwest::blocking::Client::new()
                .post(&url)
                .send()
                .unwrap_or_else(|e| panic!("request failed: {e}"));
            println!("{}", resp.text().unwrap());
        }

        Command::Seed { dir, update_limits } => {
            load_providers(&storage, &dir, update_limits);
        }

        Command::Providers { action } => match action {
            ProvidersCmd::Load { dir, update_limits } => {
                load_providers(&storage, &dir, update_limits);
            }
            ProvidersCmd::List { dir } => {
                list_providers(&dir);
            }
        },
    }
}

fn list_providers(dir: &str) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot read {dir}: {e}");
            return;
        }
    };
    println!("{:<20}  brand.json  models.json", "provider");
    println!("{}", "-".repeat(50));
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let has_brand = path.join("brand.json").exists();
        let has_models = path.join("models.json").exists();
        println!(
            "{:<20}  {:<10}  {}",
            name,
            if has_brand { "✓" } else { "✗" },
            if has_models { "✓" } else { "✗" }
        );
    }
}

fn load_providers(storage: &Arc<dyn CatalogStorage>, dir: &str, update_limits: bool) {
    match proviz_elekto_core::builtin_providers::load_from_dir(storage.as_ref(), dir, update_limits)
    {
        Ok(s) => println!(
            "providers loaded: {} brands added, {} models added, {} updated, {} skipped",
            s.brands_added, s.models_added, s.models_updated, s.models_skipped
        ),
        Err(e) => eprintln!("error loading providers: {e}"),
    }
}
