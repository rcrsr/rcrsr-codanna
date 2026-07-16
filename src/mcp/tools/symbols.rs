//! Symbol-target tools: find_symbol, get_calls, find_callers, analyze_impact.

use rmcp::model::ErrorData as McpError;
use rmcp::model::*;
use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use crate::Symbol;
use crate::indexing::facade::IndexFacade;
use crate::io::envelope::{EntityType, Envelope, ResultCode};
use crate::io::guidance_engine::generate_guidance_from_config;
use crate::mcp::requests::{
    AnalyzeImpactRequest, FindCallersRequest, FindSymbolRequest, FindSymbolsRequest,
    GetCallsRequest, GetFileOutlineRequest, OutputFormat, ReadSymbolRequest,
};
use crate::mcp::server::{CodeIntelligenceServer, generate_mcp_guidance};
use crate::mcp::service::{
    self, RelationOutcome, SymbolResolution, ambiguous_envelope, json_result,
    parse_receiver_context, qualified_call, render_ambiguity,
};
use serde::Serialize;

/// Cap on `find_symbols` batch size, mirroring `MAX_REINDEX_PATHS`
/// (`server.rs`): protects against unbounded request payloads.
///
/// `pub(crate)` so the CLI's `--json` pre-collection path (`cli/commands/mcp.rs`)
/// can enforce the identical cap without reimplementing it.
pub(crate) const MAX_FIND_SYMBOLS_NAMES: usize = 1024;

/// Per-name outcome of a `find_symbols` batch lookup.
///
/// `pub(crate)` so the CLI's `--json` pre-collection path can reuse this type
/// rather than re-deriving the found/not_found/ambiguous shape.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum FindSymbolsEntry {
    Found {
        location: String,
        kind: String,
        signature: Option<String>,
        line_range: [u32; 2],
    },
    NotFound,
    Ambiguous {
        candidates: Vec<FindSymbolsCandidate>,
    },
}

/// A single candidate symbol surfaced in an `Ambiguous` entry.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FindSymbolsCandidate {
    location: String,
    kind: String,
    signature: Option<String>,
    line_range: [u32; 2],
}

impl FindSymbolsCandidate {
    fn from_symbol(symbol: &Symbol) -> Self {
        Self {
            location: crate::symbol::context::SymbolContext::symbol_location(symbol),
            kind: format!("{:?}", symbol.kind),
            signature: symbol.signature.as_deref().map(str::to_string),
            line_range: [symbol.range.start_line + 1, symbol.range.end_line + 1],
        }
    }
}

/// Classify one name's lookup result into found/not_found/ambiguous.
///
/// Deliberately lighter than [`service::find_symbol_data`]: it resolves via
/// the plain `find_symbols_by_name`/`find_dotted_members` facade lookups
/// (the same resolution algorithm — exact match falling back to
/// dotted-member lookup) instead of building each match's full
/// symbol-card context (`get_symbol_context` with `SYMBOL_CARD`, ~8
/// relationship-index queries). `FindSymbolsCandidate` only reads
/// name/kind/signature/location/line_range off the plain `Symbol`, so a
/// batch of up to [`MAX_FIND_SYMBOLS_NAMES`] names would otherwise discard
/// thousands of relationship lookups it never uses. `find_symbol`'s own
/// JSON path still calls `service::find_symbol_data` for its full-context
/// single-name result.
///
/// `pub(crate)` so the CLI's `--json` pre-collection path (`cli/commands/mcp.rs`)
/// can build the identical per-name results without a parallel implementation.
pub(crate) fn find_symbols_entry(
    indexer: &IndexFacade,
    name: &str,
    lang: Option<&str>,
) -> FindSymbolsEntry {
    let mut symbols = indexer.find_symbols_by_name(name, lang);
    if symbols.is_empty() {
        symbols = service::find_dotted_members(name, |n| indexer.find_symbols_by_name(n, lang));
    }
    match symbols.len() {
        0 => FindSymbolsEntry::NotFound,
        1 => {
            let candidate = FindSymbolsCandidate::from_symbol(&symbols[0]);
            FindSymbolsEntry::Found {
                location: candidate.location,
                kind: candidate.kind,
                signature: candidate.signature,
                line_range: candidate.line_range,
            }
        }
        _ => FindSymbolsEntry::Ambiguous {
            candidates: symbols
                .iter()
                .map(FindSymbolsCandidate::from_symbol)
                .collect(),
        },
    }
}

/// One symbol row in a `get_file_outline` response: enough to navigate
/// without opening the file (kind/signature/visibility/line span).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FileOutlineEntry {
    name: String,
    kind: String,
    signature: Option<String>,
    visibility: String,
    start_line: u32,
    end_line: u32,
}

impl FileOutlineEntry {
    fn from_symbol(symbol: &Symbol) -> Self {
        Self {
            name: symbol.name.to_string(),
            kind: format!("{:?}", symbol.kind),
            signature: symbol.as_signature().map(str::to_string),
            visibility: format!("{:?}", symbol.visibility),
            start_line: symbol.range.start_line + 1,
            end_line: symbol.range.end_line + 1,
        }
    }
}

/// Successful `read_symbol` payload: the exact source span plus enough
/// metadata to identify what was read without a second `find_symbol` call.
#[derive(Debug, Clone, Serialize)]
struct ReadSymbolData {
    location: String,
    kind: String,
    signature: Option<String>,
    visibility: String,
    start_line: u32,
    end_line: u32,
    source: String,
}

/// Outcome of resolving a symbol's source span from disk, including the
/// staleness guard: the on-disk file's SHA256 must match the hash recorded
/// at index time (via `IndexFacade::get_file_hash_for_path`) before a span
/// is trusted, since a changed file can shift line/column offsets out from
/// under a stored `Range`.
enum ReadSymbolOutcome {
    Ok(ReadSymbolData),
    /// The index has no file-info entry for this symbol's file (never
    /// indexed as a tracked file, or the index predates file-info tracking).
    NoFileInfo(String),
    /// The file couldn't be read from disk (moved, deleted, permissions).
    ReadError {
        path: String,
        error: String,
    },
    /// The on-disk content hash no longer matches the indexed hash.
    Stale {
        path: String,
        indexed_hash: String,
        current_hash: String,
    },
}

