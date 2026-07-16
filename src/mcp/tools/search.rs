//! Search and info tools: get_index_info, semantic_search_docs,
//! semantic_search_with_context, search_symbols, search_documents.

use rmcp::model::ErrorData as McpError;
use rmcp::model::*;
use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use crate::documents::SearchQuery as DocSearchQuery;

use crate::io::envelope::{EntityType, Envelope, ResultCode};
use crate::mcp::requests::{
    GetIndexInfoRequest, OutputFormat, SearchDocumentsRequest, SearchSymbolsRequest,
    SemanticSearchRequest, SemanticSearchWithContextRequest,
};
use crate::mcp::server::{CodeIntelligenceServer, format_relative_time, generate_mcp_guidance};
use crate::mcp::service::{self, SearchOutcome, json_result};

#[tool_router(router = search_router, vis = "pub(crate)")]
impl CodeIntelligenceServer {
    #[tool(description = "Get information about the indexed codebase")]
    pub async fn get_index_info(
        &self,
        Parameters(GetIndexInfoRequest { output_format }): Parameters<GetIndexInfoRequest>,
    ) -> Result<CallToolResult, McpError> {
        let indexer = self.facade.read().await;

        if output_format == OutputFormat::Json {
            return Ok(json_result(service::index_info_envelope(&indexer)));
        }
        let symbol_count = indexer.symbol_count();
        let file_count = indexer.file_count();
        let relationship_count = indexer.relationship_count();

        // Efficiently count symbols by kind in one pass
        let mut kind_counts = std::collections::HashMap::new();
        for symbol in indexer.get_all_symbols() {
            *kind_counts.entry(symbol.kind).or_insert(0) += 1;
        }

        // Build symbol kinds display dynamically
        let mut kinds_display = String::new();

        // Sort by kind name for consistent output
        let mut sorted_kinds: Vec<_> = kind_counts.iter().collect();
        sorted_kinds.sort_by_key(|(kind, _)| format!("{kind:?}"));

        for (kind, count) in sorted_kinds {
            kinds_display.push_str(&format!("\n  - {kind:?}s: {count}"));
        }

        // Get semantic search info
        let semantic_info = if let Some(metadata) = indexer.get_semantic_metadata() {
            let live_count = indexer.semantic_search_embedding_count();
            format!(
                "\n\nSemantic Search:\n  - Status: Enabled\n  - Model: {}\n  - Embeddings: {}\n  - Dimensions: {}\n  - Created: {}\n  - Updated: {}",
                metadata.model_name,
                live_count,
                metadata.dimension,
                format_relative_time(metadata.created_at),
                format_relative_time(metadata.updated_at)
            )
        } else {
            "\n\nSemantic Search:\n  - Status: Disabled".to_string()
        };

        let result = format!(
            "Index contains {symbol_count} symbols across {file_count} files.\n\nBreakdown:\n  - Symbols: {symbol_count}\n  - Relationships: {relationship_count}\n\nSymbol Kinds:{kinds_display}{semantic_info}"
        );

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    #[tool(description = "Search documentation using natural language semantic search")]
    pub async fn semantic_search_docs(
        &self,
        Parameters(SemanticSearchRequest {
            query,
            limit,
            threshold,
            lang,
            output_format,
        }): Parameters<SemanticSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let indexer = self.facade.read().await;

        if output_format == OutputFormat::Json {
            return Ok(
                match service::semantic_search_docs_data(
                    &indexer,
                    &query,
                    limit as usize,
                    threshold,
                    lang.as_deref(),
                ) {
                    SearchOutcome::Data(results) => {
                        json_result(service::semantic_search_docs_envelope(
                            &indexer,
                            &query,
                            lang.as_deref(),
                            results,
                        ))
                    }
                    SearchOutcome::InvalidQuery(msg) | SearchOutcome::Error(msg) => {
                        json_result(service::semantic_search_error_envelope(&query, msg))
                    }
                },
            );
        }

        tracing::debug!(
            target: "mcp",
            "semantic_search_docs called - symbols: {}, semantic: {}",
            indexer.symbol_count(),
            indexer.has_semantic_search()
        );

        if !indexer.has_semantic_search() {
            // Check if semantic files exist
            let semantic_path = indexer.settings().index_path.join("semantic");
            let metadata_exists = semantic_path.join("metadata.json").exists();
            let vectors_exist = semantic_path.join("segment_0.vec").exists();
            let symbol_count = indexer.symbol_count();

            // Get current working directory for debugging
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "unknown".to_string());

            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Semantic search is not enabled. The index needs to be rebuilt with semantic search enabled.\n\nDEBUG INFO:\n- Index path: {}\n- Symbol count: {}\n- Semantic files exist: {}\n- Has semantic search: {}\n- Working dir: {}",
                indexer.settings().index_path.display(),
                symbol_count,
                metadata_exists && vectors_exist,
                indexer.has_semantic_search(),
                cwd
            ))]));
        }

        let results = match threshold {
            Some(t) => indexer.semantic_search_docs_with_threshold_and_language(
                &query,
                limit as usize,
                t,
                lang.as_deref(),
            ),
            None => {
                indexer.semantic_search_docs_with_language(&query, limit as usize, lang.as_deref())
            }
        };

        match results {
            Ok(results) => {
                if results.is_empty() {
                    let mut output =
                        format!("No semantically similar documentation found for: {query}");
                    // Add guidance for no results
                    if let Some(guidance) =
                        generate_mcp_guidance(indexer.settings(), "semantic_search_docs", 0)
                    {
                        output.push_str("\n\n---\nGuidance: ");
                        output.push_str(&guidance);
                        output.push('\n');
                    }
                    return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
                }

                let mut result = format!(
                    "Found {} semantically similar result(s) for '{}':\n\n",
                    results.len(),
                    query
                );

                for (i, (symbol, score)) in results.iter().enumerate() {
                    result.push_str(&format!(
                        "{}. {} ({:?}) - Similarity: {:.3}\n",
                        i + 1,
                        symbol.name,
                        symbol.kind,
                        score
                    ));
                    result.push_str(&format!(
                        "   File: {}:{}\n",
                        symbol.file_path,
                        symbol.range.start_line + 1
                    ));

                    if let Some(ref doc) = symbol.doc_comment {
                        // Show first 3 lines of doc
                        let preview: Vec<&str> = doc.lines().take(3).collect();
                        let doc_preview = if doc.lines().count() > 3 {
                            format!("{}...", preview.join(" "))
                        } else {
                            preview.join(" ")
                        };
                        result.push_str(&format!("   Doc: {doc_preview}\n"));
                    }

                    if let Some(ref sig) = symbol.signature {
                        result.push_str(&format!("   Signature: {sig}\n"));
                    }

                    result.push('\n');
                }

                // Add system guidance
                if let Some(guidance) =
                    generate_mcp_guidance(indexer.settings(), "semantic_search_docs", results.len())
                {
                    result.push_str("\n---\nGuidance: ");
                    result.push_str(&guidance);
                    result.push('\n');
                }

                Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Semantic search failed: {e}"
            ))])),
        }
    }

    #[tool(
        description = "Search by natural language and get full context: documentation, dependencies, callers, impact.\n\nReturns symbols with:\n- Their documentation\n- What calls them\n- What they call\n- Complete impact graph (includes ALL relationships: calls, type usage, composition)\n\nUse this when: You want to find and understand symbols with their complete usage context."
    )]
    pub async fn semantic_search_with_context(
        &self,
        Parameters(SemanticSearchWithContextRequest {
            query,
            limit,
            threshold,
            lang,
            output_format,
        }): Parameters<SemanticSearchWithContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        let indexer = self.facade.read().await;

        if output_format == OutputFormat::Json {
            return Ok(
                match service::semantic_search_with_context_data(
                    &indexer,
                    &query,
                    limit as usize,
                    threshold,
                    lang.as_deref(),
                ) {
                    SearchOutcome::Data(results) => {
                        json_result(service::semantic_search_with_context_envelope(
                            &indexer,
                            &query,
                            lang.as_deref(),
                            results,
                        ))
                    }
                    SearchOutcome::InvalidQuery(msg) | SearchOutcome::Error(msg) => {
                        json_result(service::semantic_search_error_envelope(&query, msg))
                    }
                },
            );
        }

        if !indexer.has_semantic_search() {
            tracing::debug!(
                target: "mcp",
                "semantic search not available - index_path: {}, has_semantic: {}",
                indexer.settings().index_path.display(),
                indexer.has_semantic_search()
            );
            // Check if semantic files exist
            let semantic_path = indexer.settings().index_path.join("semantic");
            let metadata_exists = semantic_path.join("metadata.json").exists();
            let vectors_exist = semantic_path.join("segment_0.vec").exists();

            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Semantic search is not enabled. The index needs to be rebuilt with semantic search enabled.\n\nDEBUG INFO:\n- Index path: {}\n- Has semantic search: {}\n- Semantic path: {}\n- Metadata exists: {}\n- Vectors exist: {}",
                indexer.settings().index_path.display(),
                indexer.has_semantic_search(),
                semantic_path.display(),
                metadata_exists,
                vectors_exist
            ))]));
        }

        // First, perform semantic search
        let search_results = match threshold {
            Some(t) => indexer.semantic_search_docs_with_threshold_and_language(
                &query,
                limit as usize,
                t,
                lang.as_deref(),
            ),
            None => {
                indexer.semantic_search_docs_with_language(&query, limit as usize, lang.as_deref())
            }
        };

        match search_results {
            Ok(results) => {
                if results.is_empty() {
                    let mut output = format!("No documentation found matching query: {query}");
                    // Add guidance for no results
                    if let Some(guidance) =
                        generate_mcp_guidance(indexer.settings(), "semantic_search_with_context", 0)
                    {
                        output.push_str("\n\n---\nGuidance: ");
                        output.push_str(&guidance);
                        output.push('\n');
                    }
                    return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
                }

                let mut output = String::new();
                output.push_str(&format!(
                    "Found {} results for query: '{}'\n\n",
                    results.len(),
                    query
                ));

                // For each result, gather comprehensive context
                for (idx, (symbol, score)) in results.iter().enumerate() {
                    // Basic symbol information - matching find_symbol format
                    output.push_str(&format!(
                        "{}. {} - {:?} at {} [symbol_id:{}]\n",
                        idx + 1,
                        symbol.name,
                        symbol.kind,
                        crate::symbol::context::SymbolContext::symbol_location(symbol),
                        symbol.id.value()
                    ));
                    output.push_str(&format!("   Similarity Score: {score:.3}\n"));

                    // Documentation
                    if let Some(ref doc) = symbol.doc_comment {
                        output.push_str("   Documentation:\n");
                        for line in doc.lines().take(5) {
                            output.push_str(&format!("     {line}\n"));
                        }
                        if doc.lines().count() > 5 {
                            output.push_str("     ...\n");
                        }
                    }

                    // Signature
                    if let Some(ref sig) = symbol.signature {
                        output.push_str(&format!("   Signature: {sig}\n"));
                    }

                    // Only gather additional context for functions/methods
                    if matches!(
                        symbol.kind,
                        crate::SymbolKind::Function | crate::SymbolKind::Method
                    ) {
                        // Dependencies (what this function calls) - using logic from get_calls
                        let called_with_metadata =
                            indexer.get_called_functions_with_metadata(symbol.id);
                        if !called_with_metadata.is_empty() {
                            output.push_str(&format!(
                                "\n   {} calls {} function(s):\n",
                                symbol.name,
                                called_with_metadata.len()
                            ));
                            for (i, (called, metadata)) in
                                called_with_metadata.iter().take(10).enumerate()
                            {
                                // Parse receiver information from metadata and get call site location
                                let (call_display, call_line) = if let Some(meta) = metadata {
                                    let display = if let Some(context) = &meta.context {
                                        if context.contains("receiver:")
                                            && context.contains("static:")
                                        {
                                            let parts: Vec<&str> = context.split(',').collect();
                                            let mut receiver = None;
                                            let mut is_static = false;

                                            for part in parts {
                                                if let Some(recv) = part.strip_prefix("receiver:") {
                                                    receiver = Some(recv.trim());
                                                } else if let Some(static_val) =
                                                    part.strip_prefix("static:")
                                                {
                                                    is_static = static_val.trim() == "true";
                                                }
                                            }

                                            match (receiver, is_static) {
                                                (Some("self"), false) => {
                                                    format!("(self.{})", called.name)
                                                }
                                                (Some(recv), true) if recv != "self" => {
                                                    format!("({}::{})", recv, called.name)
                                                }
                                                (Some(recv), false) if recv != "self" => {
                                                    format!("({}.{})", recv, called.name)
                                                }
                                                _ => called.name.to_string(),
                                            }
                                        } else {
                                            called.name.to_string()
                                        }
                                    } else {
                                        called.name.to_string()
                                    };

                                    // Use call site line if available
                                    let line = meta
                                        .line
                                        .map(|l| l + 1)
                                        .unwrap_or(called.range.start_line + 1);
                                    (display, line)
                                } else {
                                    (called.name.to_string(), called.range.start_line + 1)
                                };

                                output.push_str(&format!(
                                    "     -> {:?} {} at {}:{} [symbol_id:{}]\n",
                                    called.kind,
                                    call_display,
                                    called.file_path,
                                    call_line,
                                    called.id.value()
                                ));
                                if i == 9 && called_with_metadata.len() > 10 {
                                    output.push_str(&format!(
                                        "     ... and {} more\n",
                                        called_with_metadata.len() - 10
                                    ));
                                }
                            }
                        }

                        // Callers (who uses this function) - using logic from find_callers
                        let calling_functions_with_metadata =
                            indexer.get_calling_functions_with_metadata(symbol.id);
                        if !calling_functions_with_metadata.is_empty() {
                            output.push_str(&format!(
                                "\n   {} function(s) call {}:\n",
                                calling_functions_with_metadata.len(),
                                symbol.name
                            ));
                            for (i, (caller, metadata)) in
                                calling_functions_with_metadata.iter().take(10).enumerate()
                            {
                                // Parse metadata to extract receiver info and call site location
                                let (call_info, call_line) = if let Some(meta) = metadata {
                                    let info = if let Some(context) = &meta.context {
                                        if context.contains("receiver:")
                                            && context.contains("static:")
                                        {
                                            // Parse "receiver:{receiver},static:{is_static}"
                                            let parts: Vec<&str> = context.split(',').collect();
                                            let mut receiver = "";
                                            let mut is_static = false;

                                            for part in parts {
                                                if let Some(r) = part.strip_prefix("receiver:") {
                                                    receiver = r;
                                                } else if let Some(s) = part.strip_prefix("static:")
                                                {
                                                    is_static = s == "true";
                                                }
                                            }

                                            if !receiver.is_empty() {
                                                let qualified_name = if is_static {
                                                    format!("{}::{}", receiver, symbol.name)
                                                } else {
                                                    format!("{}.{}", receiver, symbol.name)
                                                };
                                                format!(" (calls {qualified_name})")
                                            } else {
                                                String::new()
                                            }
                                        } else {
                                            String::new()
                                        }
                                    } else {
                                        String::new()
                                    };

                                    // Use call site line if available
                                    let line = meta
                                        .line
                                        .map(|l| l + 1)
                                        .unwrap_or(caller.range.start_line + 1);
                                    (info, line)
                                } else {
                                    (String::new(), caller.range.start_line + 1)
                                };

                                output.push_str(&format!(
                                    "     <- {:?} {} at {}:{}{} [symbol_id:{}]\n",
                                    caller.kind,
                                    caller.name,
                                    caller.file_path,
                                    call_line,
                                    call_info,
                                    caller.id.value()
                                ));
                                if i == 9 && calling_functions_with_metadata.len() > 10 {
                                    output.push_str(&format!(
                                        "     ... and {} more\n",
                                        calling_functions_with_metadata.len() - 10
                                    ));
                                }
                            }
                        }

                        // Impact analysis - using logic from analyze_impact
                        let impacted = indexer.get_impact_radius(symbol.id, Some(2));
                        if !impacted.is_empty() {
                            output.push_str(&format!(
                                "\n   Changing {} would impact {} symbol(s) (max depth: 2):\n",
                                symbol.name,
                                impacted.len()
                            ));

                            // Get details and group by kind
                            let impacted_details: Vec<_> = impacted
                                .iter()
                                .filter_map(|id| indexer.get_symbol(*id))
                                .collect();

                            // Group by kind
                            let mut methods = Vec::new();
                            let mut functions = Vec::new();
                            let mut other = Vec::new();

                            for sym in impacted_details {
                                match sym.kind {
                                    crate::SymbolKind::Method => methods.push(sym),
                                    crate::SymbolKind::Function => functions.push(sym),
                                    _ => other.push(sym),
                                }
                            }

                            if !methods.is_empty() {
                                output.push_str(&format!("\n     methods ({}):\n", methods.len()));
                                for method in methods.iter().take(5) {
                                    output.push_str(&format!(
                                        "       - {} [symbol_id:{}]\n",
                                        method.name,
                                        method.id.value()
                                    ));
                                }
                                if methods.len() > 5 {
                                    output.push_str(&format!(
                                        "       ... and {} more\n",
                                        methods.len() - 5
                                    ));
                                }
                            }

                            if !functions.is_empty() {
                                output.push_str(&format!(
                                    "\n     functions ({}):\n",
                                    functions.len()
                                ));
                                for func in functions.iter().take(5) {
                                    output.push_str(&format!(
                                        "       - {} [symbol_id:{}]\n",
                                        func.name,
                                        func.id.value()
                                    ));
                                }
                                if functions.len() > 5 {
                                    output.push_str(&format!(
                                        "       ... and {} more\n",
                                        functions.len() - 5
                                    ));
                                }
                            }

                            if !other.is_empty() {
                                output.push_str(&format!("\n     other ({}):\n", other.len()));
                                for sym in other.iter().take(3) {
                                    output.push_str(&format!(
                                        "       - {} ({:?}) [symbol_id:{}]\n",
                                        sym.name,
                                        sym.kind,
                                        sym.id.value()
                                    ));
                                }
                            }
                        }
                    }

                    // Show inheritance relationships for classes/structs/enums
                    if matches!(
                        symbol.kind,
                        crate::SymbolKind::Class
                            | crate::SymbolKind::Struct
                            | crate::SymbolKind::Enum
                    ) {
                        // What does this class extend?
                        let extends = indexer.get_extends(symbol.id);
                        if !extends.is_empty() {
                            output.push_str(&format!(
                                "\n   {} extends {} class(es):\n",
                                symbol.name,
                                extends.len()
                            ));
                            for (i, base_class) in extends.iter().take(5).enumerate() {
                                output.push_str(&format!(
                                    "     -> {:?} {} at {} [symbol_id:{}]\n",
                                    base_class.kind,
                                    base_class.name,
                                    crate::symbol::context::SymbolContext::symbol_location(
                                        base_class
                                    ),
                                    base_class.id.value()
                                ));
                                if i == 4 && extends.len() > 5 {
                                    output.push_str(&format!(
                                        "     ... and {} more\n",
                                        extends.len() - 5
                                    ));
                                }
                            }
                        }

                        // What classes extend this class?
                        let extended_by = indexer.get_extended_by(symbol.id);
                        if !extended_by.is_empty() {
                            output.push_str(&format!(
                                "\n   {} class(es) extend {}:\n",
                                extended_by.len(),
                                symbol.name
                            ));
                            for (i, derived_class) in extended_by.iter().take(5).enumerate() {
                                output.push_str(&format!(
                                    "     <- {:?} {} at {} [symbol_id:{}]\n",
                                    derived_class.kind,
                                    derived_class.name,
                                    crate::symbol::context::SymbolContext::symbol_location(
                                        derived_class
                                    ),
                                    derived_class.id.value()
                                ));
                                if i == 4 && extended_by.len() > 5 {
                                    output.push_str(&format!(
                                        "     ... and {} more\n",
                                        extended_by.len() - 5
                                    ));
                                }
                            }
                        }

                        // What traits does this type implement?
                        let implements = indexer.get_implemented_traits(symbol.id);
                        if !implements.is_empty() {
                            output.push_str(&format!(
                                "\n   {} implements {} trait(s):\n",
                                symbol.name,
                                implements.len()
                            ));
                            for (i, trait_sym) in implements.iter().take(5).enumerate() {
                                output.push_str(&format!(
                                    "     -> {:?} {} at {} [symbol_id:{}]\n",
                                    trait_sym.kind,
                                    trait_sym.name,
                                    crate::symbol::context::SymbolContext::symbol_location(
                                        trait_sym
                                    ),
                                    trait_sym.id.value()
                                ));
                                if i == 4 && implements.len() > 5 {
                                    output.push_str(&format!(
                                        "     ... and {} more\n",
                                        implements.len() - 5
                                    ));
                                }
                            }
                        }
                    }

                    // Show what implements this trait/interface
                    if matches!(
                        symbol.kind,
                        crate::SymbolKind::Trait | crate::SymbolKind::Interface
                    ) {
                        let implementations = indexer.get_implementations(symbol.id);
                        if !implementations.is_empty() {
                            output.push_str(&format!(
                                "\n   {} type(s) implement {}:\n",
                                implementations.len(),
                                symbol.name
                            ));
                            for (i, impl_sym) in implementations.iter().take(5).enumerate() {
                                output.push_str(&format!(
                                    "     <- {:?} {} at {} [symbol_id:{}]\n",
                                    impl_sym.kind,
                                    impl_sym.name,
                                    crate::symbol::context::SymbolContext::symbol_location(
                                        impl_sym
                                    ),
                                    impl_sym.id.value()
                                ));
                                if i == 4 && implementations.len() > 5 {
                                    output.push_str(&format!(
                                        "     ... and {} more\n",
                                        implementations.len() - 5
                                    ));
                                }
                            }
                        }
                    }

                    // Show uses relationships (for all symbols)
                    let uses = indexer.get_uses(symbol.id);
                    if !uses.is_empty() {
                        output.push_str(&format!(
                            "\n   {} uses {} type(s):\n",
                            symbol.name,
                            uses.len()
                        ));
                        for (i, used_type) in uses.iter().take(5).enumerate() {
                            output.push_str(&format!(
                                "     -> {:?} {} at {} [symbol_id:{}]\n",
                                used_type.kind,
                                used_type.name,
                                crate::symbol::context::SymbolContext::symbol_location(used_type),
                                used_type.id.value()
                            ));
                            if i == 4 && uses.len() > 5 {
                                output.push_str(&format!("     ... and {} more\n", uses.len() - 5));
                            }
                        }
                    }

                    // What symbols use this type?
                    let used_by = indexer.get_used_by(symbol.id);
                    if !used_by.is_empty() {
                        output.push_str(&format!(
                            "\n   {} type(s) use {}:\n",
                            used_by.len(),
                            symbol.name
                        ));
                        for (i, using_symbol) in used_by.iter().take(5).enumerate() {
                            output.push_str(&format!(
                                "     <- {:?} {} at {} [symbol_id:{}]\n",
                                using_symbol.kind,
                                using_symbol.name,
                                crate::symbol::context::SymbolContext::symbol_location(
                                    using_symbol
                                ),
                                using_symbol.id.value()
                            ));
                            if i == 4 && used_by.len() > 5 {
                                output.push_str(&format!(
                                    "     ... and {} more\n",
                                    used_by.len() - 5
                                ));
                            }
                        }
                    }

                    output.push('\n');
                }

                // Add system guidance
                if let Some(guidance) = generate_mcp_guidance(
                    indexer.settings(),
                    "semantic_search_with_context",
                    results.len(),
                ) {
                    output.push_str("\n---\nGuidance: ");
                    output.push_str(&guidance);
                    output.push('\n');
                }

                Ok(CallToolResult::success(vec![ContentBlock::text(output)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Semantic search failed: {e}"
            ))])),
        }
    }

    #[tool(description = "Search for symbols using full-text search with fuzzy matching")]
    pub async fn search_symbols(
        &self,
        Parameters(SearchSymbolsRequest {
            query,
            limit,
            kind,
            module,
            lang,
            output_format,
        }): Parameters<SearchSymbolsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let indexer = self.facade.read().await;

        if output_format == OutputFormat::Json {
            return Ok(
                match service::search_symbols_data(
                    &indexer,
                    &query,
                    limit as usize,
                    kind.as_deref(),
                    module.as_deref(),
                    lang.as_deref(),
                ) {
                    SearchOutcome::Data(results) => json_result(service::search_symbols_envelope(
                        &indexer,
                        &query,
                        lang.as_deref(),
                        results,
                    )),
                    SearchOutcome::InvalidQuery(msg) => {
                        let envelope: Envelope<()> = Envelope::error(ResultCode::InvalidQuery, msg)
                            .with_entity_type(EntityType::SearchResult)
                            .with_query(&query);
                        json_result(envelope)
                    }
                    SearchOutcome::Error(msg) => {
                        let envelope: Envelope<()> = Envelope::error(
                            ResultCode::InvalidQuery,
                            format!("Failed to search for '{query}': {msg}"),
                        )
                        .with_entity_type(EntityType::SearchResult)
                        .with_query(&query)
                        .with_hint("Check query syntax");
                        json_result(envelope)
                    }
                },
            );
        }

        // One kind vocabulary (SymbolKind::from_str); unknown kinds error
        // instead of silently returning unfiltered results.
        let kind_filter = match kind.as_deref().map(str::parse::<crate::SymbolKind>) {
            None => None,
            Some(Ok(k)) => Some(k),
            Some(Err(e)) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        match indexer.search(
            &query,
            limit as usize,
            kind_filter,
            module.as_deref(),
            lang.as_deref(),
        ) {
            Ok(results) => {
                if results.is_empty() {
                    let mut output = format!("No results found for query: {query}");
                    // Add guidance for no results
                    if let Some(guidance) =
                        generate_mcp_guidance(indexer.settings(), "search_symbols", 0)
                    {
                        output.push_str("\n\n---\nGuidance: ");
                        output.push_str(&guidance);
                        output.push('\n');
                    }
                    return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
                }

                let mut result = format!(
                    "Found {} result(s) for query '{}':\n\n",
                    results.len(),
                    query
                );

                for (i, search_result) in results.iter().enumerate() {
                    result.push_str(&format!(
                        "{}. {} ({:?})\n",
                        i + 1,
                        search_result.name,
                        search_result.kind
                    ));
                    result.push_str(&format!(
                        "   File: {}:{}\n",
                        search_result.file_path, search_result.line
                    ));

                    if !search_result.module_path.is_empty() {
                        result.push_str(&format!("   Module: {}\n", search_result.module_path));
                    }

                    if let Some(ref doc) = search_result.doc_comment {
                        // Show first line of doc comment
                        let first_line = doc.lines().next().unwrap_or("");
                        result.push_str(&format!("   Doc: {first_line}\n"));
                    }

                    if let Some(ref sig) = search_result.signature {
                        result.push_str(&format!("   Signature: {sig}\n"));
                    }

                    result.push_str(&format!("   Score: {:.2}\n", search_result.score));
                    result.push('\n');
                }

                // Add system guidance
                if let Some(guidance) =
                    generate_mcp_guidance(indexer.settings(), "search_symbols", results.len())
                {
                    result.push_str("\n---\nGuidance: ");
                    result.push_str(&guidance);
                    result.push('\n');
                }

                Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Search failed: {e}"
            ))])),
        }
    }

    #[tool(
        description = "Search indexed documents (markdown, text files) using natural language queries. Returns relevant chunks with context and highlighted keywords."
    )]
    pub async fn search_documents(
        &self,
        Parameters(SearchDocumentsRequest {
            query,
            collection,
            limit,
            output_format,
        }): Parameters<SearchDocumentsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.search_documents_inner(query, collection, limit, output_format, None)
            .await
    }

    /// Test-only entry point into [`Self::search_documents_inner`] that
    /// signals via `search_phase_started` the instant the auto-sync write
    /// guard has been dropped and the read-guarded `search` call is about
    /// to begin, so tests can observe the read-guarded phase deterministically
    /// instead of racing on wall-clock timing.
    #[cfg(test)]
    pub(crate) async fn search_documents_for_test(
        &self,
        query: String,
        collection: Option<String>,
        limit: u32,
        search_phase_started: tokio::sync::oneshot::Sender<()>,
    ) -> Result<CallToolResult, McpError> {
        self.search_documents_inner(
            query,
            collection,
            limit,
            OutputFormat::Text,
            Some(search_phase_started),
        )
        .await
    }

    /// Shared implementation behind [`Self::search_documents`] and the
    /// test-only [`Self::search_documents_for_test`]. `search_phase_started`
    /// is `None` in production and fires (test-only) the instant the
    /// auto-sync write guard is dropped, before the read-guarded `search`
    /// call begins.
    async fn search_documents_inner(
        &self,
        query: String,
        collection: Option<String>,
        limit: u32,
        output_format: OutputFormat,
        search_phase_started: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<CallToolResult, McpError> {
        let store = match &self.document_store {
            Some(s) => s,
            None => {
                if output_format == OutputFormat::Json {
                    let envelope: Envelope<()> = Envelope::error(
                        ResultCode::IndexError,
                        "Document search not available. No document collections are indexed.",
                    )
                    .with_entity_type(EntityType::Document)
                    .with_query(&query)
                    .with_hint("Run 'codanna documents index' to create the index");
                    return Ok(json_result(envelope));
                }
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "Document search not available. No document collections are indexed.\n\n\
                    To enable:\n\
                    1. Add a collection: codanna documents add-collection docs docs/\n\
                    2. Index it: codanna documents index\n\
                    3. Restart the MCP server",
                )]));
            }
        };

        let indexer = self.facade.read().await;

        // Auto-sync: check for file changes in all collections before
        // searching. This is the only step that needs exclusive access to
        // the document store, so the write guard is scoped to this loop
        // only and dropped before searching (see the concurrency contract
        // documented in RCSR-README.md).
        {
            let mut sync_guard = store.write().await;
            let settings = indexer.settings();
            for (name, config) in &settings.documents.collections {
                if let Err(e) =
                    sync_guard.index_collection(name, config, &settings.documents.defaults)
                {
                    tracing::warn!(target: "rag", "auto-sync failed for collection '{}': {}", name, e);
                }
            }
        }

        if let Some(tx) = search_phase_started {
            let _ = tx.send(());
        }

        if output_format == OutputFormat::Json {
            let store = store.read().await;
            return Ok(
                match service::search_documents_data(
                    &store,
                    indexer.settings(),
                    &query,
                    collection,
                    limit as usize,
                ) {
                    Ok(results) => {
                        let count = results.len();
                        let envelope = if count == 0 {
                            Envelope::<Vec<crate::documents::SearchResult>>::not_found(format!(
                                "No documents found for '{query}'"
                            ))
                            .with_entity_type(EntityType::Document)
                            .with_query(&query)
                        } else {
                            Envelope::success(results)
                                .with_entity_type(EntityType::Document)
                                .with_count(count)
                                .with_query(&query)
                                .with_message(format!("Found {count} matching documents"))
                                .with_hint(
                                    "Use the file paths and byte ranges to read specific sections",
                                )
                        };
                        json_result(envelope)
                    }
                    Err(e) => {
                        let envelope: Envelope<()> = Envelope::error(
                            ResultCode::IndexError,
                            format!("Document search failed: {e}"),
                        )
                        .with_entity_type(EntityType::Document)
                        .with_query(&query);
                        json_result(envelope)
                    }
                },
            );
        }

        // Auto-sync already ran above (write guard scoped to the sync loop
        // and already dropped). `DocumentStore::search` only needs `&self`,
        // so the search itself runs under a read guard, letting concurrent
        // `search_documents` calls make progress against each other.
        let search_query = DocSearchQuery {
            text: query.clone(),
            collection,
            document: None,
            limit: limit as usize,
            preview_config: Some(indexer.settings().documents.search.clone()),
        };

        let store = store.read().await;
        match store.search(search_query) {
            Ok(results) => {
                if results.is_empty() {
                    let mut output = format!("No documents found for: {query}");
                    if let Some(guidance) =
                        generate_mcp_guidance(indexer.settings(), "search_documents", 0)
                    {
                        output.push_str("\n\n---\nGuidance: ");
                        output.push_str(&guidance);
                        output.push('\n');
                    }
                    return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
                }

                let mut output = format!(
                    "Found {} document(s) matching '{}':\n\n",
                    results.len(),
                    query
                );

                for (i, result) in results.iter().enumerate() {
                    output.push_str(&format!(
                        "{}. {} (score: {:.3})\n",
                        i + 1,
                        result.source_path.display(),
                        result.similarity
                    ));

                    if !result.heading_context.is_empty() {
                        output.push_str(&format!(
                            "   Context: {}\n",
                            result.heading_context.join(" > ")
                        ));
                    }

                    // Preview is already KWIC-processed with highlighting
                    output.push_str(&format!("   Preview: {}\n\n", result.content_preview));
                }

                if let Some(guidance) =
                    generate_mcp_guidance(indexer.settings(), "search_documents", results.len())
                {
                    output.push_str("\n---\nGuidance: ");
                    output.push_str(&guidance);
                    output.push('\n');
                }

                Ok(CallToolResult::success(vec![ContentBlock::text(output)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Document search failed: {e}"
            ))])),
        }
    }
}

