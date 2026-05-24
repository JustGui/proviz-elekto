use std::sync::Arc;

use clap::{Parser, Subcommand};
use proviz_elekto_core::{
    models::{Brand, Model, SelectRequest, SelectionRule},
    selector::Selector,
    storage::CatalogStorage,
};
use proviz_elekto_storage_pg::PostgresStorage;
use proviz_elekto_storage_sqlite::SqliteStorage;
use serde::Deserialize;
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
    },
    /// Hot-reload catalog in running server
    Reload,
    /// Seed built-in catalog (brands + common models)
    Seed {
        #[arg(long)]
        brands: bool,
        #[arg(long)]
        models: bool,
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
        /// Override plan for all providers (e.g. "developer"). Defaults to each brand's configured plan.
        #[arg(long)]
        plan: Option<String>,
    },
    /// List provider directories found in a directory
    List {
        #[arg(long, default_value = "./providers")]
        dir: String,
    },
}

#[derive(Debug, Deserialize)]
struct ProviderBrandDef {
    slug: String,
    name: String,
    api_key_env: Option<String>,
    base_url: Option<String>,
    /// Default plan recorded in the brand (e.g. "free", "developer")
    plan: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderModelDef {
    slug: String,
    display_name: Option<String>,
    #[serde(default)]
    max_context_tokens: u32,
    max_output_tokens: Option<u32>,
    #[serde(default)]
    supports_function_calling: bool,
    #[serde(default)]
    supports_json_mode: bool,
    price_input_per_1m: Option<f64>,
    price_output_per_1m: Option<f64>,
    tpm_limit: Option<u32>,
    rpm_limit: Option<u32>,
    rpd_limit: Option<u32>,
    tpd_limit: Option<u64>,
    tpm_limit_month: Option<u64>,
    rps_limit: Option<f64>,
    quality_score: Option<f32>,
    avg_latency_ms: Option<u32>,
    notes: Option<String>,
    category: Option<String>,
    /// Plan tier these limits apply to (e.g. "free", "developer")
    plan: Option<String>,
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
        /// Your plan with this provider (e.g. "free", "developer")
        #[arg(long)]
        plan: Option<String>,
        /// Selection priority — lower = tried first (default 0)
        #[arg(long, default_value = "0")]
        priority: i16,
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
    /// Set or update the plan for an existing brand
    SetPlan {
        #[arg(long)]
        slug: String,
        /// Plan name, e.g. "free", "developer", "enterprise"
        #[arg(long)]
        plan: String,
    },
    /// Set selection priority (lower = tried first, default 0)
    SetPriority {
        #[arg(long)]
        slug: String,
        #[arg(long)]
        priority: i16,
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
        quality: Option<f32>,
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
                plan,
                priority,
            } => {
                let brand = Brand {
                    id: Uuid::new_v4(),
                    slug: slug.clone(),
                    name: name.clone(),
                    api_key_env,
                    base_url,
                    is_active: true,
                    plan,
                    priority,
                    created_at: chrono::Utc::now(),
                };
                storage.insert_brand(&brand).unwrap();
                println!("brand '{slug}' added (id={})", brand.id);
            }
            BrandCmd::List => {
                let mut brands = storage.load_brands().unwrap();
                brands.sort_by_key(|b| b.priority);
                println!(
                    "{:<36}  {:<15}  {:<20}  {:<12}  {:>4}  active",
                    "id", "slug", "name", "plan", "prio"
                );
                println!("{}", "-".repeat(102));
                for b in brands {
                    println!(
                        "{:<36}  {:<15}  {:<20}  {:<12}  {:>4}  {}",
                        b.id,
                        b.slug,
                        b.name,
                        b.plan.as_deref().unwrap_or("-"),
                        b.priority,
                        b.is_active
                    );
                }
            }
            BrandCmd::Disable { slug } => {
                let b = find_brand(&storage, &slug);
                storage.set_brand_active(b.id, false).unwrap();
                println!("brand '{slug}' disabled");
            }
            BrandCmd::SetPlan { slug, plan } => {
                let mut b = find_brand(&storage, &slug);
                b.plan = Some(plan.clone());
                storage.insert_brand(&b).unwrap();
                println!("brand '{slug}' plan set to '{plan}'");
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
                    plan: None,
                    created_at: chrono::Utc::now(),
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
                        rps_limit: v["rps_limit"].as_f64().map(|v| v as f32),
                        quality_score: v["quality_score"].as_f64().map(|v| v as f32),
                        avg_latency_ms: v["avg_latency_ms"].as_u64().map(|v| v as u32),
                        is_enabled: v["is_enabled"].as_bool().unwrap_or(true),
                        notes: v["notes"].as_str().map(|s| s.to_string()),
                        category: v["category"].as_str().map(|s| s.to_string()),
                        plan: v["plan"].as_str().map(|s| s.to_string()),
                        created_at: chrono::Utc::now(),
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

        Command::Select {
            step,
            tokens,
            json_mode,
            fn_call,
            quality_min,
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

        Command::Seed { brands, models } => {
            if brands {
                seed_brands(&storage);
            }
            if models {
                seed_models(&storage);
            }
            if !brands && !models {
                seed_brands(&storage);
                seed_models(&storage);
            }
        }

        Command::Providers { action } => match action {
            ProvidersCmd::Load {
                dir,
                update_limits,
                plan,
            } => {
                load_providers(&storage, &dir, update_limits, plan.as_deref());
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

fn load_providers(
    storage: &Arc<dyn CatalogStorage>,
    dir: &str,
    update_limits: bool,
    plan_override: Option<&str>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot read providers dir '{dir}': {e}");
            return;
        }
    };

    let existing_brands: std::collections::HashMap<String, Brand> = storage
        .load_brands()
        .unwrap()
        .into_iter()
        .map(|b| (b.slug.clone(), b))
        .collect();
    let existing_models: std::collections::HashMap<String, Model> = storage
        .load_models()
        .unwrap()
        .into_iter()
        .map(|m| (m.slug.clone(), m))
        .collect();

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let provider_name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();

        let brand_path = path.join("brand.json");
        let models_path = path.join("models.json");

        if !brand_path.exists() {
            eprintln!("[{provider_name}] missing brand.json, skipping");
            continue;
        }
        if !models_path.exists() {
            eprintln!("[{provider_name}] missing models.json, skipping");
            continue;
        }

        let brand_def: ProviderBrandDef = match std::fs::read_to_string(&brand_path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[{provider_name}] invalid brand.json: {e}");
                continue;
            }
        };

        let model_defs: Vec<ProviderModelDef> = match std::fs::read_to_string(&models_path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[{provider_name}] invalid models.json: {e}");
                continue;
            }
        };

        // Determine which plan to load for this provider:
        // 1. CLI --plan flag overrides everything
        // 2. Existing brand's configured plan
        // 3. brand.json default plan
        // 4. None = load all rows (models with no plan field pass through)
        let effective_plan: Option<String> = plan_override
            .map(|s| s.to_string())
            .or_else(|| {
                existing_brands
                    .get(&brand_def.slug)
                    .and_then(|b| b.plan.clone())
            })
            .or_else(|| brand_def.plan.clone());

        if let Some(ref p) = effective_plan {
            println!("[{provider_name}] using plan '{p}'");
        }

        // Upsert brand: reuse existing UUID if slug already exists
        let brand_id = if let Some(existing) = existing_brands.get(&brand_def.slug) {
            println!(
                "[{}] brand '{}' already exists, skipping",
                provider_name, brand_def.slug
            );
            existing.id
        } else {
            let brand = Brand {
                id: Uuid::new_v4(),
                slug: brand_def.slug.clone(),
                name: brand_def.name.clone(),
                api_key_env: brand_def.api_key_env.clone(),
                base_url: brand_def.base_url.clone(),
                is_active: true,
                plan: effective_plan.clone(),
                priority: 0,
                created_at: chrono::Utc::now(),
            };
            storage.insert_brand(&brand).unwrap();
            println!("[{}] created brand '{}'", provider_name, brand_def.slug);
            brand.id
        };

        let mut inserted = 0usize;
        let mut updated = 0usize;
        let mut skipped = 0usize;
        let mut filtered = 0usize;

        for def in &model_defs {
            // Plan filter: if an effective plan is set, skip rows for other plans.
            // Rows without a plan field are always included.
            if let Some(ref ep) = effective_plan {
                if let Some(ref mp) = def.plan {
                    if mp != ep {
                        filtered += 1;
                        continue;
                    }
                }
            }

            if let Some(existing) = existing_models.get(&def.slug) {
                if update_limits {
                    let model = Model {
                        tpm_limit: def.tpm_limit.or(existing.tpm_limit),
                        rpm_limit: def.rpm_limit.or(existing.rpm_limit),
                        rpd_limit: def.rpd_limit.or(existing.rpd_limit),
                        tpd_limit: def.tpd_limit.or(existing.tpd_limit),
                        tpm_limit_month: def.tpm_limit_month.or(existing.tpm_limit_month),
                        rps_limit: def.rps_limit.map(|v| v as f32).or(existing.rps_limit),
                        plan: def.plan.clone().or_else(|| existing.plan.clone()),
                        ..existing.clone()
                    };
                    storage.insert_model(&model).unwrap();
                    updated += 1;
                } else {
                    skipped += 1;
                }
            } else {
                let display = def.display_name.clone().unwrap_or_else(|| def.slug.clone());
                let model = Model {
                    id: Uuid::new_v4(),
                    brand_id,
                    slug: def.slug.clone(),
                    display_name: display,
                    max_context_tokens: def.max_context_tokens,
                    max_output_tokens: def.max_output_tokens,
                    supports_function_calling: def.supports_function_calling,
                    supports_json_mode: def.supports_json_mode,
                    price_input_per_1m: def.price_input_per_1m,
                    price_output_per_1m: def.price_output_per_1m,
                    tpm_limit: def.tpm_limit,
                    rpm_limit: def.rpm_limit,
                    rpd_limit: def.rpd_limit,
                    tpd_limit: def.tpd_limit,
                    tpm_limit_month: def.tpm_limit_month,
                    rps_limit: def.rps_limit.map(|v| v as f32),
                    quality_score: def.quality_score,
                    avg_latency_ms: def.avg_latency_ms,
                    is_enabled: true,
                    notes: def.notes.clone(),
                    category: def.category.clone(),
                    plan: def.plan.clone(),
                    created_at: chrono::Utc::now(),
                };
                storage.insert_model(&model).unwrap();
                inserted += 1;
            }
        }

        println!(
            "[{}] models: {} inserted, {} updated, {} skipped, {} filtered (wrong plan)",
            provider_name, inserted, updated, skipped, filtered
        );
    }
}

fn seed_brands(storage: &Arc<dyn CatalogStorage>) {
    let entries = vec![
        ("groq", "Groq", Some("GROQ_API_KEY"), None),
        ("mistral", "Mistral AI", Some("MISTRAL_API_KEY"), None),
        ("ollama", "Ollama", None, Some("http://localhost:11434")),
    ];
    for (slug, name, api_key_env, base_url) in entries {
        let brand = Brand {
            id: Uuid::new_v4(),
            slug: slug.to_string(),
            name: name.to_string(),
            api_key_env: api_key_env.map(|s| s.to_string()),
            base_url: base_url.map(|s| s.to_string()),
            is_active: true,
            plan: None,
            priority: 0,
            created_at: chrono::Utc::now(),
        };
        storage.insert_brand(&brand).unwrap();
        println!("seeded brand: {slug}");
    }
}

fn seed_models(storage: &Arc<dyn CatalogStorage>) {
    let brands: std::collections::HashMap<String, Brand> = storage
        .load_brands()
        .unwrap()
        .into_iter()
        .map(|b| (b.slug.clone(), b))
        .collect();

    // (brand_slug, slug, display_name, max_ctx, fn_call, json_mode, price_in, price_out, tpm, rpm, quality, latency_ms)
    #[allow(clippy::type_complexity)]
    let models: &[(
        &str,
        &str,
        &str,
        u32,
        bool,
        bool,
        Option<f64>,
        Option<f64>,
        Option<u32>,
        Option<u32>,
        Option<f32>,
        Option<u32>,
    )] = &[
        // Groq
        (
            "groq",
            "llama-3.3-70b-versatile",
            "Llama 3.3 70B Versatile",
            128_000,
            true,
            true,
            Some(0.59),
            Some(0.79),
            Some(131_072),
            Some(30),
            Some(0.82),
            Some(250),
        ),
        (
            "groq",
            "llama-3.1-8b-instant",
            "Llama 3.1 8B Instant",
            128_000,
            true,
            true,
            Some(0.05),
            Some(0.08),
            Some(131_072),
            Some(30),
            Some(0.55),
            Some(120),
        ),
        (
            "groq",
            "gemma2-9b-it",
            "Gemma2 9B",
            8_192,
            false,
            true,
            Some(0.20),
            Some(0.20),
            None,
            Some(30),
            Some(0.58),
            Some(150),
        ),
        // Mistral
        (
            "mistral",
            "mistral-small-latest",
            "Mistral Small",
            32_000,
            true,
            true,
            Some(0.10),
            Some(0.30),
            Some(500_000),
            None,
            Some(0.65),
            Some(400),
        ),
        (
            "mistral",
            "mistral-large-2512",
            "Mistral Large",
            128_000,
            true,
            true,
            Some(2.00),
            Some(6.00),
            Some(500_000),
            None,
            Some(0.90),
            Some(800),
        ),
        (
            "mistral",
            "open-mixtral-8x7b",
            "Mixtral 8x7B",
            32_000,
            false,
            true,
            Some(0.70),
            Some(0.70),
            None,
            None,
            Some(0.72),
            Some(500),
        ),
    ];

    for (
        brand_slug,
        slug,
        display_name,
        max_ctx,
        fn_call,
        json_mode,
        price_in,
        price_out,
        tpm,
        rpm,
        quality,
        latency_ms,
    ) in models
    {
        let brand = match brands.get(*brand_slug) {
            Some(b) => b,
            None => {
                eprintln!("brand '{brand_slug}' not found, skipping model '{slug}'");
                continue;
            }
        };
        let model = Model {
            id: Uuid::new_v4(),
            brand_id: brand.id,
            slug: slug.to_string(),
            display_name: display_name.to_string(),
            max_context_tokens: *max_ctx,
            max_output_tokens: None,
            supports_function_calling: *fn_call,
            supports_json_mode: *json_mode,
            price_input_per_1m: *price_in,
            price_output_per_1m: *price_out,
            tpm_limit: *tpm,
            rpm_limit: *rpm,
            rpd_limit: None,
            tpd_limit: None,
            tpm_limit_month: None,
            rps_limit: None,
            quality_score: *quality,
            avg_latency_ms: *latency_ms,
            is_enabled: true,
            notes: None,
            category: None,
            plan: None,
            created_at: chrono::Utc::now(),
        };
        storage.insert_model(&model).unwrap();
        println!("seeded model: {brand_slug}/{slug}");
    }
}
