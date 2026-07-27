//! Test to verify MCP schema generation for usize fields

use std::sync::Arc;

use codanna::config::Settings;
use codanna::indexing::facade::IndexFacade;
use codanna::mcp::{
    AnalyzeImpactRequest, CallerFilter, CodeIntelligenceServer, FindCallersRequest,
    FindSymbolRequest, GetCallsRequest, GetIndexInfoRequest, GroupBy, OutputFormat, ReindexRequest,
    SearchDocumentsRequest, SearchSymbolsRequest, SemanticSearchRequest,
};
use rmcp::handler::server::wrapper::Parameters;
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

/// Extract the raw text of the single content block out of a
/// `CallToolResult` produced by `output_format: Text` (default) tool
/// calls, without attempting a JSON parse.
fn call_tool_result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|block| block.as_text().map(|t| t.text.clone()))
        .expect("CallToolResult must contain a text content block")
}

// ===========================================================================
// W-11: request/consumer regression coverage for fork-added request fields.
// ===========================================================================

/// Assertion group 1: each of the 4 fork-documented legacy aliases
/// (`function_name` on `GetCallsRequest`, `function_name` on
/// `FindCallersRequest`, `symbol_name` on `AnalyzeImpactRequest`, `depth`
/// on `AnalyzeImpactRequest`) still deserializes into its canonical field
/// (`name` / `max_depth`) even though every one of these structs also
/// carries `#[serde(deny_unknown_fields)]`. Fails on a future edit that
/// adds `deny_unknown_fields` to a struct without carrying the field's
/// `#[serde(alias = "...")]` attribute along with it -- serde's
/// `deny_unknown_fields` machinery only recognizes a key as "known" when
/// the alias is declared on the very field it targets, so a dropped
/// alias attribute breaks exactly this deserialization, not a compile
/// error.
#[test]
fn legacy_field_aliases_deserialize_under_deny_unknown_fields() {
    let get_calls: GetCallsRequest =
        serde_json::from_value(serde_json::json!({"function_name": "caller"}))
            .expect("GetCallsRequest.function_name alias must deserialize into name");
    assert_eq!(get_calls.name.as_deref(), Some("caller"));

    let find_callers: FindCallersRequest =
        serde_json::from_value(serde_json::json!({"function_name": "callee"}))
            .expect("FindCallersRequest.function_name alias must deserialize into name");
    assert_eq!(find_callers.name.as_deref(), Some("callee"));

    let analyze_impact_name: AnalyzeImpactRequest =
        serde_json::from_value(serde_json::json!({"symbol_name": "target"}))
            .expect("AnalyzeImpactRequest.symbol_name alias must deserialize into name");
    assert_eq!(analyze_impact_name.name.as_deref(), Some("target"));

    let analyze_impact_depth: AnalyzeImpactRequest =
        serde_json::from_value(serde_json::json!({"name": "target", "depth": 7}))
            .expect("AnalyzeImpactRequest.depth alias must deserialize into max_depth");
    assert_eq!(analyze_impact_depth.max_depth, 7);

    println!("[OK] all 4 legacy field aliases deserialize under deny_unknown_fields.");
}

