//! Test to verify MCP schema generation for usize fields

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::{
    AnalyzeImpactRequest, CodeIntelligenceServer, GetIndexInfoRequest, ReindexRequest,
    SearchSymbolsRequest, SemanticSearchRequest,
};
use tempfile::TempDir;
use tokio::sync::RwLock;

#[test]
fn test_mcp_schema_uint_format() {
    println!("\n=== Testing MCP Schema Generation for 'uint' Format Issue ===\n");

    // Test SearchSymbolsRequest schema
    let search_schema = rmcp::schemars::schema_for!(SearchSymbolsRequest);
    let search_json = serde_json::to_string_pretty(&search_schema).unwrap();

    println!("SearchSymbolsRequest schema:");
    println!("{search_json}");

    if search_json.contains(r#""format":"uint"#) {
        println!("\n[WARN] SearchSymbolsRequest contains 'uint' format!");
        println!("   This may cause issues with MCP clients like Gemini.");
    }

    println!("\n{}", "=".repeat(50));

    // Test SemanticSearchRequest schema
    let semantic_schema = rmcp::schemars::schema_for!(SemanticSearchRequest);
    let semantic_json = serde_json::to_string_pretty(&semantic_schema).unwrap();

    println!("\nSemanticSearchRequest schema:");
    println!("{semantic_json}");

    if semantic_json.contains(r#""format":"uint"#) {
        println!("\n[WARN] SemanticSearchRequest contains 'uint' format!");
    }

    println!("\n{}", "=".repeat(50));

    // Test AnalyzeImpactRequest schema
    let impact_schema = rmcp::schemars::schema_for!(AnalyzeImpactRequest);
    let impact_json = serde_json::to_string_pretty(&impact_schema).unwrap();

    println!("\nAnalyzeImpactRequest schema:");
    println!("{impact_json}");

    if impact_json.contains(r#""format":"uint"#) {
        println!("\n[WARN] AnalyzeImpactRequest contains 'uint' format!");
    }

    // Summary
    println!("\n{}", "=".repeat(50));
    println!("SUMMARY:");

    let has_uint = search_json.contains(r#""format":"uint"#)
        || semantic_json.contains(r#""format":"uint"#)
        || impact_json.contains(r#""format":"uint"#);

    if has_uint {
        println!("[FAIL] Schema contains 'uint' format which is not standard JSON Schema.");
        println!("   This causes compatibility issues with MCP clients.");
        println!("   Fix: Change usize fields to u32 or u64 in MCP request structs.");
    } else {
        println!("[OK] No 'uint' format found in schemas.");
    }
}

/// Regression test: `get_index_info` is a no-parameter tool whose inputSchema must satisfy
/// both MCP spec (recommends `additionalProperties: false`) and OpenAI's strict
/// function-calling validation (requires `properties` field).
#[test]
fn test_get_index_info_schema_has_properties() {
    let schema = rmcp::schemars::schema_for!(GetIndexInfoRequest);
    let json = serde_json::to_string_pretty(&schema).unwrap();
    println!("GetIndexInfoRequest schema:\n{json}");

    let root: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(
        root.get("type").and_then(|v| v.as_str()),
        Some("object"),
        "schema must have type=object\nGot:\n{json}"
    );
    assert!(
        root.get("properties").is_some(),
        "schema must contain 'properties' for OpenAI compatibility\nGot:\n{json}"
    );
    assert_eq!(
        root.get("additionalProperties").and_then(|v| v.as_bool()),
        Some(false),
        "schema should set additionalProperties=false per MCP spec\nGot:\n{json}"
    );
    println!("[OK] GetIndexInfoRequest schema is MCP-spec compliant and OpenAI-compatible.");
}

/// Schema regression test for `ReindexRequest` (the `reindex` MCP tool's
/// request struct, `src/mcp/requests.rs`): proves `paths` is present as an
/// optional array-of-string property and `force` is present as a boolean
/// property, both carrying a non-empty description so MCP clients can render
/// useful tool-call UIs instead of bare, undocumented fields.
#[test]
fn test_reindex_request_schema_has_paths_and_force() {
    let schema = rmcp::schemars::schema_for!(ReindexRequest);
    let json = serde_json::to_string_pretty(&schema).unwrap();
    println!("ReindexRequest schema:\n{json}");

    let root: serde_json::Value = serde_json::from_str(&json).unwrap();
    let properties = root
        .get("properties")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!("schema must contain 'properties'\nGot:\n{json}"));

    // `paths`: optional array of string.
    let paths_schema = properties
        .get("paths")
        .unwrap_or_else(|| panic!("schema must contain a 'paths' property\nGot:\n{json}"));
    // `Option<Vec<String>>` renders its `type` as either the bare string
    // `"array"` or, for the nullable/optional case, the two-element array
    // `["array","null"]` (current schemars behavior) -- accept either shape
    // rather than pinning to one, since both faithfully describe "optional
    // array of string".
    let paths_type_is_array = match paths_schema.get("type") {
        Some(serde_json::Value::String(s)) => s == "array",
        Some(serde_json::Value::Array(variants)) => {
            variants.iter().any(|v| v.as_str() == Some("array"))
        }
        _ => false,
    };
    assert!(
        paths_type_is_array,
        "'paths' should be an (optionally-nullable) array schema\nGot:\n{json}"
    );
    let paths_item_type = paths_schema
        .get("items")
        .and_then(|items| items.get("type"))
        .and_then(|t| t.as_str());
    assert_eq!(
        paths_item_type,
        Some("string"),
        "'paths' array items should be typed as string\nGot:\n{json}"
    );
    let paths_description = paths_schema
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !paths_description.is_empty(),
        "'paths' property must carry a non-empty description\nGot:\n{json}"
    );

    // `force`: boolean.
    let force_schema = properties
        .get("force")
        .unwrap_or_else(|| panic!("schema must contain a 'force' property\nGot:\n{json}"));
    assert_eq!(
        force_schema.get("type").and_then(|v| v.as_str()),
        Some("boolean"),
        "'force' should be a boolean schema\nGot:\n{json}"
    );
    let force_description = force_schema
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !force_description.is_empty(),
        "'force' property must carry a non-empty description\nGot:\n{json}"
    );

    println!("[OK] ReindexRequest schema exposes documented 'paths' and 'force' properties.");
}

