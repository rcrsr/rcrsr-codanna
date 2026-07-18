//! MCP direct tool invocation command.

use crate::Symbol;
use crate::config::Settings;
use crate::indexing::facade::IndexFacade;
use crate::io::args::parse_positional_args;
use crate::io::envelope::EntityType;
use crate::mcp::service::{RelationOutcome, SearchOutcome, ambiguous_envelope, filter_callers};

/// Print the shared `Ambiguous`-status envelope for an ambiguous symbol name
/// and exit with its `exit_code`. Delegates to `service::ambiguous_envelope`
/// — the single source of the status/code/exit_code mapping — so an
/// identical ambiguous-name request produces the same status/code/exit_code
/// whether it comes through `codanna mcp <tool> --json` or the MCP tool's
/// own `output_format: json` path (previously this CLI path hardcoded
/// `ResultCode::InvalidQuery`/exit 2 while the MCP handlers used
/// `Status::Ambiguous`/`ResultCode::Ambiguous`/exit 3, drifting apart).
fn exit_ambiguous(entity: EntityType, name: &str, candidates: Vec<Symbol>) -> ! {
    let envelope = ambiguous_envelope(entity, name, candidates);
    let exit_code = envelope.exit_code;
    println!("{}", envelope.to_json().expect("envelope serialization"));
    std::process::exit(exit_code.into());
}

/// Print an INDEX_ERROR envelope and exit 2. A backend failure must be
/// distinguishable from a legitimate zero-match result.
fn exit_index_error(entity: EntityType, query: &str, error: impl std::fmt::Display) -> ! {
    use crate::io::envelope::{Envelope, ResultCode};
    let envelope: Envelope<()> = Envelope::error(
        ResultCode::IndexError,
        format!("Index query failed: {error}"),
    )
    .with_entity_type(entity)
    .with_query(query);
    println!("{}", envelope.to_json().expect("envelope serialization"));
    std::process::exit(2);
}