/// Assertion group 2: every fork-added request field deserializes on its
/// owning struct, checked one field at a time with a minimal JSON
/// payload. Fails, per-field, if any of `output_format`, `count_only`,
/// `max_results`, `group_by`, `filter`, `collection`,
/// `exclude_collections`, `documents`, `force`, or `paths` is silently
/// dropped from its struct definition -- e.g. by an upstream merge that
/// replaces the struct wholesale and only keeps the fields upstream
/// itself defines.
#[test]
fn fork_added_fields_deserialize_on_owning_structs() {
    let output_format: FindSymbolRequest =
        serde_json::from_value(serde_json::json!({"name": "x", "output_format": "json"}))
            .expect("output_format must deserialize on FindSymbolRequest");
    assert_eq!(output_format.output_format, OutputFormat::Json);

    let count_only: FindCallersRequest =
        serde_json::from_value(serde_json::json!({"name": "x", "count_only": true}))
            .expect("count_only must deserialize on FindCallersRequest");
    assert!(count_only.count_only);

    let max_results: AnalyzeImpactRequest =
        serde_json::from_value(serde_json::json!({"name": "x", "max_results": 50}))
            .expect("max_results must deserialize on AnalyzeImpactRequest");
    assert_eq!(max_results.max_results, 50);

    let group_by: AnalyzeImpactRequest =
        serde_json::from_value(serde_json::json!({"name": "x", "group_by": "file"}))
            .expect("group_by must deserialize on AnalyzeImpactRequest");
    assert_eq!(group_by.group_by, GroupBy::File);

    let filter: FindCallersRequest =
        serde_json::from_value(serde_json::json!({"name": "x", "filter": "production"}))
            .expect("filter must deserialize on FindCallersRequest");
    assert_eq!(filter.filter, CallerFilter::Production);

    let collection: SearchDocumentsRequest =
        serde_json::from_value(serde_json::json!({"query": "q", "collection": "docs"}))
            .expect("collection must deserialize on SearchDocumentsRequest");
    assert!(collection.collection.is_some());

    let exclude_collections: SearchDocumentsRequest = serde_json::from_value(serde_json::json!({
        "query": "q",
        "exclude_collections": ["a", "b"],
    }))
    .expect("exclude_collections must deserialize on SearchDocumentsRequest");
    assert_eq!(
        exclude_collections.exclude_collections,
        Some(vec!["a".to_string(), "b".to_string()])
    );

    let documents: ReindexRequest = serde_json::from_value(serde_json::json!({"documents": true}))
        .expect("documents must deserialize on ReindexRequest");
    assert!(documents.documents);

    let force: ReindexRequest = serde_json::from_value(serde_json::json!({"force": true}))
        .expect("force must deserialize on ReindexRequest");
    assert!(force.force);

    let paths: ReindexRequest =
        serde_json::from_value(serde_json::json!({"paths": ["src", "tests"]}))
            .expect("paths must deserialize on ReindexRequest");
    assert_eq!(
        paths.paths,
        Some(vec!["src".to_string(), "tests".to_string()])
    );

    println!("[OK] all fork-added fields deserialize on their owning structs.");
}

/// Assertion group 3: `{"bogus": 1}` is rejected on more than one
/// `deny_unknown_fields` struct. Fails on a "shadow" regression where
/// `#[serde(deny_unknown_fields)]` is written in source but not actually
/// live at derive time on a given struct (e.g. a manual `Deserialize`
/// impl added later that bypasses the derive, or a merge that drops the
/// attribute from one struct's derive line while leaving it textually
/// present elsewhere) -- checking >= 2 independently-attributed structs
/// means a shadow on any single struct still gets caught by the others
/// failing, rather than one passing struct hiding a broken sibling.
#[test]
fn bogus_field_rejected_across_multiple_structs() {
    assert!(
        serde_json::from_value::<GetCallsRequest>(serde_json::json!({"name": "x", "bogus": 1}))
            .is_err(),
        "GetCallsRequest must reject an unknown field ('bogus')"
    );
    assert!(
        serde_json::from_value::<FindCallersRequest>(serde_json::json!({"name": "x", "bogus": 1}))
            .is_err(),
        "FindCallersRequest must reject an unknown field ('bogus')"
    );
    assert!(
        serde_json::from_value::<AnalyzeImpactRequest>(
            serde_json::json!({"name": "x", "bogus": 1})
        )
        .is_err(),
        "AnalyzeImpactRequest must reject an unknown field ('bogus')"
    );

    println!("[OK] {{\"bogus\":1}} rejected across 3 independent deny_unknown_fields structs.");
}