/// Build a minimal, real (not mocked) `IndexFacade` rooted at a fresh temp
/// workspace, with semantic search disabled so this stays fast and CI-safe.
/// No files need to be indexed: every assertion below only inspects the
/// static tool router (`list_tools`), never facade contents.
fn build_test_facade() -> (TempDir, IndexFacade) {
    let temp_dir = TempDir::new().expect("create temp dir");
    let index_path = temp_dir.path().join(".codanna-index");
    std::fs::create_dir_all(&index_path).expect("create index directory");

    let settings = Arc::new(Settings {
        workspace_root: Some(temp_dir.path().to_path_buf()),
        index_path,
        ..Default::default()
    });
    let facade = IndexFacade::new(settings).expect("create IndexFacade");
    (temp_dir, facade)
}

/// Connect a real `rmcp` client to `server` over an in-process, in-memory
/// duplex pipe (no subprocess, no mocks: a genuine MCP `initialize` +
/// `tools/list` round trip over the real stdio-shaped transport codec) and
/// return the resulting `list_tools` tool names.
async fn list_tool_names(server: CodeIntelligenceServer) -> Vec<String> {
    use rmcp::service::ServiceExt;

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (server_read, server_write) = tokio::io::split(server_io);

    let server_task = tokio::spawn(async move {
        let running = server
            .serve((server_read, server_write))
            .await
            .expect("server should complete the initialize handshake");
        // Keep the server alive until the client cancels/drops its side.
        let _ = running.waiting().await;
    });

    let client = ().serve((client_read, client_write)).await.expect(
        "client should complete the initialize handshake against the in-process server transport",
    );

    let tools = client
        .list_tools(Default::default())
        .await
        .expect("list_tools should succeed over the in-process transport");

    client
        .cancel()
        .await
        .expect("client should shut down cleanly");
    let _ = server_task.await;

    tools
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect()
}

/// W-6(B): proves the `reindex` tool is really wired into `list_tools()` for
/// EACH of the three `CodeIntelligenceServer` constructors used across
/// codanna's serving modes (`new` -- stdio `serve`, `from_facade` -- shared
/// facade / hot-reload watcher, `new_with_facade` -- HTTP server). If the
/// W-4 admin-router wiring were missing from any one constructor, this test
/// would fail against exactly that constructor while the other two still
/// pass -- a mistake a single "some server has reindex" test could hide.
#[tokio::test]
async fn test_reindex_tool_present_in_list_tools_for_all_constructors() {
    // Constructor 1: `new` -- takes ownership of a bare `IndexFacade`.
    {
        let (_temp_dir, facade) = build_test_facade();
        let server = CodeIntelligenceServer::new(facade);
        let names = list_tool_names(server).await;
        assert!(
            names.contains(&"reindex".to_string()),
            "CodeIntelligenceServer::new should list a 'reindex' tool, got: {names:?}"
        );
    }

    // Constructor 2: `from_facade` -- shares an already-`Arc<RwLock<_>>`-wrapped facade.
    {
        let (_temp_dir, facade) = build_test_facade();
        let server = CodeIntelligenceServer::from_facade(Arc::new(RwLock::new(facade)));
        let names = list_tool_names(server).await;
        assert!(
            names.contains(&"reindex".to_string()),
            "CodeIntelligenceServer::from_facade should list a 'reindex' tool, got: {names:?}"
        );
    }

    // Constructor 3: `new_with_facade` -- the HTTP server's construction path.
    {
        let (_temp_dir, facade) = build_test_facade();
        let settings = Arc::new(Settings::default());
        let server =
            CodeIntelligenceServer::new_with_facade(Arc::new(RwLock::new(facade)), settings);
        let names = list_tool_names(server).await;
        assert!(
            names.contains(&"reindex".to_string()),
            "CodeIntelligenceServer::new_with_facade should list a 'reindex' tool, got: {names:?}"
        );
    }

    println!(
        "[OK] 'reindex' tool present in list_tools() for new, from_facade, and new_with_facade."
    );
}