/// Slice a single source line by byte column, defensively clamping to the
/// line's byte length and falling back to a lossy UTF-8 decode rather than
/// panicking if a column ever lands off a char boundary.
fn slice_line_bytes(line: &str, start: usize, end: Option<usize>) -> String {
    let bytes = line.as_bytes();
    let start = start.min(bytes.len());
    let end = end.unwrap_or(bytes.len()).clamp(start, bytes.len());
    String::from_utf8_lossy(&bytes[start..end]).into_owned()
}

/// Extract the exact source text covered by `range`, slicing by the
/// Range's LINE and COLUMN numbers (never a byte offset into the whole
/// file) — the file is split into lines first, then each boundary line is
/// sliced by its own byte-column.
fn extract_span(content: &str, range: &crate::types::Range) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start_line = range.start_line as usize;
    if lines.is_empty() || start_line >= lines.len() {
        return String::new();
    }
    let end_line = (range.end_line as usize).min(lines.len() - 1);
    if start_line > end_line {
        return String::new();
    }

    if start_line == end_line {
        return slice_line_bytes(
            lines[start_line],
            range.start_column as usize,
            Some(range.end_column as usize),
        );
    }

    let mut result = slice_line_bytes(lines[start_line], range.start_column as usize, None);
    result.push('\n');
    for line in &lines[start_line + 1..end_line] {
        result.push_str(line);
        result.push('\n');
    }
    result.push_str(&slice_line_bytes(
        lines[end_line],
        0,
        Some(range.end_column as usize),
    ));
    result
}

/// Resolve the on-disk path and indexed hash for a symbol's file using only
/// facade lookups (no file I/O). Split out of the former `read_symbol_source`
/// so the caller can release the facade's async read lock before the
/// blocking file I/O in [`read_symbol_span`] runs. `Err` carries the file
/// path (not the full `ReadSymbolOutcome`, to keep the `Result`'s error
/// variant small per `clippy::result_large_err`); the caller wraps it into
/// `ReadSymbolOutcome::NoFileInfo`.
fn resolve_symbol_read_target(
    indexer: &IndexFacade,
    symbol: &Symbol,
) -> Result<(std::path::PathBuf, String), String> {
    let path_str: &str = &symbol.file_path;

    let indexed_hash = match indexer.get_file_hash_for_path(path_str) {
        Some(hash) => hash,
        None => return Err(path_str.to_string()),
    };

    let candidate = std::path::Path::new(path_str);
    let full_path = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        indexer
            .settings()
            .workspace_root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(candidate)
    };

    Ok((full_path, indexed_hash))
}

/// Read a symbol's exact source span from disk, guarded by a staleness
/// check against the indexed file hash (W-3's `get_file_hash_for_path`
/// facade accessor, which delegates to `DocumentIndex::get_file_info` ->
/// `query.rs`; this function never builds its own Tantivy query).
///
/// Pure blocking file I/O + SHA256 hashing over the whole file — callers
/// must run this off the async runtime (`tokio::task::spawn_blocking`),
/// never while holding the facade's async read lock.
fn read_symbol_span(
    full_path: std::path::PathBuf,
    indexed_hash: String,
    symbol: &Symbol,
) -> ReadSymbolOutcome {
    let path_str = symbol.file_path.to_string();

    let content = match std::fs::read_to_string(&full_path) {
        Ok(content) => content,
        Err(e) => {
            return ReadSymbolOutcome::ReadError {
                path: path_str,
                error: e.to_string(),
            };
        }
    };

    let current_hash = crate::indexing::file_info::calculate_hash(&content);
    if current_hash != indexed_hash {
        return ReadSymbolOutcome::Stale {
            path: path_str,
            indexed_hash,
            current_hash,
        };
    }

    ReadSymbolOutcome::Ok(ReadSymbolData {
        location: crate::symbol::context::SymbolContext::symbol_location(symbol),
        kind: format!("{:?}", symbol.kind),
        signature: symbol.as_signature().map(str::to_string),
        visibility: format!("{:?}", symbol.visibility),
        start_line: symbol.range.start_line + 1,
        end_line: symbol.range.end_line + 1,
        source: extract_span(&content, &symbol.range),
    })
}