/// Assertion group 4: `GetIndexInfoRequest` accepts
/// `{"output_format":"json"}` AND the `get_index_info` tool actually
/// returns an `Envelope`-shaped JSON payload (a `status`/`code`/`data`
/// object), not the pre-existing human-readable text string. Fails on a
/// future merge silently re-adopting upstream's empty
/// `GetIndexInfoRequest {}` (no `output_format` field, see W-2's
/// PRESERVED INVENTORY): if `deny_unknown_fields` were also lost in that
/// swap, `{"output_format":"json"}` would still deserialize successfully
/// (the unknown key silently discarded) and the tool would always return
/// plain text -- this assertion inspects the actual returned payload
/// shape, not just deserialization success, so that silent win by the
/// upstream empty struct cannot hide behind a passing `from_value`.
#[tokio::test]
async fn get_index_info_request_accepts_json_output_format_and_returns_envelope() {
    let request: GetIndexInfoRequest =
        serde_json::from_value(serde_json::json!({"output_format": "json"}))
            .expect("GetIndexInfoRequest must accept output_format:json");
    assert_eq!(request.output_format, OutputFormat::Json);

    let (_temp_dir, facade) = build_test_facade();
    let server = CodeIntelligenceServer::new(facade);

    let result = server
        .get_index_info(Parameters(request))
        .await
        .expect("get_index_info should succeed");

    let envelope = call_tool_result_json(&result);
    assert!(
        envelope.get("status").is_some() && envelope.get("code").is_some(),
        "output_format:json must return an Envelope-shaped payload (status/code fields present), got: {envelope:?}"
    );
    assert!(
        envelope.get("data").is_some(),
        "envelope must carry a 'data' key (present, even if null) -- a bare text string has no such key, got: {envelope:?}"
    );

    println!("[OK] GetIndexInfoRequest output_format:json returns an Envelope, not plain text.");
}

/// Assertion group 5: `IndexMetadata` JSON that predates the fork's
/// `ignore_fingerprint` field and upstream's `emission_version` field
/// (i.e. a bare pre-gate `index.meta` on disk) still `load()`s
/// successfully, with both fields reading back as `None`. Fails if a
/// future new `IndexMetadata` field is added as a required
/// (non-`Option`, non-`#[serde(default)]`) field: that would turn every
/// existing on-disk index's `index.meta` into a hard parse error on the
/// next `load()` instead of degrading gracefully, bricking the index
/// rather than just reporting the new field as unknown/absent.
///
/// A third field, `builder_commit`, arrived with the upstream v0.12.0
/// merge (`storage::metadata`) and is asserted here too. An earlier
/// revision of this comment recorded that no such field existed in
/// either merge parent; that was accurate against the v0.10.0 base and
/// is not anymore. It is exactly the shape this test guards -- a new
/// `Option` field whose absence from a legacy `index.meta` must read
/// back as `None` rather than failing the parse.
#[test]
fn legacy_index_metadata_json_loads_with_new_optional_fields_none() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let base_path = temp_dir.path();

    let legacy_meta = serde_json::json!({
        "version": 1,
        "data_source": "Fresh",
        "symbol_count": 3,
        "file_count": 1,
        "last_modified": 0
    });
    std::fs::write(
        base_path.join("index.meta"),
        serde_json::to_string_pretty(&legacy_meta).unwrap(),
    )
    .expect("write legacy index.meta fixture");

    let metadata = codanna::storage::IndexMetadata::load(base_path)
        .expect("load() must succeed on metadata lacking ignore_fingerprint/emission_version");

    assert_eq!(
        metadata.ignore_fingerprint, None,
        "ignore_fingerprint must degrade to None on legacy metadata, not error"
    );
    assert_eq!(
        metadata.emission_version, None,
        "emission_version must degrade to None on legacy metadata, not error"
    );
    assert_eq!(
        metadata.builder_commit, None,
        "builder_commit must degrade to None on legacy metadata, not error"
    );

    println!(
        "[OK] legacy IndexMetadata JSON (no ignore_fingerprint/emission_version/builder_commit) loads with all None."
    );
}

