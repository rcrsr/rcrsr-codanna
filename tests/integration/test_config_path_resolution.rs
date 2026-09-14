//! Test: Path resolution with --config flag
//!
//! This test verifies ONE thing:
//! - When using --config with a relative index_path, it resolves correctly

use codanna::{IndexPersistence, Settings};
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn test_config_relative_path_resolution() {
    // Create a temp directory to simulate being outside the project
    let temp_dir = TempDir::new().unwrap();
    let original_dir = std::env::current_dir().unwrap();

    // Create a fake project structure
    let project_dir = temp_dir.path().join("my_project");
    let codanna_dir = project_dir.join(".codanna");
    let index_dir = codanna_dir.join("index");
    let semantic_dir = index_dir.join("semantic");
    let tantivy_dir = index_dir.join("tantivy");

    std::fs::create_dir_all(&semantic_dir).unwrap();
    std::fs::create_dir_all(&tantivy_dir).unwrap();

    // Create a settings.toml with relative index_path
    let settings_path = codanna_dir.join("settings.toml");
    let settings_content = r#"
version = 1
index_path = ".codanna/index"

[semantic_search]
enabled = true
"#;
    std::fs::write(&settings_path, settings_content).unwrap();

    // Create semantic metadata to prove it exists. Must be a real,
    // parseable `SemanticMetadata` -- `validate_generation` loads and
    // validates it once this fixture is migrated into a generation.
    codanna::semantic::SemanticMetadata::new("test-model".to_string(), 384, 0)
        .save(&semantic_dir)
        .unwrap();

    // Create tantivy meta.json to make the index "exist"
    let tantivy_meta_path = tantivy_dir.join("meta.json");
    std::fs::write(&tantivy_meta_path, "{}").unwrap();

    // A real save_facade run always writes index.meta alongside tantivy/;
    // required for the migrated generation to validate (see
    // `storage::generation::layout::validate_generation`).
    codanna::storage::IndexMetadata::new()
        .save(&index_dir)
        .unwrap();

    // Change to a different directory (simulating --config from outside)
    std::env::set_current_dir(&temp_dir).unwrap();

    // Load settings as if using --config
    let settings = Settings::load_from(&settings_path).expect("Should load settings");

    // The problem: index_path is relative but workspace_root is None
    assert_eq!(settings.index_path, PathBuf::from(".codanna/index"));
    assert!(settings.workspace_root.is_none());

    // This is what currently happens - persistence looks in wrong place
    let wrong_persistence = IndexPersistence::new(settings.index_path.clone());
    assert!(
        !wrong_persistence.exists(),
        "Should not find index at relative path from temp dir"
    );

    // Now test that our resolve_index_path function fixes this
    let resolved_path = codanna::init::resolve_index_path(&settings, Some(&settings_path));

    // The resolved path should point to the project directory, not temp directory
    assert_eq!(resolved_path, project_dir.join(".codanna/index"));

    // And persistence with the resolved path should find the index
    let correct_persistence = IndexPersistence::new(resolved_path);
    assert!(
        correct_persistence.exists(),
        "Should find index at correct resolved path"
    );

    // Restore original directory
    std::env::set_current_dir(original_dir).unwrap();
}