/// Parse a JSON argument value that may be a bare string or an array of
/// strings into a flat `Vec<String>`, so `codanna mcp search_documents`
/// accepts both `collection:docs` (single string) and a JSON array for
/// multi-select. Missing or non-matching values yield an empty vec.
fn parse_string_or_array(value: Option<&serde_json::Value>) -> Vec<String> {
    let raw: Vec<String> = match value {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    // Empty/whitespace-only tokens (e.g. `collection: ""`) must not count as
    // an explicit collection selection, or they silently bypass
    // `DocumentsConfig::default_visibility_exclusions`'s emptiness check.
    raw.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

// MCP tool JSON output structures.
//
// The typed data payloads themselves (IndexInfo, CallRelation,
// SearchSymbolResult, SemanticSearchResult, SemanticSearchWithContextResult,
// ReindexInfo) live in `crate::mcp::service` and are imported above: they
// are shared with the MCP tools' `output_format: json` path, so this file
// must not re-declare them (§BASIC.2).

/// Run the MCP direct tool invocation command.
pub async fn run(
    tool: String,
    positional: Vec<String>,
    args: Option<String>,
    json: bool,
    fields: Option<Vec<String>>,
    facade: IndexFacade,
    config: &Settings,
) {
    // Build arguments from both positional and --args
    let mut arguments = if let Some(args_str) = &args {
        // Parse JSON arguments if provided (backward compatibility)
        match serde_json::from_str::<serde_json::Value>(args_str) {
            Ok(serde_json::Value::Object(map)) => Some(map),
            Ok(_) => {
                eprintln!("Error: Arguments must be a JSON object");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error parsing arguments: {e}");
                std::process::exit(1);
            }
        }
    } else {
        // Start with empty map if no --args
        Some(serde_json::Map::new())
    };

    // Process positional arguments using unified parser
    if !positional.is_empty() {
        if let Some(ref mut args_map) = arguments {
            // Use the unified parser from args.rs
            let (first_positional, params) = parse_positional_args(&positional);

            // Handle the first positional argument based on tool type
            if let Some(pos_arg) = first_positional {
                match tool.as_str() {
                    "find_symbol" => {
                        args_map.insert(
                            "name".to_string(),
                            serde_json::Value::String(pos_arg.clone()),
                        );
                    }
                    "get_calls" | "find_callers" | "analyze_impact" | "read_symbol" => {
                        // Canonical key is `name` (old `function_name` /
                        // `symbol_name` keys are still accepted via serde
                        // aliases when passed explicitly through --args).
                        args_map.insert(
                            "name".to_string(),
                            serde_json::Value::String(pos_arg.clone()),
                        );
                    }
                    "get_file_outline" => {
                        args_map.insert(
                            "path".to_string(),
                            serde_json::Value::String(pos_arg.clone()),
                        );
                    }
                    "semantic_search_docs"
                    | "semantic_search_with_context"
                    | "search_documents" => {
                        args_map.insert(
                            "query".to_string(),
                            serde_json::Value::String(pos_arg.clone()),
                        );
                    }
                    "search_symbols" => {
                        args_map.insert(
                            "query".to_string(),
                            serde_json::Value::String(pos_arg.clone()),
                        );
                    }
                    _ => {
                        eprintln!("Warning: Unknown tool '{tool}', ignoring positional argument");
                    }
                }
            }

            // Special handling: find_symbol supports symbol_id:XXX as positional
            // If symbol_id is in params but name wasn't set, use it as the name
            if tool == "find_symbol" && !args_map.contains_key("name") {
                if let Some(id) = params.get("symbol_id") {
                    args_map.insert(
                        "name".to_string(),
                        serde_json::Value::String(format!("symbol_id:{id}")),
                    );
                }
            }

            // Add all key:value pairs from params
            for (key, value) in params {
                // Try to parse as number first, then boolean, fallback to string
                let json_value = if let Ok(n) = value.parse::<i64>() {
                    serde_json::Value::Number(n.into())
                } else if let Ok(f) = value.parse::<f64>() {
                    serde_json::Value::Number(
                        serde_json::Number::from_f64(f)
                            .unwrap_or_else(|| serde_json::Number::from(0)),
                    )
                } else if let Ok(b) = value.parse::<bool>() {
                    serde_json::Value::Bool(b)
                } else {
                    serde_json::Value::String(value)
                };
                args_map.insert(key, json_value);
            }
        }
    }

    // Convert to Option<Map> only if we have arguments
    let arguments = arguments.filter(|map| !map.is_empty());

    // Validate the tool name up front: JSON mode never reaches the dispatch
    // match below, so its unknown-tool arm cannot cover this.
    //
    // Every tool below (including `reindex`) also accepts an `output_format`
    // argument (`text`, the default, or `json`) when invoked directly over
    // MCP JSON-RPC (e.g. `--args '{"output_format":"json"}'`): `json`
    // returns a single `Envelope<T>`-shaped `ContentBlock::text`, the same
    // schema this CLI's own `--json` flag produces. This CLI's `--json`
    // flag is the primary and independently-maintained CLI JSON path and
    // takes precedence over any `output_format` passed via `--args`.
    const KNOWN_TOOLS: &[&str] = &[
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
    ];
    if !KNOWN_TOOLS.contains(&tool.as_str()) {
        if json {
            use crate::io::exit_code::ExitCode;
            use crate::io::format::JsonResponse;
            let response = JsonResponse::error(
                ExitCode::GeneralError,
                &format!("Unknown tool: {tool}"),
                vec![
                    "Available tools: find_symbol, find_symbols, get_calls, find_callers, analyze_impact, get_index_info, search_symbols, semantic_search_docs, semantic_search_with_context, search_documents, reindex, get_file_outline, read_symbol",
                ],
            );
            println!("{}", serde_json::to_string_pretty(&response).unwrap());
        } else {
            eprintln!("Unknown tool: {tool}");
            eprintln!(
                "Available tools: find_symbol, find_symbols, get_calls, find_callers, analyze_impact, get_index_info, search_symbols, semantic_search_docs, semantic_search_with_context, search_documents, reindex, get_file_outline, read_symbol"
            );
        }
        std::process::exit(1);
    }

    // Collect data for find_symbol if JSON output is requested
    let find_symbol_data = if json && tool == "find_symbol" {
        let name = arguments
            .as_ref()
            .and_then(|m| m.get("name"))
            .and_then(|v| v.as_str());
        let language = arguments
            .as_ref()
            .and_then(|m| m.get("lang"))
            .and_then(|v| v.as_str());

        name.map(|symbol_name| {
            crate::mcp::service::find_symbol_data(&facade, symbol_name, language)
        })
    } else {
        None
    };

    // Collect data for find_symbols if JSON output is requested. Reuses the
    // batch tool's own `find_symbols_entry` builder and cap constant
    // (`crate::mcp::tools::symbols`) rather than re-deriving per-name
    // resolution or the size limit here — mirrors `find_symbol_data` above.
    let find_symbols_data: Option<
        std::collections::BTreeMap<String, crate::mcp::tools::symbols::FindSymbolsEntry>,
    > = if json && tool == "find_symbols" {
        let names: Vec<String> = match arguments.as_ref().and_then(|m| m.get("names")) {
            Some(value) => serde_json::from_value(value.clone()).unwrap_or_else(|e| {
                eprintln!("Error: `names` must be an array of strings: {e}");
                std::process::exit(1);
            }),
            None => {
                eprintln!("Error: find_symbols requires a 'names' array parameter");
                std::process::exit(1);
            }
        };
        if names.len() > crate::mcp::tools::symbols::MAX_FIND_SYMBOLS_NAMES {
            use crate::io::envelope::{Envelope, ResultCode};
            let envelope: Envelope<()> = Envelope::error(
                ResultCode::InvalidQuery,
                format!(
                    "Too many names requested for find_symbols: {} (max {})",
                    names.len(),
                    crate::mcp::tools::symbols::MAX_FIND_SYMBOLS_NAMES
                ),
            );
            println!("{}", envelope.to_json().expect("envelope serialization"));
            std::process::exit(2);
        }
        let language = arguments
            .as_ref()
            .and_then(|m| m.get("lang"))
            .and_then(|v| v.as_str());

        Some(
            names
                .iter()
                .map(|name| {
                    let entry =
                        crate::mcp::tools::symbols::find_symbols_entry(&facade, name, language);
                    (name.clone(), entry)
                })
                .collect(),
        )
    } else {
        None
    };

    // Collect data for get_calls if JSON output is requested.
    // Resolution goes through the shared service layer: ambiguous names
    // refuse-and-list (exit 2) exactly like the MCP handler, never aggregate.
    let get_calls_data = if json && tool == "get_calls" {
        let symbol_id = arguments
            .as_ref()
            .and_then(|m| m.get("symbol_id"))
            .and_then(|v| v.as_u64())
            .map(|id| id as u32);
        let name = arguments
            .as_ref()
            .and_then(|m| m.get("name").or_else(|| m.get("function_name")))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match crate::mcp::service::get_calls_data(&facade, symbol_id, name) {
            RelationOutcome::Data(calls) => Some(calls),
            RelationOutcome::NotFound => None,
            RelationOutcome::Ambiguous { name, candidates } => {
                exit_ambiguous(EntityType::Calls, &name, candidates)
            }
            RelationOutcome::MissingParam => {
                eprintln!("Error: get_calls requires either 'name' or 'symbol_id' parameter");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // `filter`/`count_only` for find_callers: read once here so both the
    // JSON data-collection block below and the JSON render block later in
    // this function (`if json && tool == "find_callers"`) see identical
    // values; the text-mode dispatch further down parses its own copy from
    // `arguments` into a `FindCallersRequest`.
    let find_callers_filter = arguments
        .as_ref()
        .and_then(|m| m.get("filter"))
        .and_then(|v| v.as_str())
        .map(|s| match s {
            "production" => CallerFilter::Production,
            "test" => CallerFilter::Test,
            _ => CallerFilter::All,
        })
        .unwrap_or_default();
    let find_callers_count_only = arguments
        .as_ref()
        .and_then(|m| m.get("count_only"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Collect data for find_callers if JSON output is requested.
    // Same shared resolution policy as get_calls: refuse-and-list on
    // ambiguity, never merge callers of unrelated same-named symbols.
    let find_callers_data = if json && tool == "find_callers" {
        let symbol_id = arguments
            .as_ref()
            .and_then(|m| m.get("symbol_id"))
            .and_then(|v| v.as_u64())
            .map(|id| id as u32);
        let name = arguments
            .as_ref()
            .and_then(|m| m.get("name").or_else(|| m.get("function_name")))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Unfiltered: `filter` is applied at render time so a `count_only`
        // request's per-role breakdown can still be built over the
        // UNFILTERED caller set (see `service::find_callers_counts_envelope`).
        match crate::mcp::service::find_callers_data(
            &facade,
            symbol_id,
            name,
            &config.caller_classification.test_path_patterns,
        ) {
            RelationOutcome::Data(callers) => Some(callers),
            RelationOutcome::NotFound => None,
            RelationOutcome::Ambiguous { name, candidates } => {
                exit_ambiguous(EntityType::Callers, &name, candidates)
            }
            RelationOutcome::MissingParam => {
                eprintln!("Error: find_callers requires either 'name' or 'symbol_id' parameter");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // `count_only`/`max_results`/`group_by` for analyze_impact: read once
    // here so both the JSON render block below (`if json && tool ==
    // "analyze_impact"`) and the text-mode dispatch further down see
    // identical values; the text-mode dispatch parses its own copy from
    // `arguments` into an `AnalyzeImpactRequest`.
    let analyze_impact_count_only = arguments
        .as_ref()
        .and_then(|m| m.get("count_only"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let analyze_impact_max_results = arguments
        .as_ref()
        .and_then(|m| m.get("max_results"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let analyze_impact_group_by = arguments
        .as_ref()
        .and_then(|m| m.get("group_by"))
        .and_then(|v| v.as_str())
        .map(|s| match s {
            "file" => GroupBy::File,
            _ => GroupBy::Kind,
        })
        .unwrap_or_default();

    // Collect data for analyze_impact if JSON output is requested
    let analyze_impact_data = if json && tool == "analyze_impact" {
        let symbol_id = arguments
            .as_ref()
            .and_then(|m| m.get("symbol_id"))
            .and_then(|v| v.as_u64())
            .map(|id| id as u32);
        let name = arguments
            .as_ref()
            .and_then(|m| m.get("name").or_else(|| m.get("symbol_name")))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let max_depth = arguments
            .as_ref()
            .and_then(|m| m.get("max_depth"))
            .and_then(|v| v.as_u64())
            .unwrap_or(3) as usize;

        match crate::mcp::service::analyze_impact_data(&facade, symbol_id, name, max_depth) {
            RelationOutcome::Data(impacted) => Some(impacted),
            RelationOutcome::NotFound => None,
            RelationOutcome::Ambiguous { name, candidates } => {
                exit_ambiguous(EntityType::ImpactGraph, &name, candidates)
            }
            RelationOutcome::MissingParam => {
                eprintln!("Error: analyze_impact requires either 'name' or 'symbol_id' parameter");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // Collect data for search_symbols if JSON output is requested
    let search_symbols_data = if json && tool == "search_symbols" {
        let query = arguments
            .as_ref()
            .and_then(|m| m.get("query"))
            .and_then(|v| v.as_str());

        if let Some(q) = query {
            let limit = arguments
                .as_ref()
                .and_then(|m| m.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(10) as usize;
            let kind = arguments
                .as_ref()
                .and_then(|m| m.get("kind"))
                .and_then(|v| v.as_str());
            let module = arguments
                .as_ref()
                .and_then(|m| m.get("module"))
                .and_then(|v| v.as_str());
            let language = arguments
                .as_ref()
                .and_then(|m| m.get("lang"))
                .and_then(|v| v.as_str());

            match crate::mcp::service::search_symbols_data(
                &facade, q, limit, kind, module, language,
            ) {
                SearchOutcome::Data(results) => Some(results),
                SearchOutcome::InvalidQuery(msg) => {
                    use crate::io::envelope::{Envelope, ResultCode};
                    let envelope: Envelope<()> = Envelope::error(ResultCode::InvalidQuery, msg)
                        .with_entity_type(EntityType::SearchResult)
                        .with_query(q);
                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(2);
                }
                SearchOutcome::Error(msg) => exit_index_error(EntityType::SearchResult, q, msg),
            }
        } else {
            None
        }
    } else {
        None
    };

    let semantic_search_docs_data = if json && tool == "semantic_search_docs" {
        let query = arguments
            .as_ref()
            .and_then(|m| m.get("query"))
            .and_then(|v| v.as_str());

        if let Some(q) = query {
            let limit = arguments
                .as_ref()
                .and_then(|m| m.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(10) as usize;
            let threshold = arguments
                .as_ref()
                .and_then(|m| m.get("threshold"))
                .and_then(|v| v.as_f64())
                .map(|t| t as f32);
            let language = arguments
                .as_ref()
                .and_then(|m| m.get("lang"))
                .and_then(|v| v.as_str());

            match crate::mcp::service::semantic_search_docs_data(
                &facade, q, limit, threshold, language,
            ) {
                SearchOutcome::Data(results) => Some(results),
                // "Semantic search is not enabled" is treated as "no data"
                // here (matching the pre-refactor behavior), not a hard
                // error; the emission arm below re-checks
                // `has_semantic_search` for the distinct envelope.
                SearchOutcome::InvalidQuery(_) => None,
                SearchOutcome::Error(msg) => {
                    if facade.has_semantic_search() {
                        exit_index_error(EntityType::SearchResult, q, msg)
                    } else {
                        None
                    }
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // Collect data for semantic_search_with_context if JSON output is requested
    let semantic_search_with_context_data = if json && tool == "semantic_search_with_context" {
        let query = arguments
            .as_ref()
            .and_then(|m| m.get("query"))
            .and_then(|v| v.as_str());

        if let Some(q) = query {
            let limit = arguments
                .as_ref()
                .and_then(|m| m.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(5) as usize; // Default 5 for context version
            let threshold = arguments
                .as_ref()
                .and_then(|m| m.get("threshold"))
                .and_then(|v| v.as_f64())
                .map(|t| t as f32);
            let language = arguments
                .as_ref()
                .and_then(|m| m.get("lang"))
                .and_then(|v| v.as_str());

            match crate::mcp::service::semantic_search_with_context_data(
                &facade, q, limit, threshold, language,
            ) {
                SearchOutcome::Data(results) => Some(results),
                SearchOutcome::InvalidQuery(_) => None,
                SearchOutcome::Error(msg) => {
                    if facade.has_semantic_search() {
                        exit_index_error(EntityType::SearchResult, q, msg)
                    } else {
                        None
                    }
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // Check semantic search status before moving indexer
    let has_semantic_search = facade.has_semantic_search();

    // Only load document store for tools that need it (search_documents, or
    // reindex when it was requested with documents:true). This is expensive
    // (~1s to load ML model) so we skip it for other tools.
    let needs_document_store = tool == "search_documents"
        || (tool == "reindex"
            && arguments
                .as_ref()
                .and_then(|m| m.get("documents"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false));
    let document_store = if needs_document_store {
        crate::documents::load_from_settings(config)
    } else {
        None
    };

    // get_index_info needs no pre-collection: `service::index_info_envelope`
    // is built directly at render time from the read guard acquired there.

    // Pre-collect search_documents data for JSON output
    let search_documents_data = if json && tool == "search_documents" {
        if let Some(ref store_arc) = document_store {
            let query = arguments
                .as_ref()
                .and_then(|m| m.get("query"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let collections =
                parse_string_or_array(arguments.as_ref().and_then(|m| m.get("collection")));
            let mut exclude_collections = parse_string_or_array(
                arguments
                    .as_ref()
                    .and_then(|m| m.get("exclude_collections")),
            );
            let limit = arguments
                .as_ref()
                .and_then(|m| m.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(5) as usize;

            // Both the auto-sync loop and the search below read collection
            // definitions from this single `Arc<Settings>` (rather than the
            // separately-passed `config` parameter), so the two steps can
            // never observe different collection definitions even if
            // `facade` and `config` diverge in the future.
            let settings = std::sync::Arc::clone(facade.settings());

            // When the caller named no collections, merge in every collection
            // whose `default` flag opts it out of unscoped search (mirrors
            // the MCP tool call site in mcp/tools/search.rs). Explicitly
            // named collections are always searched regardless of the flag.
            exclude_collections.extend(
                settings
                    .documents
                    .default_visibility_exclusions(&collections),
            );

            // Auto-sync: brief write guard scoped to the collection scan
            // only, dropped before searching (mirrors the MCP tool call
            // sites in mcp/tools/search.rs). `index_collection` performs
            // blocking file I/O, tantivy commits, and embedding generation,
            // so the owned write guard is moved into `spawn_blocking`
            // (mirroring `reindex_locked` in `indexing/facade.rs`) rather
            // than doing that work directly on the async worker while the
            // write lock is held.
            {
                for (name, coll_config) in &settings.documents.collections {
                    let owned_guard = std::sync::Arc::clone(store_arc).write_owned().await;
                    let coll_config = coll_config.clone();
                    let defaults = settings.documents.defaults.clone();
                    let name_owned = name.clone();
                    let join_result = tokio::task::spawn_blocking(move || {
                        let mut store = owned_guard;
                        store.index_collection(&name_owned, &coll_config, &defaults)
                    })
                    .await;

                    match join_result {
                        Ok(Err(e)) => {
                            tracing::warn!(target: "rag", "auto-sync failed for collection '{}': {}", name, e);
                        }
                        Err(e) => {
                            tracing::warn!(target: "rag", "auto-sync failed for collection '{}': {}", name, crate::utils::describe_join_error(&e));
                        }
                        Ok(Ok(_)) => {}
                    }
                }
            }

            // `search_documents_data` embeds the query text (an ONNX
            // forward pass through `generate_embeddings`) and scores every
            // candidate vector against it, so — like the auto-sync loop
            // above — it must not run directly on the async worker. The
            // owned read guard is moved into `spawn_blocking` (mirroring
            // `reindex_locked` in `indexing/facade.rs`). `settings` is the
            // same `Arc<Settings>` used by the auto-sync loop above, so
            // search always observes the collection definitions it just
            // synced against.
            let owned_guard = std::sync::Arc::clone(store_arc).read_owned().await;
            let settings = std::sync::Arc::clone(&settings);
            let query_owned = query.clone();
            let join_result = tokio::task::spawn_blocking(move || {
                let store = owned_guard;
                crate::mcp::service::search_documents_data(
                    &store,
                    &settings,
                    &query_owned,
                    collections,
                    exclude_collections,
                    limit,
                )
            })
            .await;

            match join_result {
                Ok(Ok(results)) => Some((query, results)),
                Ok(Err(e)) => exit_index_error(EntityType::Document, &query, e),
                Err(e) => exit_index_error(
                    EntityType::Document,
                    &query,
                    format!(
                        "{}. Retry 'codanna mcp search_documents'.",
                        crate::utils::describe_join_error(&e)
                    ),
                ),
            }
        } else {
            None
        }
    } else {
        None
    };

    // Embedded mode - use already loaded facade directly
    let server = {
        let server = crate::mcp::CodeIntelligenceServer::new(facade);

        // Add DocumentStore if documents are enabled and indexed
        if let Some(store_arc) = document_store {
            server.with_document_store_arc(store_arc)
        } else {
            server
        }
    };

    // Invoke reindex now for JSON output. reindex mutates the index, and
    // json mode's dispatch match below is a text-only no-op (see below), so
    // this cannot wait for the shared dispatch path used by read-only tools.
    let reindex_data = if json && tool == "reindex" {
        let (paths, force, documents) =
            match crate::mcp::requests::ReindexRequest::parse_args(arguments.as_ref()) {
                Ok(v) => v,
                Err(e) => {
                    use crate::io::envelope::{Envelope, ResultCode};
                    let envelope: Envelope<()> = Envelope::error(
                        ResultCode::InvalidQuery,
                        format!("Invalid reindex arguments: {e}"),
                    );
                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(2);
                }
            };

        match server.run_reindex(paths, force, documents).await {
            Ok(outcome) => Some(outcome),
            Err(e) => {
                use crate::io::envelope::{Envelope, ResultCode};
                let envelope: Envelope<()> =
                    Envelope::error(ResultCode::IndexError, format!("Reindex failed: {e}"));
                println!("{}", envelope.to_json().expect("envelope serialization"));
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    // Call the tool directly
    use crate::mcp::*;
    use rmcp::handler::server::wrapper::Parameters;

    // JSON mode already collected everything above through the shared
    // service layer — one execution per invocation. The JSON emit arms
    // below use only pre-collected data; handler dispatch is text-only.
    //
    // For "reindex" specifically: `reindex_data` above (the `json && tool ==
    // "reindex"` branch) is the sole execution of `run_reindex` on the JSON
    // path. That is only safe because the `"reindex" =>` arm in the `match`
    // below — which calls `server.reindex()` and would run a second
    // reindex — is gated behind this `if json { .. } else { match .. } }`
    // and never reached when `json` is true. If this `if json` gate is ever
    // removed or the "reindex" match arm is moved outside it, `reindex_data`
    // must be updated in lockstep or a JSON-mode reindex will run twice.
    let result = if json {
        Ok(rmcp::model::CallToolResult::success(vec![]))
    } else {
        match tool.as_str() {
            "find_symbol" => {
                let name = arguments
                    .as_ref()
                    .and_then(|m| m.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        eprintln!("Error: find_symbol requires 'name' parameter");
                        std::process::exit(1);
                    });
                let lang = arguments
                    .as_ref()
                    .and_then(|m| m.get("lang"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let symbol_id = arguments
                    .as_ref()
                    .and_then(|m| m.get("symbol_id"))
                    .and_then(|v| v.as_u64())
                    .map(|id| id as u32);
                server
                    .find_symbol(Parameters(FindSymbolRequest {
                        name: name.to_string(),
                        symbol_id,
                        lang,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "find_symbols" => {
                // Strict parsing, mirroring `ReindexRequest::parse_args`: a
                // present-but-malformed `names` array (e.g. a non-string
                // element) is a hard error rather than silently dropping the
                // offending entries, which would silently narrow the batch.
                let names: Vec<String> = match arguments.as_ref().and_then(|m| m.get("names")) {
                    Some(value) => serde_json::from_value(value.clone()).unwrap_or_else(|e| {
                        eprintln!("Error: `names` must be an array of strings: {e}");
                        std::process::exit(1);
                    }),
                    None => {
                        eprintln!("Error: find_symbols requires a 'names' array parameter");
                        std::process::exit(1);
                    }
                };
                let lang = arguments
                    .as_ref()
                    .and_then(|m| m.get("lang"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                server
                    .find_symbols(Parameters(FindSymbolsRequest {
                        names,
                        lang,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "get_calls" => {
                let name = arguments
                    .as_ref()
                    .and_then(|m| m.get("name").or_else(|| m.get("function_name")))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let symbol_id = arguments
                    .as_ref()
                    .and_then(|m| m.get("symbol_id"))
                    .and_then(|v| v.as_u64())
                    .map(|id| id as u32);

                // Require either name or symbol_id
                if name.is_none() && symbol_id.is_none() {
                    eprintln!("Error: get_calls requires either 'name' or 'symbol_id' parameter");
                    std::process::exit(1);
                }

                server
                    .get_calls(Parameters(GetCallsRequest {
                        name,
                        symbol_id,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "find_callers" => {
                let name = arguments
                    .as_ref()
                    .and_then(|m| m.get("name").or_else(|| m.get("function_name")))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let symbol_id = arguments
                    .as_ref()
                    .and_then(|m| m.get("symbol_id"))
                    .and_then(|v| v.as_u64())
                    .map(|id| id as u32);

                // Require either name or symbol_id
                if name.is_none() && symbol_id.is_none() {
                    eprintln!(
                        "Error: find_callers requires either 'name' or 'symbol_id' parameter"
                    );
                    std::process::exit(1);
                }

                server
                    .find_callers(Parameters(FindCallersRequest {
                        name,
                        symbol_id,
                        filter: find_callers_filter,
                        count_only: find_callers_count_only,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "analyze_impact" => {
                let name = arguments
                    .as_ref()
                    .and_then(|m| m.get("name").or_else(|| m.get("symbol_name")))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let symbol_id = arguments
                    .as_ref()
                    .and_then(|m| m.get("symbol_id"))
                    .and_then(|v| v.as_u64())
                    .map(|id| id as u32);

                // Require either name or symbol_id
                if name.is_none() && symbol_id.is_none() {
                    eprintln!(
                        "Error: analyze_impact requires either 'name' or 'symbol_id' parameter"
                    );
                    std::process::exit(1);
                }

                let max_depth = arguments
                    .as_ref()
                    .and_then(|m| m.get("max_depth"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3) as u32;
                server
                    .analyze_impact(Parameters(AnalyzeImpactRequest {
                        name,
                        symbol_id,
                        max_depth,
                        count_only: analyze_impact_count_only,
                        max_results: analyze_impact_max_results,
                        group_by: analyze_impact_group_by,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "get_index_info" => {
                use crate::mcp::GetIndexInfoRequest;
                use rmcp::handler::server::wrapper::Parameters;
                server
                    .get_index_info(Parameters(GetIndexInfoRequest {
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "search_symbols" => {
                let query = arguments
                    .as_ref()
                    .and_then(|m| m.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        eprintln!("Error: search_symbols requires 'query' parameter");
                        std::process::exit(1);
                    });
                let limit = arguments
                    .as_ref()
                    .and_then(|m| m.get("limit"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10) as u32;
                let kind = arguments
                    .as_ref()
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let module = arguments
                    .as_ref()
                    .and_then(|m| m.get("module"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let lang = arguments
                    .as_ref()
                    .and_then(|m| m.get("lang"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                server
                    .search_symbols(Parameters(SearchSymbolsRequest {
                        query: query.to_string(),
                        limit,
                        kind,
                        module,
                        lang,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "semantic_search_docs" => {
                let query = arguments
                    .as_ref()
                    .and_then(|m| m.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        eprintln!("Error: semantic_search_docs requires 'query' parameter");
                        std::process::exit(1);
                    });
                let limit = arguments
                    .as_ref()
                    .and_then(|m| m.get("limit"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10) as u32;
                let threshold = arguments
                    .as_ref()
                    .and_then(|m| m.get("threshold"))
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32);
                let lang = arguments
                    .as_ref()
                    .and_then(|m| m.get("lang"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                server
                    .semantic_search_docs(Parameters(SemanticSearchRequest {
                        query: query.to_string(),
                        limit,
                        threshold,
                        lang,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "semantic_search_with_context" => {
                let query = arguments
                    .as_ref()
                    .and_then(|m| m.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        eprintln!("Error: semantic_search_with_context requires 'query' parameter");
                        std::process::exit(1);
                    });
                let limit = arguments
                    .as_ref()
                    .and_then(|m| m.get("limit"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5) as u32;
                let threshold = arguments
                    .as_ref()
                    .and_then(|m| m.get("threshold"))
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32);
                let lang = arguments
                    .as_ref()
                    .and_then(|m| m.get("lang"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                server
                    .semantic_search_with_context(Parameters(SemanticSearchWithContextRequest {
                        query: query.to_string(),
                        limit,
                        threshold,
                        lang,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "search_documents" => {
                use crate::mcp::SearchDocumentsRequest;
                let query = arguments
                    .as_ref()
                    .and_then(|m| m.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        eprintln!("Error: search_documents requires 'query' parameter");
                        std::process::exit(1);
                    })
                    .to_string();
                let collection = arguments
                    .as_ref()
                    .and_then(|m| m.get("collection"))
                    .map(|v| crate::mcp::requests::OneOrMany::Many(parse_string_or_array(Some(v))));
                let exclude_collections = arguments
                    .as_ref()
                    .and_then(|m| m.get("exclude_collections"))
                    .map(|v| parse_string_or_array(Some(v)));
                let limit = arguments
                    .as_ref()
                    .and_then(|m| m.get("limit"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5) as u32;
                server
                    .search_documents(Parameters(SearchDocumentsRequest {
                        query,
                        collection,
                        exclude_collections,
                        limit,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "reindex" => {
                let (paths, force, documents) =
                    crate::mcp::requests::ReindexRequest::parse_args(arguments.as_ref())
                        .unwrap_or_else(|e| {
                            eprintln!("Error: invalid reindex arguments: {e}");
                            std::process::exit(1);
                        });
                server
                    .reindex(Parameters(ReindexRequest {
                        paths,
                        force,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                        documents,
                    }))
                    .await
            }
            "get_file_outline" => {
                let path = arguments
                    .as_ref()
                    .and_then(|m| m.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        eprintln!("Error: get_file_outline requires 'path' parameter");
                        std::process::exit(1);
                    });
                let max_results = arguments
                    .as_ref()
                    .and_then(|m| m.get("max_results"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                server
                    .get_file_outline(Parameters(crate::mcp::requests::GetFileOutlineRequest {
                        path: path.to_string(),
                        max_results,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            "read_symbol" => {
                let name = arguments
                    .as_ref()
                    .and_then(|m| m.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let symbol_id = arguments
                    .as_ref()
                    .and_then(|m| m.get("symbol_id"))
                    .and_then(|v| v.as_u64())
                    .map(|id| id as u32);

                if name.is_none() && symbol_id.is_none() {
                    eprintln!("Error: read_symbol requires either 'name' or 'symbol_id' parameter");
                    std::process::exit(1);
                }

                server
                    .read_symbol(Parameters(crate::mcp::requests::ReadSymbolRequest {
                        name,
                        symbol_id,
                        output_format: crate::mcp::requests::OutputFormat::Text,
                    }))
                    .await
            }
            _ => {
                if json {
                    use crate::io::exit_code::ExitCode;
                    use crate::io::format::JsonResponse;
                    let response = JsonResponse::error(
                        ExitCode::GeneralError,
                        &format!("Unknown tool: {tool}"),
                        vec![
                            "Available tools: find_symbol, find_symbols, get_calls, find_callers, analyze_impact, get_index_info, search_symbols, semantic_search_docs, semantic_search_with_context, search_documents, reindex, get_file_outline, read_symbol",
                        ],
                    );
                    println!("{}", serde_json::to_string_pretty(&response).unwrap());
                } else {
                    eprintln!("Unknown tool: {tool}");
                    eprintln!(
                        "Available tools: find_symbol, find_symbols, get_calls, find_callers, analyze_impact, get_index_info, search_symbols, semantic_search_docs, semantic_search_with_context, search_documents, reindex, get_file_outline, read_symbol"
                    );
                }
                std::process::exit(1);
            }
        }
    };

    // Read guard used only for building JSON envelopes below via the
    // shared `crate::mcp::service` builders (§BASIC.2): acquired here,
    // after the mutating `reindex_data` collection above has already
    // released its own write lock, so this never contends with it.
    let indexer = server.facade.read().await;

    // Print result
    match result {
        Ok(call_result) => {
            if json && tool == "get_index_info" {
                let envelope = crate::mcp::service::index_info_envelope(&indexer);
                let output = match &fields {
                    Some(f) => envelope.to_json_with_fields(f),
                    None => envelope.to_json(),
                };
                println!("{}", output.expect("envelope serialization"));
            } else if json && tool == "find_symbol" {
                // Use pre-collected data for JSON output
                if let Some(symbol_contexts) = find_symbol_data {
                    let name = arguments
                        .as_ref()
                        .and_then(|m| m.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let language = arguments
                        .as_ref()
                        .and_then(|m| m.get("lang"))
                        .and_then(|v| v.as_str());

                    let is_not_found = symbol_contexts.is_empty();
                    let envelope = crate::mcp::service::find_symbol_envelope(
                        &indexer,
                        name,
                        language,
                        symbol_contexts,
                    );
                    if is_not_found {
                        // Envelope serialization is infallible for simple types
                        println!("{}", envelope.to_json().expect("envelope serialization"));
                        std::process::exit(3);
                    }
                    let output = match &fields {
                        Some(f) => envelope.to_json_with_fields(f),
                        None => envelope.to_json(),
                    };
                    println!("{}", output.expect("envelope serialization"));
                }
            } else if json && tool == "find_symbols" {
                // Use pre-collected data for JSON output — mirrors find_symbol
                // above, but the envelope wraps a per-name map rather than a
                // single symbol's candidate list.
                if let Some(results) = find_symbols_data {
                    let language = arguments
                        .as_ref()
                        .and_then(|m| m.get("lang"))
                        .and_then(|v| v.as_str());

                    let envelope =
                        crate::mcp::service::find_symbols_envelope(&indexer, results, language);
                    let output = match &fields {
                        Some(f) => envelope.to_json_with_fields(f),
                        None => envelope.to_json(),
                    };
                    println!("{}", output.expect("envelope serialization"));
                }
            } else if json && tool == "get_calls" {
                let identifier = if let Some(id) = arguments
                    .as_ref()
                    .and_then(|m| m.get("symbol_id"))
                    .and_then(|v| v.as_u64())
                {
                    format!("symbol_id:{id}")
                } else {
                    arguments
                        .as_ref()
                        .and_then(|m| m.get("name").or_else(|| m.get("function_name")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string()
                };

                if let Some(calls) = get_calls_data {
                    let envelope = crate::mcp::service::get_calls_success_envelope(
                        &indexer,
                        &identifier,
                        calls,
                    );
                    let output = match &fields {
                        Some(f) => envelope.to_json_with_fields(f),
                        None => envelope.to_json(),
                    };
                    println!("{}", output.expect("envelope serialization"));
                } else {
                    let envelope =
                        crate::mcp::service::get_calls_not_found_envelope(&indexer, &identifier);
                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(3);
                }
            } else if json && tool == "find_callers" {
                let identifier = if let Some(id) = arguments
                    .as_ref()
                    .and_then(|m| m.get("symbol_id"))
                    .and_then(|v| v.as_u64())
                {
                    format!("symbol_id:{id}")
                } else {
                    arguments
                        .as_ref()
                        .and_then(|m| m.get("name").or_else(|| m.get("function_name")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string()
                };

                if let Some(unfiltered) = find_callers_data {
                    // Per-role breakdown is always computed over the
                    // UNFILTERED caller set — `filter` narrows the returned
                    // listing, never the counted breakdown (see
                    // `service::find_callers_counts_envelope`).
                    let output = if find_callers_count_only {
                        let envelope = crate::mcp::service::find_callers_counts_envelope(
                            &indexer,
                            &identifier,
                            &unfiltered,
                        );
                        match &fields {
                            Some(f) => envelope.to_json_with_fields(f),
                            None => envelope.to_json(),
                        }
                    } else {
                        let filtered = filter_callers(unfiltered, find_callers_filter);
                        let envelope = crate::mcp::service::find_callers_list_envelope(
                            &indexer,
                            &identifier,
                            filtered,
                        );
                        match &fields {
                            Some(f) => envelope.to_json_with_fields(f),
                            None => envelope.to_json(),
                        }
                    };
                    println!("{}", output.expect("envelope serialization"));
                } else {
                    let envelope =
                        crate::mcp::service::find_callers_not_found_envelope(&indexer, &identifier);
                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(3);
                }
            } else if json && tool == "analyze_impact" {
                // Get identifier for messages
                let identifier = if let Some(id) = arguments
                    .as_ref()
                    .and_then(|m| m.get("symbol_id"))
                    .and_then(|v| v.as_u64())
                {
                    format!("symbol_id:{id}")
                } else {
                    arguments
                        .as_ref()
                        .and_then(|m| m.get("name").or_else(|| m.get("symbol_name")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string()
                };

                let max_depth = arguments
                    .as_ref()
                    .and_then(|m| m.get("max_depth"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3) as u32;

                if let Some(impacted) = analyze_impact_data {
                    let output = if analyze_impact_count_only {
                        let envelope = crate::mcp::service::analyze_impact_counts_envelope(
                            &indexer,
                            &identifier,
                            max_depth,
                            &impacted,
                        );
                        match &fields {
                            Some(f) => envelope.to_json_with_fields(f),
                            None => envelope.to_json(),
                        }
                    } else {
                        let envelope = crate::mcp::service::analyze_impact_listing_envelope(
                            &indexer,
                            &identifier,
                            max_depth,
                            impacted,
                            analyze_impact_group_by,
                            analyze_impact_max_results,
                        );
                        match &fields {
                            Some(f) => envelope.to_json_with_fields(f),
                            None => envelope.to_json(),
                        }
                    };
                    println!("{}", output.expect("envelope serialization"));
                } else {
                    // Symbol not found
                    let envelope = crate::mcp::service::analyze_impact_not_found_envelope(
                        &indexer,
                        &identifier,
                    );
                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(3);
                }
            } else if json && tool == "search_symbols" {
                use crate::io::envelope::{EntityType, Envelope, ResultCode};

                let query = arguments
                    .as_ref()
                    .and_then(|m| m.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let language = arguments
                    .as_ref()
                    .and_then(|m| m.get("lang"))
                    .and_then(|v| v.as_str());

                if let Some(results) = search_symbols_data {
                    // `crate::mcp::service::search_symbols_data` already
                    // returns the nested-symbol shape.
                    let envelope = crate::mcp::service::search_symbols_envelope(
                        &indexer, query, language, results,
                    );
                    let output = match &fields {
                        Some(f) => envelope.to_json_with_fields(f),
                        None => envelope.to_json(),
                    };
                    println!("{}", output.expect("envelope serialization"));
                } else {
                    let envelope: Envelope<()> = Envelope::error(
                        ResultCode::InvalidQuery,
                        format!("Failed to search for '{query}'"),
                    )
                    .with_entity_type(EntityType::SearchResult)
                    .with_query(query)
                    .with_hint("Check query syntax");

                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(1);
                }
            } else if json && tool == "semantic_search_docs" {
                use crate::io::envelope::{Envelope, ResultCode};

                let query = arguments
                    .as_ref()
                    .and_then(|m| m.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let language = arguments
                    .as_ref()
                    .and_then(|m| m.get("lang"))
                    .and_then(|v| v.as_str());

                if let Some(results) = semantic_search_docs_data {
                    let envelope = crate::mcp::service::semantic_search_docs_envelope(
                        &indexer, query, language, results,
                    );
                    let output = match &fields {
                        Some(f) => envelope.to_json_with_fields(f),
                        None => envelope.to_json(),
                    };
                    println!("{}", output.expect("envelope serialization"));
                } else if !has_semantic_search {
                    let envelope = crate::mcp::service::semantic_search_error_envelope(
                        query,
                        "Semantic search is not enabled",
                    );
                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(1);
                } else {
                    let envelope: Envelope<()> = Envelope::error(
                        ResultCode::InvalidQuery,
                        format!("Failed to search for '{query}'"),
                    )
                    .with_entity_type(crate::io::envelope::EntityType::Symbol)
                    .with_query(query)
                    .with_hint("Check query syntax");

                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(1);
                }
            } else if json && tool == "semantic_search_with_context" {
                use crate::io::envelope::{Envelope, ResultCode};

                let query = arguments
                    .as_ref()
                    .and_then(|m| m.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let language = arguments
                    .as_ref()
                    .and_then(|m| m.get("lang"))
                    .and_then(|v| v.as_str());

                if let Some(results) = semantic_search_with_context_data {
                    let envelope = crate::mcp::service::semantic_search_with_context_envelope(
                        &indexer, query, language, results,
                    );
                    let output = match &fields {
                        Some(f) => envelope.to_json_with_fields(f),
                        None => envelope.to_json(),
                    };
                    println!("{}", output.expect("envelope serialization"));
                } else if !has_semantic_search {
                    let envelope = crate::mcp::service::semantic_search_error_envelope(
                        query,
                        "Semantic search is not enabled",
                    );
                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(1);
                } else {
                    let envelope: Envelope<()> = Envelope::error(
                        ResultCode::InvalidQuery,
                        format!("Failed to search for '{query}'"),
                    )
                    .with_entity_type(crate::io::envelope::EntityType::Symbol)
                    .with_query(query)
                    .with_hint("Check query syntax");

                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(1);
                }
            } else if json && tool == "search_documents" {
                use crate::io::envelope::{EntityType, Envelope};

                let query = arguments
                    .as_ref()
                    .and_then(|m| m.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                if let Some((query_text, results)) = search_documents_data {
                    let count = results.len();

                    // Convert to serializable format
                    let data: Vec<_> = results
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "chunk_id": r.chunk_id,
                                "collection": r.collection,
                                "source_path": r.source_path,
                                "heading_context": r.heading_context,
                                "content_preview": r.content_preview,
                                "byte_range": r.byte_range,
                                "similarity": r.similarity
                            })
                        })
                        .collect();

                    let envelope = if count == 0 {
                        Envelope::<Vec<serde_json::Value>>::not_found(format!(
                            "No documents found for '{query_text}'"
                        ))
                        .with_entity_type(EntityType::Document)
                        .with_query(&query_text)
                    } else {
                        Envelope::success(data)
                            .with_entity_type(EntityType::Document)
                            .with_count(count)
                            .with_query(&query_text)
                            .with_message(format!("Found {count} matching documents"))
                            .with_hint(
                                "Use the file paths and byte ranges to read specific sections",
                            )
                    };

                    let output = match &fields {
                        Some(f) => envelope.to_json_with_fields(f),
                        None => envelope.to_json(),
                    };
                    println!("{}", output.expect("envelope serialization"));

                    if count == 0 {
                        std::process::exit(1);
                    }
                } else {
                    let envelope: Envelope<()> = Envelope::error(
                        crate::io::envelope::ResultCode::IndexError,
                        "Document search not available",
                    )
                    .with_entity_type(EntityType::Document)
                    .with_query(query)
                    .with_hint("Run 'codanna documents index' to create the index");

                    println!("{}", envelope.to_json().expect("envelope serialization"));
                    std::process::exit(1);
                }
            } else if json && tool == "reindex" {
                if let Some(outcome) = reindex_data {
                    let envelope = crate::mcp::service::reindex_envelope(&outcome);

                    let output = match &fields {
                        Some(f) => envelope.to_json_with_fields(f),
                        None => envelope.to_json(),
                    };
                    println!("{}", output.expect("envelope serialization"));
                }
            } else {
                // Default text output
                for content in &call_result.content {
                    match content {
                        rmcp::model::ContentBlock::Text(text_content) => {
                            println!("{}", text_content.text);
                        }
                        _ => {
                            eprintln!("Warning: Non-text content returned");
                        }
                    }
                }
            }
        }
        Err(e) => {
            if json {
                use crate::io::envelope::{Envelope, ResultCode};
                let envelope: Envelope<()> =
                    Envelope::error(ResultCode::InternalError, e.message.to_string())
                        .with_hint("Check the tool name and arguments");

                println!("{}", envelope.to_json().expect("envelope serialization"));
                std::process::exit(1);
            } else {
                eprintln!("Error calling tool: {}", e.message);
                std::process::exit(1);
            }
        }
    }
}