/// Assertion group 6: `get_index_info`'s text-mode staleness warning
/// (`"Warning: index may be stale: ignore rules changed since last
/// index"`) appears if and only if the on-disk `ignore_fingerprint`
/// differs from one freshly recomputed from current settings -- checked
/// both ways (matching -> absent, mismatched -> present) against the
/// exact live string. Fails on a relocated or hardcoded warning string
/// (e.g. a copy of the message elsewhere that drifts from the real one
/// `service::ignore_rules_changed` guards), and fails on the warning
/// becoming unconditional (always shown) or permanently suppressed
/// (never shown) regardless of the actual fingerprint comparison.
#[tokio::test]
async fn get_index_info_text_staleness_warning_matches_fingerprint_mismatch() {
    use codanna::indexing::walk_config::ignore_fingerprint;

    const WARNING: &str = "Warning: index may be stale: ignore rules changed since last index";

    // Matching fingerprint: warning must be absent.
    {
        let (temp_dir, facade) = build_test_facade();
        let root = temp_dir.path();
        let current = ignore_fingerprint(facade.settings(), root)
            .expect("compute current ignore fingerprint");

        let mut metadata = codanna::storage::IndexMetadata::new();
        metadata.update_ignore_fingerprint(current);
        metadata
            .save(facade.index_base())
            .expect("save matching-fingerprint metadata");

        let server = CodeIntelligenceServer::new(facade);
        let result = server
            .get_index_info(Parameters(GetIndexInfoRequest {
                output_format: OutputFormat::Text,
            }))
            .await
            .expect("get_index_info should succeed");
        let text = call_tool_result_text(&result);
        assert!(
            !text.contains(WARNING),
            "matching fingerprint must not emit the staleness warning, got:\n{text}"
        );
    }

    // Mismatched fingerprint: warning must be present, verbatim.
    {
        let (_temp_dir, facade) = build_test_facade();
        let mut metadata = codanna::storage::IndexMetadata::new();
        metadata.update_ignore_fingerprint("deliberately-mismatched-fingerprint".to_string());
        metadata
            .save(facade.index_base())
            .expect("save mismatched-fingerprint metadata");

        let server = CodeIntelligenceServer::new(facade);
        let result = server
            .get_index_info(Parameters(GetIndexInfoRequest {
                output_format: OutputFormat::Text,
            }))
            .await
            .expect("get_index_info should succeed");
        let text = call_tool_result_text(&result);
        assert!(
            text.contains(WARNING),
            "mismatched fingerprint must emit the exact staleness warning string, got:\n{text}"
        );
    }

    println!(
        "[OK] get_index_info text staleness warning appears iff the on-disk fingerprint mismatches."
    );
}

/// Assertion group 7: extends
/// [`test_reindex_tool_present_in_list_tools_for_all_constructors`]'s
/// single-tool ("does 'reindex' appear") check to the *complete* expected
/// 13-tool set, for each of the three `CodeIntelligenceServer`
/// constructors independently. Fails on an upstream tool wired into only
/// one or two of the three constructors' routers (a mistake a single
/// "some server has tool X" check would hide, since the other two
/// constructors would still pass), and fails on a fork tool (e.g.
/// `reindex`) dropped from any one constructor's composed router set.
#[tokio::test]
async fn full_tool_set_matches_for_all_constructors() {
    let expected: std::collections::BTreeSet<&str> = [
        "find_symbol",
        "find_symbols",
        "get_calls",
        "find_callers",
        "analyze_impact",
        "get_index_info",
        "search_symbols",
        "semantic_search_docs",
        "semantic_search_with_context",
        "search_documents",
        "reindex",
        "get_file_outline",
        "read_symbol",
    ]
    .into_iter()
    .collect();

    // Constructor 1: `new` -- takes ownership of a bare `IndexFacade`.
    {
        let (_temp_dir, facade) = build_test_facade();
        let server = CodeIntelligenceServer::new(facade);
        let names = list_tool_names(server).await;
        let actual: std::collections::BTreeSet<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(
            actual, expected,
            "CodeIntelligenceServer::new tool set mismatch, got: {names:?}"
        );
    }

    // Constructor 2: `from_facade` -- shares an already-`Arc<RwLock<_>>`-wrapped facade.
    {
        let (_temp_dir, facade) = build_test_facade();
        let server = CodeIntelligenceServer::from_facade(Arc::new(RwLock::new(facade)));
        let names = list_tool_names(server).await;
        let actual: std::collections::BTreeSet<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(
            actual, expected,
            "CodeIntelligenceServer::from_facade tool set mismatch, got: {names:?}"
        );
    }

    // Constructor 3: `new_with_facade` -- the HTTP server's construction path.
    {
        let (_temp_dir, facade) = build_test_facade();
        let settings = Arc::new(Settings::default());
        let server =
            CodeIntelligenceServer::new_with_facade(Arc::new(RwLock::new(facade)), settings);
        let names = list_tool_names(server).await;
        let actual: std::collections::BTreeSet<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(
            actual, expected,
            "CodeIntelligenceServer::new_with_facade tool set mismatch, got: {names:?}"
        );
    }

    println!("[OK] full 13-tool set matches expected for new, from_facade, and new_with_facade.");
}