#[cfg(test)]
mod search_documents_concurrency_tests {
    use super::*;
    use crate::config::Settings;
    use crate::documents::{CollectionConfig, DocumentStore};
    use crate::indexing::facade::IndexFacade;
    use crate::mcp::server::CodeIntelligenceServer;
    use crate::vector::VectorDimension;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Builds fixture settings for a single `docs` collection.
    ///
    /// `file_count` stays small so the auto-sync file-walk/hash step (which
    /// runs on every `search_documents` call, even when nothing changed,
    /// and whose cost scales with total file content) is cheap and does not
    /// dominate the timing signal used below. Each file is one giant single
    /// paragraph (no blank lines, so the chunker never splits on paragraph
    /// boundaries); a very tight `max_chunk_chars` override then makes the
    /// chunker's sliding-window split produce many chunks from that small
    /// amount of content, so `chunks_per_file` mostly controls the cost of
    /// `search`'s per-chunk KWIC-enrichment step (one tantivy lookup per
    /// candidate) rather than the cost of auto-sync's file hashing.
    /// Returns the fixture `Settings` together with the backing `TempDir`.
    /// The settings hold paths into the temp dir, so callers must keep the
    /// returned `TempDir` alive (bound to a variable) for as long as the
    /// settings/server are in use -- dropping it removes the temp dir from
    /// disk. Returning it here (instead of leaking it) keeps fixture temp
    /// dirs from accumulating across test runs.
    fn fixture_settings(file_count: usize, chunks_per_file: usize) -> (Settings, TempDir) {
        let temp = tempfile::tempdir().expect("create temp root");
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir_all(&docs_dir).expect("create docs dir");

        const CHUNK_CHARS: usize = 20;
        let sentence = "lorem ipsum ";
        let body = sentence.repeat((chunks_per_file * CHUNK_CHARS / sentence.len()) + 4);
        for i in 0..file_count {
            std::fs::write(docs_dir.join(format!("doc_{i}.md")), &body)
                .unwrap_or_else(|e| panic!("write doc_{i}.md fixture: {e}"));
        }

        let index_dir = temp.path().join("index");
        let mut settings = Settings {
            index_path: index_dir,
            workspace_root: None,
            ..Default::default()
        };
        settings.documents.collections.insert(
            "docs".to_string(),
            CollectionConfig {
                paths: vec![docs_dir],
                patterns: vec!["**/*.md".to_string()],
                min_chunk_chars: Some(5),
                max_chunk_chars: Some(CHUNK_CHARS),
                overlap_chars: Some(2),
                ..Default::default()
            },
        );

        (settings, temp)
    }