#[tool_router(router = symbols_router, vis = "pub(crate)")]
impl CodeIntelligenceServer {
    #[tool(description = "Find a symbol by name in the indexed codebase")]
    pub async fn find_symbol(
        &self,
        Parameters(FindSymbolRequest {
            name,
            symbol_id,
            lang,
            output_format,
        }): Parameters<FindSymbolRequest>,
    ) -> Result<CallToolResult, McpError> {
        use crate::symbol::context::ContextIncludes;

        let indexer = self.facade.read().await;

        if output_format == OutputFormat::Json {
            let symbol_contexts = service::find_symbol_data_by_id_or_name(
                &indexer,
                symbol_id,
                &name,
                lang.as_deref(),
            );
            let envelope =
                service::find_symbol_envelope(&indexer, &name, lang.as_deref(), symbol_contexts);
            return Ok(json_result(envelope));
        }

        // Prefer the typed symbol_id when present; otherwise fall back to
        // name-based lookup, including the legacy symbol_id:XXX prefix
        // format (from semantic search results).
        let symbols = if let Some(id) = symbol_id {
            indexer
                .get_symbol(crate::SymbolId(id))
                .map(|s| vec![s])
                .unwrap_or_default()
        } else if let Some(id_str) = name.strip_prefix("symbol_id:") {
            if let Ok(id) = id_str.parse::<u32>() {
                indexer
                    .get_symbol(crate::SymbolId(id))
                    .map(|s| vec![s])
                    .unwrap_or_default()
            } else {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Invalid symbol_id format: {id_str}"
                ))]));
            }
        } else {
            // Shared resolution algorithm; see service::find_symbol_data —
            // the JSON path above (find_symbol) and find_symbols both call
            // the same builder so name resolution cannot drift apart.
            service::find_symbol_data(&indexer, &name, lang.as_deref())
                .into_iter()
                .map(|ctx| ctx.symbol)
                .collect()
        };

        if symbols.is_empty() {
            let mut output = format!("No symbols found with name: {name}");
            // Add guidance for no results
            if let Some(guidance) = generate_mcp_guidance(indexer.settings(), "find_symbol", 0) {
                output.push_str("\n\n---\nGuidance: ");
                output.push_str(&guidance);
                output.push('\n');
            }
            return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
        }

        let mut result = format!("Found {} symbol(s) named '{}':\n\n", symbols.len(), name);

        for (idx, symbol) in symbols.iter().enumerate() {
            if idx > 0 {
                result.push_str("\n---\n\n");
            }

            // Try to get full context with all relationship types
            if let Some(ctx) = indexer.get_symbol_context(symbol.id, ContextIncludes::SYMBOL_CARD) {
                // Header from the name-matched doc, not the id-keyed context:
                // on an index with duplicate symbol_ids the context lookup
                // returns another generation's doc and the row reads crossed.
                result.push_str(&crate::symbol::context::SymbolContext::location_with_type(
                    symbol,
                ));
                result.push('\n');

                // Add module path if available
                if let Some(module) = symbol.as_module_path() {
                    result.push_str(&format!("Module: {module}\n"));
                }

                // Add signature if available
                if let Some(sig) = symbol.as_signature() {
                    result.push_str(&format!("Signature: {sig}\n"));
                }

                // Add documentation preview
                if let Some(doc) = symbol.as_doc_comment() {
                    let doc_preview: Vec<&str> = doc.lines().take(3).collect();
                    let preview = if doc.lines().count() > 3 {
                        format!("{}...", doc_preview.join(" "))
                    } else {
                        doc_preview.join(" ")
                    };
                    result.push_str(&format!("Documentation: {preview}\n"));
                }

                // Add relationship summary
                let mut has_relationships = false;

                // What traits this type implements
                if let Some(impls) = &ctx.relationships.implements {
                    if !impls.is_empty() {
                        result.push_str(&format!("Implements: {} trait(s)\n", impls.len()));
                        for trait_sym in impls.iter().take(5) {
                            result.push_str(&format!(
                                "  -> {} at {}\n",
                                trait_sym.name,
                                crate::symbol::context::SymbolContext::symbol_location(trait_sym)
                            ));
                        }
                        if impls.len() > 5 {
                            result.push_str(&format!("  ... and {} more\n", impls.len() - 5));
                        }
                        has_relationships = true;
                    }
                }

                // What types implement this trait
                if let Some(impls) = &ctx.relationships.implemented_by {
                    if !impls.is_empty() {
                        result.push_str(&format!("Implemented by: {} type(s)\n", impls.len()));
                        for impl_sym in impls.iter().take(5) {
                            result.push_str(&format!(
                                "  <- {} at {}\n",
                                impl_sym.name,
                                crate::symbol::context::SymbolContext::symbol_location(impl_sym)
                            ));
                        }
                        if impls.len() > 5 {
                            result.push_str(&format!("  ... and {} more\n", impls.len() - 5));
                        }
                        has_relationships = true;
                    }
                }

                if let Some(defines) = &ctx.relationships.defines {
                    if !defines.is_empty() {
                        let methods = defines
                            .iter()
                            .filter(|s| s.kind == crate::SymbolKind::Method)
                            .count();
                        if methods > 0 {
                            result.push_str(&format!("Defines: {methods} method(s)\n"));
                            has_relationships = true;
                        }
                    }
                }

                if let Some(callers) = &ctx.relationships.called_by {
                    if !callers.is_empty() {
                        result.push_str(&format!("Called by: {} function(s)\n", callers.len()));
                        has_relationships = true;
                    }
                }

                // What base class(es) this extends
                if let Some(extends) = &ctx.relationships.extends {
                    if !extends.is_empty() {
                        result.push_str(&format!("Extends: {} class(es)\n", extends.len()));
                        for base in extends.iter().take(3) {
                            result.push_str(&format!(
                                "  -> {} at {}\n",
                                base.name,
                                crate::symbol::context::SymbolContext::symbol_location(base)
                            ));
                        }
                        if extends.len() > 3 {
                            result.push_str(&format!("  ... and {} more\n", extends.len() - 3));
                        }
                        has_relationships = true;
                    }
                }

                // What classes extend this
                if let Some(extended_by) = &ctx.relationships.extended_by {
                    if !extended_by.is_empty() {
                        result.push_str(&format!("Extended by: {} class(es)\n", extended_by.len()));
                        for derived in extended_by.iter().take(3) {
                            result.push_str(&format!(
                                "  <- {} at {}\n",
                                derived.name,
                                crate::symbol::context::SymbolContext::symbol_location(derived)
                            ));
                        }
                        if extended_by.len() > 3 {
                            result.push_str(&format!("  ... and {} more\n", extended_by.len() - 3));
                        }
                        has_relationships = true;
                    }
                }

                // What types this symbol uses
                if let Some(uses) = &ctx.relationships.uses {
                    if !uses.is_empty() {
                        result.push_str(&format!("Uses: {} type(s)\n", uses.len()));
                        for used in uses.iter().take(3) {
                            result.push_str(&format!(
                                "  -> {} at {}\n",
                                used.name,
                                crate::symbol::context::SymbolContext::symbol_location(used)
                            ));
                        }
                        if uses.len() > 3 {
                            result.push_str(&format!("  ... and {} more\n", uses.len() - 3));
                        }
                        has_relationships = true;
                    }
                }

                // What symbols use this type
                if let Some(used_by) = &ctx.relationships.used_by {
                    if !used_by.is_empty() {
                        result.push_str(&format!("Used by: {} symbol(s)\n", used_by.len()));
                        has_relationships = true;
                    }
                }

                if !has_relationships && symbol.kind == crate::SymbolKind::Function {
                    result.push_str("No direct callers found\n");
                }
            } else {
                // Fallback to basic info
                result.push_str(&format!(
                    "{:?} at {}:{}\n",
                    symbol.kind,
                    symbol.file_path,
                    symbol.range.start_line + 1
                ));

                if let Some(ref doc) = symbol.doc_comment {
                    let doc_preview: Vec<&str> = doc.lines().take(3).collect();
                    let preview = if doc.lines().count() > 3 {
                        format!("{}...", doc_preview.join(" "))
                    } else {
                        doc_preview.join(" ")
                    };
                    result.push_str(&format!("Documentation: {preview}\n"));
                }

                if let Some(ref sig) = symbol.signature {
                    result.push_str(&format!("Signature: {sig}\n"));
                }
            }
        }

        // Add system guidance
        if let Some(guidance) =
            generate_mcp_guidance(indexer.settings(), "find_symbol", symbols.len())
        {
            result.push_str("\n---\nGuidance: ");
            result.push_str(&guidance);
            result.push('\n');
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    #[tool(
        description = "Find multiple symbols by name in a single batch call (up to 1024 names). Returns a per-name result: found (location, kind, signature, line range), not_found, or ambiguous (candidate list) — never merges same-named symbols."
    )]
    pub async fn find_symbols(
        &self,
        Parameters(FindSymbolsRequest {
            names,
            lang,
            output_format,
        }): Parameters<FindSymbolsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if names.len() > MAX_FIND_SYMBOLS_NAMES {
            let message = format!(
                "Too many names requested for find_symbols: {} (max {MAX_FIND_SYMBOLS_NAMES})",
                names.len()
            );
            if output_format == OutputFormat::Json {
                let envelope: Envelope<()> = Envelope::error(ResultCode::InvalidQuery, message);
                return Ok(json_result(envelope));
            }
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Error: {message}"
            ))]));
        }

        let indexer = self.facade.read().await;

        let results: std::collections::BTreeMap<String, FindSymbolsEntry> = names
            .iter()
            .map(|name| {
                let entry = find_symbols_entry(&indexer, name, lang.as_deref());
                (name.clone(), entry)
            })
            .collect();

        if output_format == OutputFormat::Json {
            let envelope = service::find_symbols_envelope(&indexer, results, lang.as_deref());
            return Ok(json_result(envelope));
        }

        let mut result = format!("find_symbols: {} name(s) queried\n\n", results.len());
        for (name, entry) in &results {
            match entry {
                FindSymbolsEntry::Found {
                    location,
                    kind,
                    signature,
                    line_range,
                } => {
                    result.push_str(&format!(
                        "{name}: {kind} at {location} (lines {}-{})\n",
                        line_range[0], line_range[1]
                    ));
                    if let Some(sig) = signature {
                        result.push_str(&format!("  Signature: {sig}\n"));
                    }
                }
                FindSymbolsEntry::NotFound => {
                    result.push_str(&format!("{name}: not found\n"));
                }
                FindSymbolsEntry::Ambiguous { candidates } => {
                    result.push_str(&format!(
                        "{name}: ambiguous ({} candidates)\n",
                        candidates.len()
                    ));
                    for candidate in candidates {
                        result.push_str(&format!(
                            "  -> {} at {}\n",
                            candidate.kind, candidate.location
                        ));
                    }
                }
            }
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    #[tool(
        description = "Get functions that a given function CALLS (invokes with parentheses).\n\nShows: function_name() → what it calls\nDoes NOT show: Type usage, component rendering, or who calls this function.\n\nUse analyze_impact for: Type dependencies, component usage (JSX), or reverse lookups."
    )]
    pub async fn get_calls(
        &self,
        Parameters(GetCallsRequest {
            name,
            symbol_id,
            output_format,
        }): Parameters<GetCallsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let indexer = self.facade.read().await;

        if output_format == OutputFormat::Json {
            let identifier = service::identifier_for(symbol_id, &name);

            return Ok(match service::get_calls_data(&indexer, symbol_id, name) {
                RelationOutcome::Data(calls) => json_result(service::get_calls_success_envelope(
                    &indexer,
                    &identifier,
                    calls,
                )),
                RelationOutcome::NotFound => {
                    json_result(service::get_calls_not_found_envelope(&indexer, &identifier))
                }
                RelationOutcome::Ambiguous { name, candidates } => {
                    json_result(ambiguous_envelope(EntityType::Calls, &name, candidates))
                }
                RelationOutcome::MissingParam => {
                    json_result(Envelope::<Vec<service::CallRelation>>::error(
                        ResultCode::InvalidQuery,
                        "get_calls requires either 'name' or 'symbol_id' parameter",
                    ))
                }
            });
        }

        // Resolution policy is shared with the CLI JSON path via the
        // service layer; text renderings stay byte-identical.
        let (symbol, identifier) = match service::resolve_symbol_or_id(&indexer, symbol_id, name) {
            SymbolResolution::Resolved { symbol, identifier } => (symbol, identifier),
            SymbolResolution::NotFoundById(id) => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Symbol not found: symbol_id:{id}"
                ))]));
            }
            SymbolResolution::NotFoundByName(name) => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Function not found: {name}"
                ))]));
            }
            SymbolResolution::Ambiguous { name, candidates } => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(
                    render_ambiguity("get_calls", &name, &candidates),
                )]));
            }
            SymbolResolution::MissingParam => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(
                    "Error: Either name or symbol_id must be provided".to_string(),
                )]));
            }
        };

        // Get calls for this specific symbol
        let all_called_with_metadata = indexer.get_called_functions_with_metadata(symbol.id);

        if all_called_with_metadata.is_empty() {
            let mut output = format!("{identifier} doesn't call any functions");
            // Add guidance for no results
            if let Some(guidance) = generate_mcp_guidance(indexer.settings(), "get_calls", 0) {
                output.push_str("\n\n---\nGuidance: ");
                output.push_str(&guidance);
                output.push('\n');
            }
            return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
        }

        let result_count = all_called_with_metadata.len();
        let mut result = format!("{identifier} calls {result_count} function(s):\n");
        for (callee, metadata) in all_called_with_metadata {
            // Parse metadata to extract receiver info and call site location
            let (call_display, call_line) = if let Some(ref meta) = metadata {
                let display = meta
                    .context
                    .as_deref()
                    .and_then(parse_receiver_context)
                    .map(|(receiver, is_static)| qualified_call(receiver, is_static, &callee.name))
                    .unwrap_or_else(|| callee.name.to_string());

                // Use call site line if available, otherwise definition line
                let line = meta
                    .line
                    .map(|l| l + 1)
                    .unwrap_or(callee.range.start_line + 1);
                (display, line)
            } else {
                (callee.name.to_string(), callee.range.start_line + 1)
            };

            result.push_str(&format!(
                "  -> {:?} {} at {}:{}\n",
                callee.kind, call_display, callee.file_path, call_line
            ));
            if let Some(ref sig) = callee.signature {
                result.push_str(&format!("     Signature: {sig}\n"));
            }
        }

        // Add system guidance
        if let Some(guidance) = generate_mcp_guidance(indexer.settings(), "get_calls", result_count)
        {
            result.push_str("\n---\nGuidance: ");
            result.push_str(&guidance);
            result.push('\n');
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    #[tool(
        description = "Find functions that CALL a given function (invoke it with parentheses).\n\nShows: what calls → function_name()\nDoes NOT show: Type references, component rendering, or what this function calls.\n\nUse analyze_impact for: Complete dependency graph including type usage and composition."
    )]
    pub async fn find_callers(
        &self,
        Parameters(FindCallersRequest {
            name,
            symbol_id,
            filter,
            count_only,
            output_format,
        }): Parameters<FindCallersRequest>,
    ) -> Result<CallToolResult, McpError> {
        let indexer = self.facade.read().await;
        let test_path_patterns = &indexer.settings().caller_classification.test_path_patterns;

        if output_format == OutputFormat::Json {
            let identifier = service::identifier_for(symbol_id, &name);

            return Ok(
                match service::find_callers_data(&indexer, symbol_id, name, test_path_patterns) {
                    RelationOutcome::Data(unfiltered) => {
                        if count_only {
                            // Per-role breakdown is always computed over the
                            // UNFILTERED caller set: `filter` narrows the
                            // returned listing, never the counted breakdown.
                            json_result(service::find_callers_counts_envelope(
                                &indexer,
                                &identifier,
                                &unfiltered,
                            ))
                        } else {
                            let filtered = service::filter_callers(unfiltered, filter);
                            json_result(service::find_callers_list_envelope(
                                &indexer,
                                &identifier,
                                filtered,
                            ))
                        }
                    }
                    RelationOutcome::NotFound => json_result(
                        service::find_callers_not_found_envelope(&indexer, &identifier),
                    ),
                    RelationOutcome::Ambiguous { name, candidates } => {
                        json_result(ambiguous_envelope(EntityType::Callers, &name, candidates))
                    }
                    RelationOutcome::MissingParam => {
                        json_result(Envelope::<Vec<service::CallerRelation>>::error(
                            ResultCode::InvalidQuery,
                            "find_callers requires either 'name' or 'symbol_id' parameter",
                        ))
                    }
                },
            );
        }

        // Shared resolution policy; see service.rs.
        let (symbol, identifier) = match service::resolve_symbol_or_id(&indexer, symbol_id, name) {
            SymbolResolution::Resolved { symbol, identifier } => (symbol, identifier),
            SymbolResolution::NotFoundById(id) => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Symbol not found: symbol_id:{id}"
                ))]));
            }
            SymbolResolution::NotFoundByName(name) => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Function not found: {name}"
                ))]));
            }
            SymbolResolution::Ambiguous { name, candidates } => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(
                    render_ambiguity("find_callers", &name, &candidates),
                )]));
            }
            SymbolResolution::MissingParam => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(
                    "Error: Either name or symbol_id must be provided".to_string(),
                )]));
            }
        };

        // Get callers for THIS SPECIFIC symbol only (no aggregation), tag
        // each with its production/test role. Filtering and per-role
        // counting go through the same shared predicates the JSON path
        // uses (`service::role_passes_filter`/`service::count_roles`), so
        // the two cannot drift (§BASIC.2); `CallerRelation` itself isn't
        // reused here since it drops the call-site `context` this renderer
        // needs for the "(calls receiver.method)" qualifier.
        let all_callers_with_metadata = indexer.get_calling_functions_with_metadata(symbol.id);
        let all_tagged: Vec<_> = all_callers_with_metadata
            .into_iter()
            .map(|(caller, metadata)| {
                let role = service::classify_caller_role(&caller.file_path, test_path_patterns);
                (caller, metadata, role)
            })
            .collect();

        if count_only {
            // Per-role breakdown is always computed over the UNFILTERED
            // caller set: `filter` narrows the returned listing, never the
            // counted breakdown.
            let counts = service::count_roles(all_tagged.iter().map(|(_, _, role)| *role));
            if counts.total == 0 {
                let mut output = format!("No functions call {identifier}");
                if let Some(guidance) = generate_mcp_guidance(indexer.settings(), "find_callers", 0)
                {
                    output.push_str("\n\n---\nGuidance: ");
                    output.push_str(&guidance);
                    output.push('\n');
                }
                return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
            }
            let mut result = format!(
                "{} function(s) call {identifier} ({} production, {} test)\n",
                counts.total, counts.production, counts.test
            );
            if let Some(guidance) =
                generate_mcp_guidance(indexer.settings(), "find_callers", counts.total)
            {
                result.push_str("\n---\nGuidance: ");
                result.push_str(&guidance);
                result.push('\n');
            }
            return Ok(CallToolResult::success(vec![ContentBlock::text(result)]));
        }

        let tagged_callers: Vec<_> = all_tagged
            .into_iter()
            .filter(|(_, _, role)| service::role_passes_filter(*role, filter))
            .collect();

        if tagged_callers.is_empty() {
            let mut output = format!("No functions call {identifier}");
            // Add guidance for no results
            if let Some(guidance) = generate_mcp_guidance(indexer.settings(), "find_callers", 0) {
                output.push_str("\n\n---\nGuidance: ");
                output.push_str(&guidance);
                output.push('\n');
            }
            return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
        }

        let result_count = tagged_callers.len();

        // Build structured text response with rich metadata
        let mut result = format!("{result_count} function(s) call {identifier}:\n");

        for (caller, metadata, role) in tagged_callers {
            // Parse metadata to extract receiver info and call site location
            let (call_info, call_line) = if let Some(ref meta) = metadata {
                let info = meta
                    .context
                    .as_deref()
                    .and_then(parse_receiver_context)
                    .map(|(receiver, is_static)| {
                        format!(
                            " (calls {})",
                            qualified_call(receiver, is_static, &symbol.name)
                        )
                    })
                    .unwrap_or_default();

                // Use call site line if available, otherwise definition line
                let line = meta
                    .line
                    .map(|l| l + 1)
                    .unwrap_or(caller.range.start_line + 1);
                (info, line)
            } else {
                (String::new(), caller.range.start_line + 1)
            };

            let role_label = match role {
                service::CallerRole::Production => "production",
                service::CallerRole::Test => "test",
            };

            result.push_str(&format!(
                "  <- {:?} {} at {}:{}{} [{}]\n",
                caller.kind, caller.name, caller.file_path, call_line, call_info, role_label
            ));

            if let Some(ref sig) = caller.signature {
                result.push_str(&format!("     Signature: {sig}\n"));
            }
        }

        // Add system guidance
        if let Some(guidance) =
            generate_mcp_guidance(indexer.settings(), "find_callers", result_count)
        {
            result.push_str("\n---\nGuidance: ");
            result.push_str(&guidance);
            result.push('\n');
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    #[tool(
        description = "Analyze complete impact of changing a symbol. Shows ALL relationships: function calls, type usage, composition.\n\nShows:\n- What CALLS this function\n- What USES this as a type (fields, parameters, returns)\n- What RENDERS/COMPOSES this (JSX: <Component>, Rust: struct fields, etc.)\n- Full dependency graph across files\n\nUse this when: You need to see everything that depends on a symbol."
    )]
    pub async fn analyze_impact(
        &self,
        Parameters(AnalyzeImpactRequest {
            name,
            symbol_id,
            max_depth,
            count_only,
            max_results,
            group_by,
            output_format,
        }): Parameters<AnalyzeImpactRequest>,
    ) -> Result<CallToolResult, McpError> {
        use crate::symbol::context::ContextIncludes;

        let indexer = self.facade.read().await;

        if output_format == OutputFormat::Json {
            let identifier = service::identifier_for(symbol_id, &name);

            return Ok(
                match service::analyze_impact_data(&indexer, symbol_id, name, max_depth as usize) {
                    RelationOutcome::Data(impacted) => {
                        if count_only {
                            json_result(service::analyze_impact_counts_envelope(
                                &indexer,
                                &identifier,
                                max_depth,
                                &impacted,
                            ))
                        } else {
                            json_result(service::analyze_impact_listing_envelope(
                                &indexer,
                                &identifier,
                                max_depth,
                                impacted,
                                group_by,
                                max_results,
                            ))
                        }
                    }
                    RelationOutcome::NotFound => json_result(
                        service::analyze_impact_not_found_envelope(&indexer, &identifier),
                    ),
                    RelationOutcome::Ambiguous { name, candidates } => json_result(
                        ambiguous_envelope(EntityType::ImpactGraph, &name, candidates),
                    ),
                    RelationOutcome::MissingParam => json_result(Envelope::<Vec<Symbol>>::error(
                        ResultCode::InvalidQuery,
                        "analyze_impact requires either 'name' or 'symbol_id' parameter",
                    )),
                },
            );
        }

        // Shared resolution policy; see service.rs.
        let (symbol, identifier) = match service::resolve_symbol_or_id(&indexer, symbol_id, name) {
            SymbolResolution::Resolved { symbol, identifier } => (symbol, identifier),
            SymbolResolution::NotFoundById(id) => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Symbol not found: symbol_id:{id}"
                ))]));
            }
            SymbolResolution::NotFoundByName(name) => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Symbol not found: {name}"
                ))]));
            }
            SymbolResolution::Ambiguous { name, candidates } => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(
                    render_ambiguity("analyze_impact", &name, &candidates),
                )]));
            }
            SymbolResolution::MissingParam => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(
                    "Error: Either name or symbol_id must be provided".to_string(),
                )]));
            }
        };

        // Analyze impact for THIS SPECIFIC symbol only (no aggregation)
        let impacted = indexer.get_impact_radius(symbol.id, Some(max_depth as usize));

        if impacted.is_empty() {
            let mut output = format!("No symbols would be impacted by changing {identifier}");
            // Add guidance for no results
            if let Some(guidance) = generate_mcp_guidance(indexer.settings(), "analyze_impact", 0) {
                output.push_str("\n\n---\nGuidance: ");
                output.push_str(&guidance);
                output.push('\n');
            }
            return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
        }

        let mut result = format!("Analyzing impact of changing: {identifier}\n");

        // Show the specific symbol being analyzed
        if let Some(ctx) = indexer.get_symbol_context(
            symbol.id,
            ContextIncludes::CALLERS | ContextIncludes::EXTENDS | ContextIncludes::USES,
        ) {
            // Name-matched doc, not the id-keyed context (see find_symbol).
            let location = crate::symbol::context::SymbolContext::location(&symbol);
            let direct_callers = ctx
                .relationships
                .called_by
                .as_ref()
                .map(|c| c.len())
                .unwrap_or(0);

            // For classes, also show inheritance info
            let inheritance_info = if matches!(
                symbol.kind,
                crate::SymbolKind::Class | crate::SymbolKind::Struct
            ) {
                let extends_count = ctx
                    .relationships
                    .extends
                    .as_ref()
                    .map(|e| e.len())
                    .unwrap_or(0);
                let extended_by_count = ctx
                    .relationships
                    .extended_by
                    .as_ref()
                    .map(|e| e.len())
                    .unwrap_or(0);

                if extends_count > 0 || extended_by_count > 0 {
                    format!(", extends: {extends_count}, extended by: {extended_by_count}")
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            // Show uses info for all symbols
            let uses_count = ctx
                .relationships
                .uses
                .as_ref()
                .map(|u| u.len())
                .unwrap_or(0);
            let used_by_count = ctx
                .relationships
                .used_by
                .as_ref()
                .map(|u| u.len())
                .unwrap_or(0);

            let uses_info = if uses_count > 0 || used_by_count > 0 {
                format!(", uses: {uses_count}, used by: {used_by_count}")
            } else {
                String::new()
            };

            result.push_str(&format!(
                "Symbol: {:?} at {} (direct callers: {}{}{})\n\n",
                symbol.kind, location, direct_callers, inheritance_info, uses_info
            ));
        }

        let resolved: Vec<Symbol> = impacted
            .into_iter()
            .filter_map(|id| indexer.get_symbol(id))
            .collect();
        let impact_count = resolved.len();
        result.push_str(&format!(
            "Total impact: {impact_count} symbol(s) would be affected (max depth: {max_depth})\n"
        ));

        if count_only {
            let counts = service::count_impact(&resolved);
            result.push_str(&format!("Distinct files affected: {}\n", counts.files));
        } else {
            // Apply `group_by` ordering FIRST, then `max_results`
            // truncation — the same order the JSON path applies via
            // `service::group_and_truncate_impact` — so text and JSON
            // truncate the identical subset instead of the text path
            // truncating the raw BFS order before grouping.
            let (listing, truncated) =
                service::group_and_truncate_impact(resolved, group_by, max_results);

            // Group by symbol kind (default) or by file
            let sections = service::group_impact_sections(listing, group_by);

            // Display grouped sections with locations
            for (label, symbols) in sections {
                result.push_str(&format!("\n{label} ({}): \n", symbols.len()));
                for sym in symbols {
                    result.push_str(&format!(
                        "  - {} at {}:{}\n",
                        sym.name,
                        sym.file_path,
                        sym.range.start_line + 1
                    ));
                }
            }

            if truncated {
                result.push_str(&format!(
                    "\n(truncated to {max_results} of {impact_count} symbol(s))\n"
                ));
            }
        }

        // Add system guidance
        if let Some(guidance) =
            generate_mcp_guidance(indexer.settings(), "analyze_impact", impact_count)
        {
            result.push_str("\n---\nGuidance: ");
            result.push_str(&guidance);
            result.push('\n');
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    #[tool(
        description = "List all symbols defined in an indexed file: name, kind, signature, visibility, and start-end line range for each. Use to get a structural overview of a file before reading it in full."
    )]
    pub async fn get_file_outline(
        &self,
        Parameters(GetFileOutlineRequest {
            path,
            max_results,
            output_format,
        }): Parameters<GetFileOutlineRequest>,
    ) -> Result<CallToolResult, McpError> {
        let indexer = self.facade.read().await;
        let file_id = indexer.get_file_id_for_path(&path);

        let hint_fn = |count: usize| {
            generate_guidance_from_config(
                &indexer.settings().guidance,
                "get_file_outline",
                Some(path.as_str()),
                count,
            )
        };

        if output_format == OutputFormat::Json {
            return Ok(match file_id {
                None => {
                    let mut envelope: Envelope<Vec<FileOutlineEntry>> =
                        Envelope::not_found(format!("File '{path}' not found in index"))
                            .with_entity_type(EntityType::Symbol)
                            .with_query(&path);
                    if let Some(hint) = hint_fn(0) {
                        envelope = envelope.with_hint(hint);
                    }
                    json_result(envelope)
                }
                Some(file_id) => {
                    let mut entries: Vec<FileOutlineEntry> = indexer
                        .get_symbols_by_file(file_id)
                        .iter()
                        .map(FileOutlineEntry::from_symbol)
                        .collect();
                    entries.sort_by_key(|e| e.start_line);
                    let total = entries.len();
                    let truncated = max_results > 0 && (max_results as usize) < entries.len();
                    if truncated {
                        entries.truncate(max_results as usize);
                    }
                    let count = entries.len();
                    let mut envelope = Envelope::success(entries)
                        .with_entity_type(EntityType::Symbol)
                        .with_count(total)
                        .with_query(&path)
                        .with_message(format!("{total} symbol(s) in {path}"));
                    if truncated {
                        envelope = envelope.with_truncated(true);
                    }
                    if let Some(hint) = hint_fn(count) {
                        envelope = envelope.with_hint(hint);
                    }
                    json_result(envelope)
                }
            });
        }

        let file_id = match file_id {
            Some(id) => id,
            None => {
                let mut output = format!("File not found in index: {path}");
                if let Some(guidance) =
                    generate_mcp_guidance(indexer.settings(), "get_file_outline", 0)
                {
                    output.push_str("\n\n---\nGuidance: ");
                    output.push_str(&guidance);
                    output.push('\n');
                }
                return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
            }
        };

        let mut symbols = indexer.get_symbols_by_file(file_id);
        symbols.sort_by_key(|s| s.range.start_line);

        if symbols.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "No symbols found in {path}"
            ))]));
        }

        let symbol_count = symbols.len();
        let truncated = max_results > 0 && (max_results as usize) < symbols.len();
        if truncated {
            symbols.truncate(max_results as usize);
        }
        let mut result = format!("Outline for {path}: {symbol_count} symbol(s)\n\n");
        for symbol in &symbols {
            result.push_str(&format!(
                "{:?} {} ({:?}) lines {}-{}\n",
                symbol.kind,
                symbol.name,
                symbol.visibility,
                symbol.range.start_line + 1,
                symbol.range.end_line + 1
            ));
            if let Some(sig) = symbol.as_signature() {
                result.push_str(&format!("  Signature: {sig}\n"));
            }
        }

        if truncated {
            result.push_str(&format!(
                "\n(truncated to {max_results} of {symbol_count} symbol(s))\n"
            ));
        }

        if let Some(guidance) =
            generate_mcp_guidance(indexer.settings(), "get_file_outline", symbol_count)
        {
            result.push_str("\n---\nGuidance: ");
            result.push_str(&guidance);
            result.push('\n');
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    #[tool(
        description = "Read a symbol's exact source span (sliced by line/column, not byte offset), plus kind/signature/visibility metadata. Refuses to return a span if the file has changed on disk since indexing (staleness guard via SHA256 hash comparison) since the recorded line/column range may no longer match the current file."
    )]
    pub async fn read_symbol(
        &self,
        Parameters(ReadSymbolRequest {
            name,
            symbol_id,
            output_format,
        }): Parameters<ReadSymbolRequest>,
    ) -> Result<CallToolResult, McpError> {
        let indexer = self.facade.read().await;

        // Shared resolution policy; see service.rs. Ambiguity refuses and
        // lists rather than picking or merging, matching get_calls/
        // find_callers/analyze_impact.
        let (symbol, identifier) = match service::resolve_symbol_or_id(&indexer, symbol_id, name) {
            SymbolResolution::Resolved { symbol, identifier } => (symbol, identifier),
            SymbolResolution::NotFoundById(id) => {
                let message = format!("Symbol not found: symbol_id:{id}");
                return Ok(if output_format == OutputFormat::Json {
                    json_result(
                        Envelope::<()>::not_found(message)
                            .with_entity_type(EntityType::Symbol)
                            .with_query(format!("symbol_id:{id}")),
                    )
                } else {
                    CallToolResult::success(vec![ContentBlock::text(message)])
                });
            }
            SymbolResolution::NotFoundByName(name) => {
                let message = format!("Symbol not found: {name}");
                return Ok(if output_format == OutputFormat::Json {
                    json_result(
                        Envelope::<()>::not_found(message)
                            .with_entity_type(EntityType::Symbol)
                            .with_query(&name),
                    )
                } else {
                    CallToolResult::success(vec![ContentBlock::text(format!(
                        "Symbol not found: {name}"
                    ))])
                });
            }
            SymbolResolution::Ambiguous { name, candidates } => {
                return Ok(if output_format == OutputFormat::Json {
                    json_result(ambiguous_envelope(EntityType::Symbol, &name, candidates))
                } else {
                    CallToolResult::success(vec![ContentBlock::text(render_ambiguity(
                        "read_symbol",
                        &name,
                        &candidates,
                    ))])
                });
            }
            SymbolResolution::MissingParam => {
                return Ok(if output_format == OutputFormat::Json {
                    json_result(Envelope::<()>::error(
                        ResultCode::InvalidQuery,
                        "read_symbol requires either 'name' or 'symbol_id' parameter",
                    ))
                } else {
                    CallToolResult::success(vec![ContentBlock::text(
                        "Error: Either name or symbol_id must be provided".to_string(),
                    )])
                });
            }
        };

        // Resolve the read target from the facade, then release the async
        // read lock before the blocking file I/O + SHA256 hashing runs on a
        // blocking-pool thread instead of the async runtime.
        let read_target = resolve_symbol_read_target(&indexer, &symbol);
        drop(indexer);

        let file_path_for_errors = symbol.file_path.to_string();
        let read_outcome = match read_target {
            Ok((full_path, indexed_hash)) => tokio::task::spawn_blocking(move || {
                read_symbol_span(full_path, indexed_hash, &symbol)
            })
            .await
            .unwrap_or_else(|e| ReadSymbolOutcome::ReadError {
                path: file_path_for_errors,
                error: format!("blocking task panicked: {e}"),
            }),
            Err(path) => ReadSymbolOutcome::NoFileInfo(path),
        };

        Ok(match read_outcome {
            ReadSymbolOutcome::Ok(data) => {
                if output_format == OutputFormat::Json {
                    json_result(
                        Envelope::success(data)
                            .with_entity_type(EntityType::Symbol)
                            .with_query(&identifier)
                            .with_message("Read symbol source"),
                    )
                } else {
                    let mut result = format!(
                        "{} at {} (lines {}-{})\n",
                        data.kind, data.location, data.start_line, data.end_line
                    );
                    if let Some(sig) = &data.signature {
                        result.push_str(&format!("Signature: {sig}\n"));
                    }
                    result.push_str(&format!("Visibility: {}\n\n", data.visibility));
                    result.push_str(&data.source);
                    result.push('\n');
                    CallToolResult::success(vec![ContentBlock::text(result)])
                }
            }
            ReadSymbolOutcome::NoFileInfo(path) => {
                let message = format!(
                    "No indexed file-info for '{path}'; reindex before read_symbol can verify freshness"
                );
                if output_format == OutputFormat::Json {
                    json_result(
                        Envelope::<()>::error(ResultCode::IndexError, message)
                            .with_entity_type(EntityType::Symbol)
                            .with_query(&identifier),
                    )
                } else {
                    CallToolResult::error(vec![ContentBlock::text(message)])
                }
            }
            ReadSymbolOutcome::ReadError { path, error } => {
                let message = format!("Failed to read '{path}' from disk: {error}");
                if output_format == OutputFormat::Json {
                    json_result(
                        Envelope::<()>::error(ResultCode::IndexError, message)
                            .with_entity_type(EntityType::Symbol)
                            .with_query(&identifier),
                    )
                } else {
                    CallToolResult::error(vec![ContentBlock::text(message)])
                }
            }
            ReadSymbolOutcome::Stale {
                path,
                indexed_hash,
                current_hash,
            } => {
                let message = format!(
                    "STALE_INDEX: '{path}' has changed on disk since indexing (indexed hash {indexed_hash}, current hash {current_hash}) — the requested span may no longer match the source. Reindex before reading this symbol."
                );
                if output_format == OutputFormat::Json {
                    json_result(
                        Envelope::<()>::error(ResultCode::IndexError, message)
                            .with_entity_type(EntityType::Symbol)
                            .with_query(&identifier),
                    )
                } else {
                    CallToolResult::error(vec![ContentBlock::text(message)])
                }
            }
        })
    }
}