/// Index a fixture where `dup` is defined identically in two separate
/// files -- an ambiguous-by-construction fixture, so `get_calls`'s name
/// resolution cannot pick a "correct" one and must report `Ambiguous`.
async fn build_ambiguous_server() -> (TempDir, CodeIntelligenceServer) {
    let temp = TempDir::new().expect("create temp dir");
    let src_dir = temp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(src_dir.join("dup_a.py"), "def dup():\n    pass\n")
        .expect("write dup_a.py fixture");
    std::fs::write(src_dir.join("dup_b.py"), "def dup():\n    pass\n")
        .expect("write dup_b.py fixture");

    let settings = Settings {
        index_path: temp.path().join("index"),
        workspace_root: None,
        ..Default::default()
    };
    let mut facade =
        IndexFacade::new(Arc::new(settings)).expect("create facade over temp index dir");
    facade
        .index_directory(&src_dir, false)
        .expect("index fixture directory");

    (temp, CodeIntelligenceServer::new(facade))
}

/// Assertion group 8 (MCP half): an ambiguous name resolved through the
/// `output_format: json` MCP path reports `ResultCode::Ambiguous`
/// (`"code":"AMBIGUOUS"`) and the envelope's declared `exit_code` is `3`
/// -- not the generic error `exit_code` of `2`. Fails on the exit-code
/// 2-vs-3 regression: a merge that routes the ambiguous outcome through
/// `Envelope::error()` (which always stamps `exit_code: 2`) instead of
/// the dedicated `Envelope::ambiguous()` constructor (`exit_code: 3`)
/// would still report `status: "ambiguous"` correctly while silently
/// getting the machine-readable exit code wrong -- a mistake a
/// status-only assertion (as in the pre-existing `get_calls_json_output_
/// format_ambiguous` test) cannot catch.
#[tokio::test]
async fn ambiguous_name_yields_ambiguous_code_and_exit_code_3_via_mcp_json() {
    let (_temp_dir, server) = build_ambiguous_server().await;

    let result = server
        .get_calls(Parameters(GetCallsRequest {
            name: Some("dup".to_string()),
            symbol_id: None,
            output_format: OutputFormat::Json,
        }))
        .await
        .expect("get_calls should succeed (ambiguity is not a tool error)");

    let envelope = call_tool_result_json(&result);
    assert_eq!(
        envelope.get("code").and_then(|v| v.as_str()),
        Some("AMBIGUOUS"),
        "ambiguous get_calls must report ResultCode::Ambiguous, got: {envelope:?}"
    );
    assert_eq!(
        envelope.get("exit_code").and_then(|v| v.as_u64()),
        Some(3),
        "ambiguous get_calls must report exit_code 3, not the generic error exit_code 2, got: {envelope:?}"
    );

    println!(
        "[OK] MCP output_format:json ambiguous get_calls reports code=AMBIGUOUS, exit_code=3."
    );
}

/// Locate the `codanna` binary for subprocess CLI testing, building it
/// on demand if neither the Cargo-provided path nor a prior debug build
/// is available. Self-contained (not shared with
/// `tests/cli/test_mcp_exit_code_matrix.rs`) because this work item is
/// restricted to appending to this single file.
fn locate_codanna_binary_for_cli_ambiguity_test() -> std::path::PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_codanna") {
        let bin = std::path::PathBuf::from(path);
        if bin.exists() {
            return bin;
        }
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("current dir"));

    let debug_bin = if cfg!(windows) {
        manifest_dir.join("target/debug/codanna.exe")
    } else {
        manifest_dir.join("target/debug/codanna")
    };
    if debug_bin.exists() {
        return debug_bin;
    }

    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "codanna"])
        .current_dir(&manifest_dir)
        .status()
        .expect("build codanna binary");
    assert!(status.success(), "cargo build failed");
    debug_bin
}

