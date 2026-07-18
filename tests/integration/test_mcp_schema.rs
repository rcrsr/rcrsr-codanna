//! Test to verify MCP schema generation for usize fields

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::{
    AnalyzeImpactRequest, CodeIntelligenceServer, GetIndexInfoRequest, ReindexRequest,
    SearchDocumentsRequest, SearchSymbolsRequest, SemanticSearchRequest,
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

/// Schema regression test for `ReindexRequest`'s `documents` field: proves it
/// is present as a boolean property carrying a non-empty description,
/// mirroring [`test_reindex_request_schema_has_paths_and_force`] above.
#[test]
fn test_reindex_request_schema_has_documents() {
    let schema = rmcp::schemars::schema_for!(ReindexRequest);
    let json = serde_json::to_string_pretty(&schema).unwrap();

    let root: serde_json::Value = serde_json::from_str(&json).unwrap();
    let properties = root
        .get("properties")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!("schema must contain 'properties'\nGot:\n{json}"));

    let documents_schema = properties
        .get("documents")
        .unwrap_or_else(|| panic!("schema must contain a 'documents' property\nGot:\n{json}"));
    assert_eq!(
        documents_schema.get("type").and_then(|v| v.as_str()),
        Some("boolean"),
        "'documents' should be a boolean schema\nGot:\n{json}"
    );
    let documents_description = documents_schema
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !documents_description.is_empty(),
        "'documents' property must carry a non-empty description\nGot:\n{json}"
    );

    println!("[OK] ReindexRequest schema exposes documented 'documents' property.");
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

/// `reindex documents:true` discovers new files added to a configured
/// markdown collection since the document store was last synced, and
/// aggregates non-zero totals across the collection; `documents:false`
/// (the default) leaves the document store untouched and reports
/// `documents: None`, proving the flag actually gates document reindexing
/// rather than always running it.
#[tokio::test]
async fn test_reindex_documents_flag_discovers_new_files() {
    use codanna::documents::{CollectionConfig, DocumentStore};
    use codanna::mcp::requests::OutputFormat;
    use codanna::vector::VectorDimension;
    use rmcp::handler::server::wrapper::Parameters;

    let temp_dir = TempDir::new().expect("create temp dir");
    let docs_dir = temp_dir.path().join("docs");
    std::fs::create_dir_all(&docs_dir).expect("create docs dir");
    std::fs::write(docs_dir.join("first.md"), "# First\n\nSome content.\n")
        .expect("write first.md fixture");

    let index_path = temp_dir.path().join(".codanna-index");
    std::fs::create_dir_all(&index_path).expect("create index directory");

    let mut collections = std::collections::HashMap::new();
    collections.insert(
        "docs".to_string(),
        CollectionConfig {
            paths: vec![docs_dir.clone()],
            ..Default::default()
        },
    );

    let settings = Settings {
        workspace_root: Some(temp_dir.path().to_path_buf()),
        index_path: index_path.clone(),
        documents: codanna::documents::DocumentsConfig {
            enabled: true,
            collections,
            ..Default::default()
        },
        ..Default::default()
    };

    let collection_config = settings.documents.collections["docs"].clone();
    let chunking_defaults = settings.documents.defaults.clone();

    let facade = IndexFacade::new(Arc::new(settings)).expect("create facade over temp index");

    // Pre-sync the document store once (mirrors a prior `codanna documents
    // index` run) so the server starts with a document store already
    // configured, matching `document_store: Option<...>` being populated at
    // server construction in every real serving mode.
    let mut store = DocumentStore::new(
        index_path.join("documents"),
        VectorDimension::dimension_384(),
    )
    .expect("create document store");
    store
        .index_collection("docs", &collection_config, &chunking_defaults)
        .expect("pre-sync docs collection");

    let server = CodeIntelligenceServer::new(facade).with_document_store(store);

    // `documents:false` (the default): the reindex must not touch the
    // document store at all.
    let result_no_documents = server
        .reindex(Parameters(ReindexRequest {
            paths: None,
            force: false,
            output_format: OutputFormat::Json,
            documents: false,
        }))
        .await
        .expect("reindex with documents:false should succeed");
    let json_no_documents = call_tool_result_json(&result_no_documents);
    assert_eq!(
        json_no_documents
            .get("data")
            .and_then(|d| d.get("documents")),
        None,
        "documents:false must report documents: None (omitted), got: {json_no_documents:?}"
    );

    // Add a new file to the collection, then reindex with documents:true.
    std::fs::write(docs_dir.join("second.md"), "# Second\n\nMore content.\n")
        .expect("write second.md fixture");

    let result_with_documents = server
        .reindex(Parameters(ReindexRequest {
            paths: None,
            force: false,
            output_format: OutputFormat::Json,
            documents: true,
        }))
        .await
        .expect("reindex with documents:true should succeed");
    let json_with_documents = call_tool_result_json(&result_with_documents);
    let documents_data = json_with_documents
        .get("data")
        .and_then(|d| d.get("documents"))
        .unwrap_or_else(|| {
            panic!(
                "documents:true must report a non-null 'documents' totals object, got: {json_with_documents:?}"
            )
        });

    assert_eq!(
        documents_data.get("collections").and_then(|v| v.as_u64()),
        Some(1),
        "expected exactly the one configured 'docs' collection to be processed, got: {documents_data:?}"
    );
    let files_processed = documents_data
        .get("files_processed")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert!(
        files_processed >= 1,
        "expected the newly added second.md to be discovered and processed, got: {documents_data:?}"
    );
    let chunks_created = documents_data
        .get("chunks_created")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert!(
        chunks_created >= 1,
        "expected at least one new chunk from second.md, got: {documents_data:?}"
    );

    println!(
        "[OK] reindex documents:true discovers new collection files; documents:false reports documents: None."
    );
}

