use proviz_elekto_core::{builtin_providers::load_provider_from_dir, storage::CatalogStorage};
use proviz_elekto_storage_sqlite::SqliteStorage;

#[test]
fn builtin_typesafe_jev_disables_training_on_load_and_reload() {
    let storage = SqliteStorage::open_in_memory().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers");
    for _ in 0..2 {
        load_provider_from_dir(&storage, root.to_str().unwrap(), "typesafe").unwrap();
        let models = storage.load_models().unwrap();
        let jev = models.iter().find(|m| m.slug == "jev-latest").unwrap();
        assert_eq!(jev.trains_on_data, Some(false));
        assert_eq!(jev.retains_data, None);
    }
}
