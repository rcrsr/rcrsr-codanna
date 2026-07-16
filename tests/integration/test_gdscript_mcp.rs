use std::sync::Arc;

use codanna::config::{SemanticSearchConfig, Settings};
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::{
    AnalyzeImpactRequest, CodeIntelligenceServer, FindSymbolRequest, GetCallsRequest,
    SemanticSearchWithContextRequest,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use tempfile::TempDir;

const PLAYER_FIXTURE: &str = include_str!("../fixtures/gdscript/player.gd");
const ENEMY_FIXTURE: &str = include_str!("../fixtures/gdscript/enemies/enemy.gd");
const HEAL_EFFECT_FIXTURE: &str = include_str!("../fixtures/gdscript/effects/heal_effect.gd");

#[tokio::test(flavor = "current_thread")]
#[ignore = "Downloads 86MB embedding model - unsuitable for CI/CD. Run with: cargo test -- --ignored"]
async fn test_gdscript_semantic_search_and_analyze_impact() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let workspace_root = temp_dir.path();

    let fixtures = [
        ("player.gd", PLAYER_FIXTURE),
        ("enemies/enemy.gd", ENEMY_FIXTURE),
        ("effects/heal_effect.gd", HEAL_EFFECT_FIXTURE),
    ];

    for (relative_path, contents) in fixtures {
        let full_path = workspace_root.join(relative_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture directory");
        }
        std::fs::write(&full_path, contents).expect("write fixture");
    }

    let index_path = workspace_root.join(".codanna-index");
    std::fs::create_dir_all(&index_path).expect("create index directory");

    let settings = Settings {
        workspace_root: Some(workspace_root.to_path_buf()),
        index_path: index_path.clone(),
        semantic_search: SemanticSearchConfig {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };

    let settings = Arc::new(settings);
    let mut indexer = IndexFacade::new(settings.clone()).expect("Failed to create IndexFacade");
    indexer
        .enable_semantic_search()
        .expect("enable semantic search");

    for relative in ["player.gd", "enemies/enemy.gd", "effects/heal_effect.gd"] {
        let file_path = workspace_root.join(relative);
        indexer
            .index_file(file_path.to_str().expect("utf8 path"))
            .expect("index fixture file");
    }

    let server = CodeIntelligenceServer::new(indexer);

    let semantic_result = server
        .semantic_search_with_context(Parameters(SemanticSearchWithContextRequest {
            query: "apply damage".to_string(),
            limit: 1,
            threshold: None,
            lang: Some("gdscript".to_string()),
            output_format: Default::default(),
        }))
        .await
        .expect("semantic_search_with_context should succeed");

    let semantic_text = semantic_result
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        semantic_text.contains("apply_damage"),
        "expected semantic output to mention apply_damage, got:\n{semantic_text}"
    );

    let apply_damage_symbol_id = semantic_text
        .split("[symbol_id:")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .and_then(|digits| digits.parse::<u32>().ok())
        .expect("semantic output should expose symbol_id for apply_damage");

    let impact_result = server
        .analyze_impact(Parameters(AnalyzeImpactRequest {
            name: None,
            symbol_id: Some(apply_damage_symbol_id),
            max_depth: 2,
            count_only: false,
            max_results: 0,
            group_by: Default::default(),
            output_format: Default::default(),
        }))
        .await
        .expect("analyze_impact should succeed");

    let impact_text = impact_result
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        impact_text.contains("apply_damage")
            || impact_text.contains("No symbols would be impacted"),
        "expected analyze_impact output to reference apply_damage or report no impacted symbols, got:\n{impact_text}"
    );

    // find_symbol resolves by typed symbol_id alone, with no `name` provided.
    let find_symbol_result = server
        .find_symbol(Parameters(FindSymbolRequest {
            name: String::new(),
            symbol_id: Some(apply_damage_symbol_id),
            lang: None,
            output_format: Default::default(),
        }))
        .await
        .expect("find_symbol should succeed");

    let find_symbol_text = find_symbol_result
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        find_symbol_text.contains("apply_damage"),
        "expected find_symbol(symbol_id) output to reference apply_damage, got:\n{find_symbol_text}"
    );
}

/// find_symbol resolves via typed `symbol_id`, with no `name` needed, and
/// without depending on semantic search / the embedding model download.
#[tokio::test(flavor = "current_thread")]
async fn test_find_symbol_resolves_by_symbol_id_alone() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let workspace_root = temp_dir.path();

    let full_path = workspace_root.join("player.gd");
    std::fs::write(&full_path, PLAYER_FIXTURE).expect("write fixture");

    let index_path = workspace_root.join(".codanna-index");
    std::fs::create_dir_all(&index_path).expect("create index directory");

    let settings = Settings {
        workspace_root: Some(workspace_root.to_path_buf()),
        index_path: index_path.clone(),
        ..Default::default()
    };

    let settings = Arc::new(settings);
    let mut indexer = IndexFacade::new(settings.clone()).expect("Failed to create IndexFacade");
    indexer
        .index_file(full_path.to_str().expect("utf8 path"))
        .expect("index fixture file");

    let server = CodeIntelligenceServer::new(indexer);

    // Look up by name first to obtain a concrete symbol_id.
    let by_name = server
        .find_symbol(Parameters(FindSymbolRequest {
            name: "apply_damage".to_string(),
            symbol_id: None,
            lang: None,
            output_format: Default::default(),
        }))
        .await
        .expect("find_symbol by name should succeed");

    let by_name_text = by_name
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let symbol_id = by_name_text
        .split("symbol_id:")
        .nth(1)
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .map(str::to_string)
        })
        .and_then(|digits| digits.parse::<u32>().ok())
        .expect("find_symbol output should expose a symbol_id for apply_damage");

    // Now resolve using symbol_id alone, no name.
    let by_id = server
        .find_symbol(Parameters(FindSymbolRequest {
            name: String::new(),
            symbol_id: Some(symbol_id),
            lang: None,
            output_format: Default::default(),
        }))
        .await
        .expect("find_symbol by symbol_id should succeed");

    let by_id_text = by_id
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        by_id_text.contains("apply_damage"),
        "expected find_symbol(symbol_id) output to reference apply_damage, got:\n{by_id_text}"
    );
}

/// `GetCallsRequest` accepts both the canonical `name` key and the legacy
/// `function_name` key via `#[serde(alias)]`.
#[test]
fn test_get_calls_request_accepts_name_and_legacy_alias() {
    let via_canonical: GetCallsRequest =
        serde_json::from_value(serde_json::json!({"name": "x"})).expect("canonical key parses");
    assert_eq!(via_canonical.name.as_deref(), Some("x"));

    let via_alias: GetCallsRequest =
        serde_json::from_value(serde_json::json!({"function_name": "x"}))
            .expect("legacy alias parses");
    assert_eq!(via_alias.name.as_deref(), Some("x"));
}

/// `AnalyzeImpactRequest` accepts both the canonical `name` key and the
/// legacy `symbol_name` key via `#[serde(alias)]`.
#[test]
fn test_analyze_impact_request_accepts_name_and_legacy_alias() {
    let via_canonical: AnalyzeImpactRequest =
        serde_json::from_value(serde_json::json!({"name": "x", "max_depth": 3}))
            .expect("canonical key parses");
    assert_eq!(via_canonical.name.as_deref(), Some("x"));

    let via_alias: AnalyzeImpactRequest =
        serde_json::from_value(serde_json::json!({"symbol_name": "x", "max_depth": 3}))
            .expect("legacy alias parses");
    assert_eq!(via_alias.name.as_deref(), Some("x"));
}