    /// Builds a `CodeIntelligenceServer` over the given settings, with a
    /// real `DocumentStore` backing the settings' `docs` collection,
    /// pre-synced once outside the timed test phase so the auto-sync loop
    /// inside `search_documents` is a fast no-op for every call made during
    /// a test -- isolating any timing signal to the `search` step itself,
    /// not collection scanning. No embedding generator is configured (keeps
    /// tests hermetic -- no network/model download), so `search` exercises
    /// the non-vector `enrich_results` path.
    fn build_server(settings: Settings) -> CodeIntelligenceServer {
        let index_path = settings.index_path.clone();
        let collection_config = settings.documents.collections["docs"].clone();
        let chunking_defaults = settings.documents.defaults.clone();

        let facade = IndexFacade::new(Arc::new(settings)).expect("create facade over temp index");

        let mut store = DocumentStore::new(
            index_path.join("documents"),
            VectorDimension::dimension_384(),
        )
        .expect("create document store");

        store
            .index_collection("docs", &collection_config, &chunking_defaults)
            .expect("pre-sync docs collection");

        CodeIntelligenceServer::new(facade).with_document_store(store)
    }

    fn search_request(limit: usize) -> Parameters<SearchDocumentsRequest> {
        Parameters(SearchDocumentsRequest {
            query: "lorem".to_string(),
            collection: None,
            limit: limit as u32,
            output_format: OutputFormat::Text,
        })
    }