/// Run the `codanna` binary as a subprocess against `workspace`, returning
/// `(process_exit_code, stdout, stderr)`.
fn run_codanna_cli_for_ambiguity_test(
    workspace: &std::path::Path,
    args: &[&str],
) -> (i32, String, String) {
    let bin = locate_codanna_binary_for_cli_ambiguity_test();
    let test_home = workspace.join(".home");
    std::fs::create_dir_all(&test_home).expect("create test home");

    let output = std::process::Command::new(&bin)
        .args(args)
        .current_dir(workspace)
        .env("HOME", &test_home)
        .output()
        .expect("run codanna CLI");

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// Assertion group 8 (CLI half): the same ambiguous-name regression as
/// [`ambiguous_name_yields_ambiguous_code_and_exit_code_3_via_mcp_json`],
/// exercised through the actual `codanna mcp <tool> --json` CLI
/// subprocess path (`src/cli/commands/mcp.rs`) instead of an in-process
/// `CodeIntelligenceServer` call. Fails on the same exit-code 2-vs-3
/// regression as the MCP half, but on the CLI's independent
/// envelope-to-process-exit plumbing (`exit_ambiguous`,
/// `std::process::exit(envelope.exit_code.into())`); also fails if a
/// future refactor relocates the CLI's ambiguity handling into a helper
/// that no longer round-trips the declared `exit_code` into the real
/// process exit status, since this assertion checks the actual spawned
/// process's exit code, not just the JSON payload.
#[test]
fn ambiguous_name_yields_ambiguous_code_and_exit_code_3_via_cli_json() {
    let workspace = TempDir::new().expect("temp dir");
    let src = workspace.path().join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("alpha.rs"),
        "pub fn dup_name() -> i32 {\n    1\n}\n",
    )
    .expect("write alpha fixture");
    std::fs::write(
        src.join("beta.rs"),
        "pub fn dup_name() -> i32 {\n    2\n}\n",
    )
    .expect("write beta fixture");

    let codanna_dir = workspace.path().join(".codanna");
    std::fs::create_dir_all(&codanna_dir).expect("create .codanna");
    let src_abs = src.canonicalize().expect("canonicalize src dir");
    let src_path = src_abs.to_str().expect("utf8 src path");
    let settings = format!(
        "index_path = \".codanna/index\"\n\n[indexing]\nindexed_paths = [\"{src_path}\"]\n\n[semantic_search]\nenabled = false\n"
    );
    std::fs::write(codanna_dir.join("settings.toml"), settings).expect("write settings");

    let (index_code, index_stdout, index_stderr) = run_codanna_cli_for_ambiguity_test(
        workspace.path(),
        &["index", "src", "--force", "--no-progress"],
    );
    assert_eq!(
        index_code, 0,
        "index should succeed\nstdout:\n{index_stdout}\nstderr:\n{index_stderr}"
    );

    let (code, stdout, stderr) = run_codanna_cli_for_ambiguity_test(
        workspace.path(),
        &["mcp", "get_calls", "function_name:dup_name", "--json"],
    );
    assert_eq!(
        code, 3,
        "process exit for an ambiguous get_calls CLI call must be 3 (ResultCode::Ambiguous), \
         not the generic error exit code 2 -- this is the exit-code 2-vs-3 regression\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let envelope: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not a JSON envelope: {e}\nstdout:\n{stdout}"));
    assert_eq!(
        envelope.get("code").and_then(|v| v.as_str()),
        Some("AMBIGUOUS"),
        "ambiguous CLI --json call must report ResultCode::Ambiguous, got: {envelope}"
    );
    assert_eq!(
        envelope.get("exit_code").and_then(|v| v.as_i64()),
        Some(3),
        "ambiguous CLI --json call's declared exit_code must be 3, got: {envelope}"
    );

    println!(
        "[OK] CLI --json ambiguous get_calls reports code=AMBIGUOUS, exit_code=3, process exit=3."
    );
}