/// `SearchDocumentsRequest.collection` accepts both a bare string (backward
/// compatible with existing clients) and an array of strings (multi-select),
/// deserializing the same JSON key `"collection"` in both shapes, and the
/// schema still renders successfully for either representation.
#[test]
fn test_search_documents_request_collection_accepts_string_or_array() {
    let single: SearchDocumentsRequest =
        serde_json::from_value(serde_json::json!({"query": "auth", "collection": "docs"}))
            .expect("collection as a bare string must deserialize");
    let single_vec = single
        .collection
        .expect("collection must be present")
        .into_vec();
    assert_eq!(single_vec, vec!["docs".to_string()]);

    let many: SearchDocumentsRequest = serde_json::from_value(serde_json::json!({
        "query": "auth",
        "collection": ["a", "b"],
    }))
    .expect("collection as an array must deserialize");
    let many_vec = many
        .collection
        .expect("collection must be present")
        .into_vec();
    assert_eq!(many_vec, vec!["a".to_string(), "b".to_string()]);

    // No collection at all must still deserialize (field is optional).
    let none: SearchDocumentsRequest = serde_json::from_value(serde_json::json!({"query": "auth"}))
        .expect("omitted collection must deserialize");
    assert!(none.collection.is_none());

    // Schema generation must still succeed for the untagged one-or-many shape.
    let schema = rmcp::schemars::schema_for!(SearchDocumentsRequest);
    let json = serde_json::to_string_pretty(&schema).unwrap();
    let root: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        root.get("properties").is_some(),
        "SearchDocumentsRequest schema must render properties\nGot:\n{json}"
    );

    println!(
        "[OK] SearchDocumentsRequest.collection deserializes from both a bare string and an array."
    );
}

/// Extract and parse the single JSON text content block out of a
/// `CallToolResult` produced by `output_format: Json` tool calls.
fn call_tool_result_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .find_map(|block| block.as_text().map(|t| t.text.clone()))
        .expect("CallToolResult must contain a text content block");
    serde_json::from_str(&text).expect("tool JSON output must parse as valid JSON")
}