    /// Terminal-state / provenance regression for `search_documents`'s
    /// lock scoping, mirroring
    /// `run_reindex_releases_write_lock_during_off_lock_walk`
    /// (mcp/server.rs): the write guard used for collection auto-sync must
    /// be dropped before `DocumentStore::search` runs, and `search` itself
    /// must run under a read guard so concurrent `search_documents` calls
    /// can make progress against each other instead of serializing.
    ///
    /// This is the discriminating check against a regression that instead
    /// moves `.search()` back inside the same write guard used for
    /// auto-sync: under that regression, a fresh `try_read()` on the
    /// document-store lock would fail for the entire duration a first call
    /// spends inside `search`, since the write guard would still be held.
    /// Under the correct fix, `search` runs under a read guard, so a fresh
    /// `try_read()` succeeds -- concurrently -- while the first call's
    /// `search` step is still in flight, exactly like a second
    /// `search_documents` call would be able to.
    ///
    /// Uses [`CodeIntelligenceServer::search_documents_for_test`]'s
    /// phase-started signal (fired the instant the auto-sync write guard is
    /// dropped) rather than wall-clock racing between two spawned tasks:
    /// `DocumentStore::search` is synchronous CPU-bound code with no
    /// internal `.await`, so tokio's scheduler is not guaranteed to run two
    /// freshly spawned short-lived tasks on genuinely separate OS threads,
    /// making a raw wall-clock-overlap comparison an unreliable
    /// (scheduler-dependent) discriminator.
    ///
    /// Requires a multi-thread runtime: `search`'s synchronous CPU-bound
    /// work has no internal `.await`, so on a current-thread runtime the
    /// polling loop below would never be scheduled while `search_task` is
    /// mid-`search` -- it would only ever observe the state before
    /// `search_task` starts or after it finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn search_documents_search_phase_runs_under_read_guard() {
        // Enough chunks that `search`'s KWIC-enrichment step takes long
        // enough to reliably sample `try_read()` multiple times while it is
        // still in flight.
        const FILE_COUNT: usize = 3;
        const CHUNKS_PER_FILE: usize = 6000;
        const TOTAL_CHUNKS: usize = FILE_COUNT * CHUNKS_PER_FILE;

        let (settings, _temp) = fixture_settings(FILE_COUNT, CHUNKS_PER_FILE);
        let server = build_server(settings);
        let store_arc = server
            .document_store
            .clone()
            .expect("server must have a document store configured");

        let (search_phase_started_tx, search_phase_started_rx) = tokio::sync::oneshot::channel();

        let search_server = server.clone();
        let search_task = tokio::spawn(async move {
            search_server
                .search_documents_for_test(
                    "lorem".to_string(),
                    None,
                    TOTAL_CHUNKS as u32,
                    search_phase_started_tx,
                )
                .await
        });

        // Wait for the auto-sync write guard to be dropped and the
        // read-guarded `search` call to be about to start, ruling out the
        // pre-start window where `try_read()` would trivially succeed
        // simply because the spawned task had not yet been polled.
        search_phase_started_rx
            .await
            .expect("search_documents_for_test must signal before the search phase starts");

        // Sample `try_read()` while the search task is still in flight.
        // Require several consecutive successes (rather than a single one)
        // so a regression that re-holds the write guard across `.search()`
        // -- which would still fire the phase-started signal, since that
        // send is unconditional, but would keep the write guard alive
        // during `search` -- reliably fails this assertion instead of
        // getting lucky on a single sample.
        const REQUIRED_CONSECUTIVE_SUCCESSES: u32 = 5;
        let mut consecutive_successes = 0u32;
        let mut attempts = 0;
        while !search_task.is_finished() && attempts < 200_000 {
            if store_arc.try_read().is_ok() {
                consecutive_successes += 1;
                if consecutive_successes >= REQUIRED_CONSECUTIVE_SUCCESSES {
                    break;
                }
            } else {
                consecutive_successes = 0;
            }
            attempts += 1;
            if attempts % 100 == 0 {
                tokio::time::sleep(std::time::Duration::from_micros(50)).await;
            } else {
                tokio::task::yield_now().await;
            }
        }

        let acquired_while_in_flight = consecutive_successes >= REQUIRED_CONSECUTIVE_SUCCESSES;

        let result = search_task
            .await
            .expect("search task must not panic")
            .expect("search_documents_for_test must succeed");

        assert!(
            acquired_while_in_flight,
            "expected try_read() on the document store to succeed {REQUIRED_CONSECUTIVE_SUCCESSES} \
             times in a row while search_documents's `search` step was still in flight; a \
             regression that re-holds the write guard across `.search()` would fail try_read() \
             for the search step's entire in-flight duration"
        );
        assert!(
            !result.is_error.unwrap_or(false),
            "search_documents_for_test result must not be an error"
        );
        assert!(
            text_of(&result).contains("Found"),
            "search_documents_for_test must find matching documents, got: {}",
            text_of(&result)
        );
    }

    /// Two concurrent `search_documents` calls against the same server both
    /// make progress and both succeed -- neither one is starved or blocked
    /// on the other's error path. This exercises the two-call-site
    /// hoisting of the auto-sync write step (text path and JSON path) end
    /// to end, complementing the single-call lock-scoping check above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn search_documents_concurrent_calls_both_succeed() {
        const FILE_COUNT: usize = 3;
        const CHUNKS_PER_FILE: usize = 50;
        const TOTAL_CHUNKS: usize = FILE_COUNT * CHUNKS_PER_FILE;

        let (settings, _temp) = fixture_settings(FILE_COUNT, CHUNKS_PER_FILE);
        let server = build_server(settings);

        let server_a = server.clone();
        let server_b = server.clone();

        let task_a = tokio::spawn(async move {
            server_a
                .search_documents(search_request(TOTAL_CHUNKS))
                .await
        });
        let task_b = tokio::spawn(async move {
            server_b
                .search_documents(search_request(TOTAL_CHUNKS))
                .await
        });

        let (result_a, result_b) = tokio::join!(task_a, task_b);

        let result_a = result_a
            .expect("task a must not panic")
            .expect("search_documents call a must succeed");
        let result_b = result_b
            .expect("task b must not panic")
            .expect("search_documents call b must succeed");

        for (label, result) in [("a", &result_a), ("b", &result_b)] {
            assert!(
                !result.is_error.unwrap_or(false),
                "concurrent search_documents call {label} must not be an error result"
            );
            assert!(
                text_of(result).contains("Found"),
                "concurrent search_documents call {label} must find matching documents"
            );
        }
    }

    /// `search_documents`'s text output must append the same
    /// `generate_mcp_guidance` block as `search_symbols` (search.rs, the
    /// `search_symbols` handler) in both the empty-result and
    /// non-empty-result branches, once a guidance template is configured
    /// for the tool.
    #[tokio::test]
    async fn search_documents_text_output_includes_configured_guidance() {
        use crate::config::{GuidanceRange, GuidanceTemplate};

        let (mut settings, _temp) = fixture_settings(3, 1);
        settings.guidance.enabled = true;
        settings.guidance.templates.insert(
            "search_documents".to_string(),
            GuidanceTemplate {
                no_results: Some("no-results-guidance-marker".to_string()),
                single_result: None,
                multiple_results: None,
                custom: vec![GuidanceRange {
                    min: 1,
                    max: None,
                    template: "found-results-guidance-marker".to_string(),
                }],
            },
        );

        let server = build_server(settings);

        // Empty-result branch: `DocumentStore::search` filters by exact
        // collection match (query text only ranks/relevance-scores when an
        // embedding generator is configured, which this fixture
        // deliberately omits), so a nonexistent collection is the reliable
        // way to force zero candidates here.
        let empty_result = server
            .search_documents(Parameters(SearchDocumentsRequest {
                query: "lorem".to_string(),
                collection: Some("no-such-collection".to_string()),
                limit: 10,
                output_format: OutputFormat::Text,
            }))
            .await
            .expect("empty-result search_documents call must succeed");
        assert!(
            text_of(&empty_result).contains("no-results-guidance-marker"),
            "expected the configured no-results guidance template in the empty-result branch, \
             got: {}",
            text_of(&empty_result)
        );

        // Non-empty-result branch.
        let non_empty_result = server
            .search_documents(search_request(10))
            .await
            .expect("non-empty search_documents call must succeed");
        assert!(
            text_of(&non_empty_result).contains("found-results-guidance-marker"),
            "expected the configured guidance template in the non-empty-result branch, got: {}",
            text_of(&non_empty_result)
        );
    }
}
