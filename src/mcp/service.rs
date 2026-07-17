//! Shared query layer for MCP handlers and CLI JSON output.
//!
//! One resolution policy and one receiver-metadata codec, consumed by both
//! renderings (MCP text, CLI JSON envelopes) so the two cannot drift. The
//! ambiguity policy is refuse-and-list, never aggregate: relationships from
//! same-named but unrelated symbols must not merge into one result.
//!
//! The `*_data` builder functions below are the single source of the typed
//! JSON payload for each tool: both the CLI's `--json` path
//! (`cli/commands/mcp.rs`) and the MCP tools' `output_format: json` path
//! (`mcp/tools/*.rs`) call these instead of independently re-deriving the
//! same facade queries, so the two renderings cannot drift apart (§BASIC.2).

use crate::Symbol;
use crate::indexing::facade::IndexFacade;
use crate::io::envelope::{EntityType, Envelope};
use crate::io::guidance_engine::generate_guidance_from_config;
use crate::mcp::requests::{CallerFilter, GroupBy};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;

/// Render a JSON [`Envelope`] as a single-block tool result. Falls back to
/// serializing a fallback `Envelope<()>` error if the primary envelope
/// fails to serialize (keeping the `output_format: json` contract of always
/// returning a parseable `Envelope<T>` shape), and only as an absolute last
/// resort — if even the fallback envelope fails to serialize — emits a raw
/// text error block. Shared by every MCP tool's json branch
/// (`mcp/tools/*.rs`) so the fallback behavior cannot drift per file.
pub fn json_result<T: Serialize>(envelope: Envelope<T>) -> CallToolResult {
    let text = serde_json::to_string(&envelope).unwrap_or_else(|e| {
        let fallback = Envelope::<()>::error(
            crate::io::envelope::ResultCode::InternalError,
            format!("envelope serialization failed: {e}"),
        );
        serde_json::to_string(&fallback).unwrap_or_else(|_| {
            format!(r#"{{"type":"error","message":"envelope serialization failed: {e}"}}"#)
        })
    });
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Outcome of resolving a tool's target symbol from `symbol_id` or name.
pub enum SymbolResolution {
    Resolved {
        symbol: Symbol,
        /// Display identifier: the queried name, or `symbol_id:<id>`.
        identifier: String,
    },
    NotFoundById(u32),
    NotFoundByName(String),
    Ambiguous {
        name: String,
        candidates: Vec<Symbol>,
    },
    MissingParam,
}

/// Resolve a target symbol by `symbol_id` (unambiguous) or by name.
/// More than one name match is `Ambiguous` — callers present the candidate
/// list instead of picking or merging.
pub fn resolve_symbol_or_id(
    facade: &IndexFacade,
    symbol_id: Option<u32>,
    name: Option<String>,
) -> SymbolResolution {
    if let Some(id) = symbol_id {
        match facade.get_symbol(crate::SymbolId(id)) {
            Some(symbol) => SymbolResolution::Resolved {
                symbol,
                identifier: format!("symbol_id:{id}"),
            },
            None => SymbolResolution::NotFoundById(id),
        }
    } else if let Some(name) = name {
        let mut symbols = facade.find_symbols_by_name(&name, None);
        if symbols.is_empty() {
            symbols = find_dotted_members(&name, |n| facade.find_symbols_by_name(n, None));
        }
        match symbols.len() {
            0 => SymbolResolution::NotFoundByName(name),
            1 => SymbolResolution::Resolved {
                symbol: symbols.pop().expect("len checked"),
                identifier: name,
            },
            _ => SymbolResolution::Ambiguous {
                name,
                candidates: symbols,
            },
        }
    } else {
        SymbolResolution::MissingParam
    }
}

/// Class-scoped fallback for dotted queries: "Class.method" resolves the
/// method within the named type when no symbol matches the literal name.
/// Uniform across languages; `find` supplies name candidates (typically a
/// `find_symbols_by_name` closure so language filters carry through).
pub fn find_dotted_members(name: &str, find: impl Fn(&str) -> Vec<Symbol>) -> Vec<Symbol> {
    let Some((class, member)) = name.rsplit_once('.') else {
        return Vec::new();
    };
    if class.is_empty() || member.is_empty() {
        return Vec::new();
    }
    find(member)
        .into_iter()
        .filter(|sym| is_member_of(sym, class))
        .collect()
}

/// Whether `sym` is a member of type `class`: ClassMember scope with a
/// matching class name (rightmost segment matches for nested classes), or
/// a member-kind symbol whose module_path ends in the type for languages
/// that record the containing type there (mirrors the
/// `is_receiver_compatible` default). The kind bound keeps the vocabulary
/// at Type.member: without it, module-scoped queries like
/// "components.Button" resolve by accident of the suffix predicate.
fn is_member_of(sym: &Symbol, class: &str) -> bool {
    if let Some(crate::symbol::ScopeContext::ClassMember {
        class_name: Some(c),
    }) = &sym.scope_context
    {
        if c.as_ref() == class || c.rsplit('.').next() == Some(class) {
            return true;
        }
    }
    matches!(
        sym.kind,
        crate::SymbolKind::Method | crate::SymbolKind::Field | crate::SymbolKind::Constant
    ) && sym.module_path.as_deref().is_some_and(|mp| {
        mp.strip_suffix(class)
            .is_some_and(|rest| rest.ends_with("::") || rest.ends_with('.'))
    })
}

/// Text rendering of the ambiguity listing. `tool` appears in the trailing
/// usage hint; output must stay byte-identical across the three handlers.
pub fn render_ambiguity(tool: &str, name: &str, candidates: &[Symbol]) -> String {
    let mut msg = format!(
        "Ambiguous: found {} symbol(s) named '{}':\n",
        candidates.len(),
        name
    );
    for (i, sym) in candidates.iter().take(10).enumerate() {
        msg.push_str(&format!(
            "  {}. symbol_id:{} - {:?} at {}:{}\n",
            i + 1,
            sym.id.value(),
            sym.kind,
            sym.file_path,
            sym.range.start_line + 1
        ));
    }
    if candidates.len() > 10 {
        msg.push_str(&format!("  ... and {} more\n", candidates.len() - 10));
    }
    msg.push_str(&format!("\nUse: {tool} symbol_id:<id> for specific symbol"));
    msg
}

/// Parse the `receiver:{r},static:{s}` relationship context written by the
/// parsers. Returns `None` when the context lacks the pattern or the
/// receiver is empty.
pub fn parse_receiver_context(context: &str) -> Option<(&str, bool)> {
    if !(context.contains("receiver:") && context.contains("static:")) {
        return None;
    }
    let mut receiver = "";
    let mut is_static = false;
    for part in context.split(',') {
        if let Some(r) = part.strip_prefix("receiver:") {
            receiver = r;
        } else if let Some(s) = part.strip_prefix("static:") {
            is_static = s == "true";
        }
    }
    if receiver.is_empty() {
        None
    } else {
        Some((receiver, is_static))
    }
}

/// `Receiver::method` for static calls, `receiver.method` for instance calls.
pub fn qualified_call(receiver: &str, is_static: bool, name: &str) -> String {
    if is_static {
        format!("{receiver}::{name}")
    } else {
        format!("{receiver}.{name}")
    }
}

// =============================================================================
// Shared JSON data-payload types and builders
// =============================================================================
//
// One struct/function per tool's data shape, reused by both the CLI JSON
// path and the MCP `output_format: json` path. Text rendering is untouched
// and stays byte-identical; these are additive.

/// Outcome of a data-payload builder that resolves a target symbol via
/// [`resolve_symbol_or_id`] (get_calls, find_callers, analyze_impact).
pub enum RelationOutcome<T> {
    Data(T),
    NotFound,
    Ambiguous {
        name: String,
        candidates: Vec<Symbol>,
    },
    MissingParam,
}

/// Outcome of a search-shaped data-payload builder (search_symbols,
/// semantic_search_docs, semantic_search_with_context).
pub enum SearchOutcome<T> {
    Data(Vec<T>),
    /// Malformed query input (e.g. an unknown symbol kind filter).
    InvalidQuery(String),
    /// Backend/search failure, including "semantic search is not enabled".
    Error(String),
}

/// Flattened call/caller info combining symbol with call site metadata.
/// Avoids tuple waste like `[[symbol, null], ...]` in JSON output.
#[derive(Debug, Clone, Serialize)]
pub struct CallRelation {
    #[serde(flatten)]
    pub symbol: Symbol,
    /// Line number of the call site (1-indexed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_line: Option<u32>,
    /// Column of the call site
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_column: Option<u16>,
}

/// Source-role tag for a `find_callers` result: whether the calling
/// symbol's file looks like test code or production code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallerRole {
    Production,
    Test,
}

/// Path-heuristic classifier for `find_callers`: tags a caller's role by
/// matching `file_path` against the configured
/// `caller_classification.test_path_patterns` (W-2, `Settings`). No schema
/// change — reads the `Symbol.file_path` already present on every caller
/// result.
///
/// Directory-shaped patterns (containing `/`, e.g. `"tests/"`, `"/test/"`,
/// `"__tests__/"`) match as a substring anywhere in the full path.
/// Glob-shaped patterns (containing `*`, e.g. `"*_test.*"`, `"test_*.py"`,
/// `"*.spec.*"`) match against the file's basename with a simple `*`
/// wildcard (no `?`/character classes — a trait/abstraction here would be
/// unwarranted; see §BASIC.5/YAGNI).
pub fn classify_caller_role(file_path: &str, test_path_patterns: &[String]) -> CallerRole {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    let is_test = test_path_patterns.iter().any(|pattern| {
        if pattern.contains('*') {
            glob_match(pattern, file_name)
        } else {
            file_path.contains(pattern.as_str())
        }
    });
    if is_test {
        CallerRole::Test
    } else {
        CallerRole::Production
    }
}

/// Minimal `*`-only wildcard matcher (no `?`, no character classes) backing
/// [`classify_caller_role`]'s glob-shaped patterns.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut match_i = 0usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            match_i = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            match_i += 1;
            ti = match_i;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// `find_callers` result entry: call relation plus the computed
/// [`CallerRole`] tag.
#[derive(Debug, Clone, Serialize)]
pub struct CallerRelation {
    #[serde(flatten)]
    pub call: CallRelation,
    pub role: CallerRole,
}

/// Whether a caller's role passes a [`CallerFilter`]. `All` always passes.
/// The single boolean predicate backing both [`filter_callers`] (which
/// operates on the full [`CallerRelation`] JSON payload) and the MCP
/// text-mode `find_callers` renderer, which retains call-site metadata
/// (`context`) that `CallerRelation` does not carry and therefore cannot go
/// through `filter_callers` directly — both call sites share this predicate
/// instead of re-deriving the `match filter { .. }` arms (§BASIC.2).
pub fn role_passes_filter(role: CallerRole, filter: CallerFilter) -> bool {
    match filter {
        CallerFilter::All => true,
        CallerFilter::Production => role == CallerRole::Production,
        CallerFilter::Test => role == CallerRole::Test,
    }
}

/// Apply a [`CallerFilter`] to a resolved caller list. `All` is a no-op.
pub fn filter_callers(callers: Vec<CallerRelation>, filter: CallerFilter) -> Vec<CallerRelation> {
    callers
        .into_iter()
        .filter(|c| role_passes_filter(c.role, filter))
        .collect()
}

/// `count_only` envelope data: totals with a per-role breakdown.
/// `production + test == total` always holds by construction, since both
/// are counted from the same list.
#[derive(Debug, Clone, Serialize)]
pub struct CallerCounts {
    pub total: usize,
    pub production: usize,
    pub test: usize,
}

/// Build the per-role breakdown from any iterator of roles. Backs
/// [`count_callers_by_role`] and the MCP text-mode `find_callers` renderer
/// (see [`role_passes_filter`] for why the latter can't route through
/// `CallerRelation`-typed data directly). Callers decide whether to pass a
/// filtered or unfiltered sequence; `find_callers`'s `count_only` path
/// always passes the unfiltered set so `filter` narrows the listing, never
/// the counted breakdown.
pub fn count_roles(roles: impl Iterator<Item = CallerRole>) -> CallerCounts {
    let mut total = 0usize;
    let mut production = 0usize;
    let mut test = 0usize;
    for role in roles {
        total += 1;
        match role {
            CallerRole::Production => production += 1,
            CallerRole::Test => test += 1,
        }
    }
    CallerCounts {
        total,
        production,
        test,
    }
}

/// Build the per-role breakdown for a caller list (typically the
/// UNFILTERED set — see [`count_roles`]).
pub fn count_callers_by_role(callers: &[CallerRelation]) -> CallerCounts {
    count_roles(callers.iter().map(|c| c.role))
}

/// Build full symbol-card contexts for an already-resolved symbol list.
/// Shared tail of [`find_symbol_data`] and [`find_symbol_data_by_id_or_name`]
/// so the context-building fallback (missing relationship-index entry ->
/// bare context from the facade's file path) lives in one place.
fn symbols_to_contexts(
    facade: &IndexFacade,
    symbols: Vec<Symbol>,
) -> Vec<crate::symbol::context::SymbolContext> {
    use crate::symbol::context::ContextIncludes;
    let mut results = Vec::new();
    for symbol in symbols {
        let context = facade.get_symbol_context(symbol.id, ContextIncludes::SYMBOL_CARD);
        if let Some(ctx) = context {
            results.push(ctx);
        } else {
            let file_path = facade
                .get_file_path(symbol.file_id)
                .unwrap_or_else(|| "unknown".to_string());
            results.push(crate::symbol::context::SymbolContext {
                symbol,
                file_path,
                relationships: Default::default(),
            });
        }
    }
    results
}

/// Build the `find_symbol` JSON data payload: full symbol-card context for
/// every match (including the dotted-member fallback). An empty result
/// means "not found"; `find_symbol` treats multiple matches as ordinary
/// success data, never ambiguity.
pub fn find_symbol_data(
    facade: &IndexFacade,
    name: &str,
    lang: Option<&str>,
) -> Vec<crate::symbol::context::SymbolContext> {
    let mut symbols = facade.find_symbols_by_name(name, lang);
    if symbols.is_empty() {
        symbols = find_dotted_members(name, |n| facade.find_symbols_by_name(n, lang));
    }
    if symbols.is_empty() {
        return Vec::new();
    }
    symbols_to_contexts(facade, symbols)
}

/// Build the `find_symbol` JSON data payload, resolving by `symbol_id` when
/// present (mirroring the text-mode path's typed-id preference) and falling
/// back to name-based resolution (via [`find_symbol_data`]) otherwise. Keeps
/// the JSON and text renderings consistent: `find_symbol(symbol_id:...)`
/// must resolve identically in both output modes.
pub fn find_symbol_data_by_id_or_name(
    facade: &IndexFacade,
    symbol_id: Option<u32>,
    name: &str,
    lang: Option<&str>,
) -> Vec<crate::symbol::context::SymbolContext> {
    if let Some(id) = symbol_id {
        let symbols = facade
            .get_symbol(crate::SymbolId(id))
            .map(|s| vec![s])
            .unwrap_or_default();
        return symbols_to_contexts(facade, symbols);
    }
    find_symbol_data(facade, name, lang)
}

/// Build the `get_calls` JSON data payload.
pub fn get_calls_data(
    facade: &IndexFacade,
    symbol_id: Option<u32>,
    name: Option<String>,
) -> RelationOutcome<Vec<CallRelation>> {
    use crate::symbol::context::ContextIncludes;
    match resolve_symbol_or_id(facade, symbol_id, name) {
        SymbolResolution::Resolved { symbol, .. } => {
            let mut all_calls = Vec::new();
            if let Some(ctx) = facade.get_symbol_context(symbol.id, ContextIncludes::CALLS)
                && let Some(calls) = ctx.relationships.calls
            {
                for (called, metadata) in calls {
                    all_calls.push(CallRelation {
                        symbol: called,
                        call_line: metadata.as_ref().and_then(|m| m.line).map(|l| l + 1),
                        call_column: metadata.as_ref().and_then(|m| m.column),
                    });
                }
            }
            RelationOutcome::Data(all_calls)
        }
        SymbolResolution::NotFoundById(_) | SymbolResolution::NotFoundByName(_) => {
            RelationOutcome::NotFound
        }
        SymbolResolution::Ambiguous { name, candidates } => {
            RelationOutcome::Ambiguous { name, candidates }
        }
        SymbolResolution::MissingParam => RelationOutcome::MissingParam,
    }
}

/// Build the `find_callers` JSON data payload. Every caller is tagged with
/// its [`CallerRole`] via [`classify_caller_role`], computed against
/// `test_path_patterns` (typically `Settings.caller_classification`); the
/// unfiltered, untagged list is returned — callers apply
/// [`filter_callers`]/[`count_callers_by_role`] as needed.
pub fn find_callers_data(
    facade: &IndexFacade,
    symbol_id: Option<u32>,
    name: Option<String>,
    test_path_patterns: &[String],
) -> RelationOutcome<Vec<CallerRelation>> {
    match resolve_symbol_or_id(facade, symbol_id, name) {
        SymbolResolution::Resolved { symbol, .. } => {
            let callers = facade.get_calling_functions_with_metadata(symbol.id);
            let all_callers: Vec<_> = callers
                .into_iter()
                .map(|(caller, metadata)| {
                    let role = classify_caller_role(&caller.file_path, test_path_patterns);
                    CallerRelation {
                        call: CallRelation {
                            symbol: caller,
                            call_line: metadata.as_ref().and_then(|m| m.line).map(|l| l + 1),
                            call_column: metadata.as_ref().and_then(|m| m.column),
                        },
                        role,
                    }
                })
                .collect();
            RelationOutcome::Data(all_callers)
        }
        SymbolResolution::NotFoundById(_) | SymbolResolution::NotFoundByName(_) => {
            RelationOutcome::NotFound
        }
        SymbolResolution::Ambiguous { name, candidates } => {
            RelationOutcome::Ambiguous { name, candidates }
        }
        SymbolResolution::MissingParam => RelationOutcome::MissingParam,
    }
}

/// Build the `analyze_impact` JSON data payload.
pub fn analyze_impact_data(
    facade: &IndexFacade,
    symbol_id: Option<u32>,
    name: Option<String>,
    max_depth: usize,
) -> RelationOutcome<Vec<Symbol>> {
    match resolve_symbol_or_id(facade, symbol_id, name) {
        SymbolResolution::Resolved { symbol, .. } => {
            let impacted_ids = facade.get_impact_radius(symbol.id, Some(max_depth));
            let mut impacted_symbols = Vec::new();
            for impact_id in impacted_ids {
                if let Some(sym) = facade.get_symbol(impact_id) {
                    impacted_symbols.push(sym);
                }
            }
            RelationOutcome::Data(impacted_symbols)
        }
        SymbolResolution::NotFoundById(_) | SymbolResolution::NotFoundByName(_) => {
            RelationOutcome::NotFound
        }
        SymbolResolution::Ambiguous { name, candidates } => {
            RelationOutcome::Ambiguous { name, candidates }
        }
        SymbolResolution::MissingParam => RelationOutcome::MissingParam,
    }
}

/// Symbol info extracted from a search result for a consistent JSON shape.
/// Matches the nested `symbol: {...}` pattern used by semantic_search_docs.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolInfo {
    pub id: crate::types::SymbolId,
    pub name: String,
    pub kind: crate::types::SymbolKind,
    pub file_path: String,
    pub line: u32,
    pub column: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub module_path: String,
}

/// Search result with nested symbol for consistent JSON output.
/// Standardizes on `symbol: {...}` rather than flat `symbol_id: ...`.
#[derive(Debug, Clone, Serialize)]
pub struct SearchSymbolResult {
    pub symbol: SymbolInfo,
    pub score: f32,
    pub highlights: Vec<crate::storage::tantivy::TextHighlight>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

impl From<crate::storage::tantivy::SearchResult> for SearchSymbolResult {
    fn from(sr: crate::storage::tantivy::SearchResult) -> Self {
        Self {
            symbol: SymbolInfo {
                id: sr.symbol_id,
                name: sr.name,
                kind: sr.kind,
                file_path: sr.file_path,
                line: sr.line,
                column: sr.column,
                doc_comment: sr.doc_comment,
                signature: sr.signature,
                module_path: sr.module_path,
            },
            score: sr.score,
            highlights: sr.highlights,
            context: sr.context,
        }
    }
}

/// Build the `search_symbols` JSON data payload.
pub fn search_symbols_data(
    facade: &IndexFacade,
    query: &str,
    limit: usize,
    kind: Option<&str>,
    module: Option<&str>,
    lang: Option<&str>,
) -> SearchOutcome<SearchSymbolResult> {
    let kind_filter = match kind.map(str::parse::<crate::SymbolKind>) {
        None => None,
        Some(Ok(k)) => Some(k),
        Some(Err(e)) => return SearchOutcome::InvalidQuery(e.to_string()),
    };

    match facade.search(query, limit, kind_filter, module, lang) {
        Ok(results) => SearchOutcome::Data(results.into_iter().map(Into::into).collect()),
        Err(e) => SearchOutcome::Error(e.to_string()),
    }
}

/// A semantic search hit (symbol + similarity score).
#[derive(Debug, Clone, Serialize)]
pub struct SemanticSearchResult {
    pub symbol: Symbol,
    pub score: f32,
}

/// Context without the symbol (avoids duplication since symbol is at the
/// top level of [`SemanticSearchWithContextResult`]).
#[derive(Debug, Clone, Serialize)]
pub struct ContextWithoutSymbol {
    pub file_path: String,
    pub relationships: crate::symbol::context::SymbolRelationships,
}

/// A `semantic_search_with_context` hit: symbol, score, and full relationship context.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticSearchWithContextResult {
    pub symbol: Symbol,
    pub score: f32,
    pub context: ContextWithoutSymbol,
}

/// Build the `semantic_search_docs` JSON data payload.
pub fn semantic_search_docs_data(
    facade: &IndexFacade,
    query: &str,
    limit: usize,
    threshold: Option<f32>,
    lang: Option<&str>,
) -> SearchOutcome<SemanticSearchResult> {
    if !facade.has_semantic_search() {
        return SearchOutcome::Error("Semantic search is not enabled".to_string());
    }

    let results = match threshold {
        Some(t) => facade.semantic_search_docs_with_threshold_and_language(query, limit, t, lang),
        None => facade.semantic_search_docs_with_language(query, limit, lang),
    };

    match results {
        Ok(results) => SearchOutcome::Data(
            results
                .into_iter()
                .map(|(symbol, score)| SemanticSearchResult { symbol, score })
                .collect(),
        ),
        Err(e) => SearchOutcome::Error(e.to_string()),
    }
}

/// Build the `semantic_search_with_context` JSON data payload.
pub fn semantic_search_with_context_data(
    facade: &IndexFacade,
    query: &str,
    limit: usize,
    threshold: Option<f32>,
    lang: Option<&str>,
) -> SearchOutcome<SemanticSearchWithContextResult> {
    if !facade.has_semantic_search() {
        return SearchOutcome::Error("Semantic search is not enabled".to_string());
    }

    let search_results = match threshold {
        Some(t) => facade.semantic_search_docs_with_threshold_and_language(query, limit, t, lang),
        None => facade.semantic_search_docs_with_language(query, limit, lang),
    };

    match search_results {
        Ok(results) => {
            use crate::symbol::context::ContextIncludes;
            let context_results: Vec<SemanticSearchWithContextResult> = results
                .into_iter()
                .filter_map(|(symbol, score)| {
                    let context = facade.get_symbol_context(
                        symbol.id,
                        ContextIncludes::SYMBOL_CARD | ContextIncludes::CALLS,
                    );
                    context.map(|ctx| SemanticSearchWithContextResult {
                        symbol,
                        score,
                        context: ContextWithoutSymbol {
                            file_path: ctx.file_path,
                            relationships: ctx.relationships,
                        },
                    })
                })
                .collect();
            SearchOutcome::Data(context_results)
        }
        Err(e) => SearchOutcome::Error(e.to_string()),
    }
}

/// Build the `search_documents` JSON data payload.
///
/// Search-only: collection auto-sync is the caller's responsibility (see the
/// two `search_documents` tool call sites in `mcp/tools/search.rs`), which
/// run it under a brief write guard scoped to the sync loop only, dropped
/// before calling this function. `DocumentStore::search` only needs `&self`
/// (vector reads go through `ConcurrentVectorStorage`'s interior locking),
/// so callers can hold only a read guard on the document store here,
/// letting concurrent `search_documents` calls make progress against each
/// other instead of serializing behind one write guard for sync + search.
pub fn search_documents_data(
    store: &crate::documents::DocumentStore,
    settings: &crate::config::Settings,
    query: &str,
    collection: Option<String>,
    limit: usize,
) -> crate::documents::store::StoreResult<Vec<crate::documents::SearchResult>> {
    let search_query = crate::documents::SearchQuery {
        text: query.to_string(),
        collection,
        document: None,
        limit,
        preview_config: Some(settings.documents.search.clone()),
    };

    store.search(search_query)
}

/// Symbol-kind counts shown by `get_index_info`.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolKindBreakdown {
    pub functions: usize,
    pub methods: usize,
    pub structs: usize,
    pub traits: usize,
}

/// Semantic search status/metadata shown by `get_index_info`.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticSearchInfo {
    pub enabled: bool,
    pub model_name: Option<String>,
    pub embeddings: Option<usize>,
    pub dimensions: Option<usize>,
    pub created: Option<String>,
    pub updated: Option<String>,
}

/// The `get_index_info` JSON data payload.
#[derive(Debug, Clone, Serialize)]
pub struct IndexInfo {
    pub symbol_count: usize,
    pub file_count: usize,
    pub relationship_count: usize,
    pub symbol_kinds: SymbolKindBreakdown,
    pub semantic_search: SemanticSearchInfo,
    /// Whether the ignore-rule inputs (`.codannaignore`, `.gitignore`,
    /// `.git/info/exclude`, `indexing.ignore_patterns`,
    /// `indexing.follow_links`) have changed since the index was last
    /// built. `None` means unknown -- either the index predates this field,
    /// or the fingerprint could not be recomputed -- and must never be
    /// reported as `Some(true)` ("changed"). Detect-and-report only: this
    /// does not trigger reindexing or reconciliation (issue #28).
    pub ignore_rules_changed: Option<bool>,
}

/// Compares the ignore-rule fingerprint stored at the last index build
/// against one computed fresh from the facade's current settings/ignore
/// files. Returns `None` (unknown) rather than `Some(true)` (changed) when
/// metadata predates this field or the fingerprint cannot be recomputed --
/// see [`IndexInfo::ignore_rules_changed`].
pub(crate) fn ignore_rules_changed(facade: &IndexFacade) -> Option<bool> {
    let metadata = crate::storage::IndexMetadata::load(facade.index_base()).ok()?;
    let stored_fingerprint = metadata.ignore_fingerprint?;

    let settings = facade.settings();
    let root = settings
        .workspace_root
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("."));
    let current_fingerprint =
        crate::indexing::walk_config::ignore_fingerprint(settings, root).ok()?;

    Some(current_fingerprint != stored_fingerprint)
}

/// Build the `get_index_info` JSON data payload.
pub fn index_info_data(facade: &IndexFacade) -> IndexInfo {
    let symbol_count = facade.symbol_count();
    let file_count = facade.file_count();
    let relationship_count = facade.relationship_count();

    let mut kind_counts = std::collections::HashMap::new();
    for symbol in facade.get_all_symbols() {
        *kind_counts.entry(symbol.kind).or_insert(0) += 1;
    }

    let functions = *kind_counts.get(&crate::SymbolKind::Function).unwrap_or(&0);
    let methods = *kind_counts.get(&crate::SymbolKind::Method).unwrap_or(&0);
    let structs = *kind_counts.get(&crate::SymbolKind::Struct).unwrap_or(&0);
    let traits = *kind_counts.get(&crate::SymbolKind::Trait).unwrap_or(&0);

    let semantic_search = if let Some(metadata) = facade.get_semantic_metadata() {
        SemanticSearchInfo {
            enabled: true,
            model_name: Some(metadata.model_name),
            embeddings: Some(metadata.embedding_count),
            dimensions: Some(metadata.dimension),
            created: Some(crate::mcp::format_relative_time(metadata.created_at)),
            updated: Some(crate::mcp::format_relative_time(metadata.updated_at)),
        }
    } else {
        SemanticSearchInfo {
            enabled: false,
            model_name: None,
            embeddings: None,
            dimensions: None,
            created: None,
            updated: None,
        }
    };

    IndexInfo {
        symbol_count,
        file_count: file_count as usize,
        relationship_count,
        symbol_kinds: SymbolKindBreakdown {
            functions,
            methods,
            structs,
            traits,
        },
        semantic_search,
        ignore_rules_changed: ignore_rules_changed(facade),
    }
}

// =============================================================================
// Shared envelope builders
// =============================================================================
//
// One builder per tool's `output_format: json` envelope (status + message +
// hint), reused by both the CLI's `--json` path (`cli/commands/mcp.rs`) and
// the MCP tools' json branch (`mcp/tools/*.rs`) so the message/hint text and
// status mapping cannot drift apart (§BASIC.2). Callers that need to special
// case ambiguity (the CLI exits the process; the MCP tools return an inline
// `Ambiguous`-status envelope) keep their own dispatch around
// [`resolve_symbol_or_id`]/[`RelationOutcome`] and call these builders only
// for the resolved-data and not-found cases, which share identical shape.

/// Build the `Ambiguous`-status envelope shared by get_calls, find_callers,
/// and analyze_impact's `output_format: json` path, and by the CLI's
/// `--json` `exit_ambiguous` (`cli/commands/mcp.rs`). Refuse-and-list, never
/// merge: relationships from same-named but unrelated symbols must not
/// merge into one result. Single source of the status/code/exit_code
/// mapping so the CLI and MCP tool paths cannot drift apart (§BASIC.2).
pub fn ambiguous_envelope(
    entity: EntityType,
    name: &str,
    candidates: Vec<Symbol>,
) -> Envelope<Vec<Symbol>> {
    let count = candidates.len();
    Envelope::ambiguous(
        format!("Ambiguous: found {count} symbol(s) named '{name}'"),
        Some(candidates),
    )
    .with_entity_type(entity)
    .with_query(name)
    .with_count(count)
    .with_hint("Ambiguous name: re-run with symbol_id:<id> using a candidate from data")
}

/// Resolve the display identifier shared by get_calls/find_callers/
/// analyze_impact envelopes: `symbol_id:<id>` when an id was supplied,
/// otherwise the queried name, defaulting to `"unknown"`.
pub fn identifier_for(symbol_id: Option<u32>, name: &Option<String>) -> String {
    symbol_id
        .map(|id| format!("symbol_id:{id}"))
        .or_else(|| name.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Build the `find_symbol` envelope: success (possibly multiple matches,
/// including the dotted-member fallback) or not-found. `find_symbol` never
/// treats multiple matches as ambiguous, so this is a standalone builder.
pub fn find_symbol_envelope(
    facade: &IndexFacade,
    name: &str,
    lang: Option<&str>,
    symbol_contexts: Vec<crate::symbol::context::SymbolContext>,
) -> Envelope<Vec<crate::symbol::context::SymbolContext>> {
    let count = symbol_contexts.len();
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "find_symbol",
        Some(name),
        count,
    );
    let mut envelope = if symbol_contexts.is_empty() {
        Envelope::not_found(format!("Symbol '{name}' not found"))
            .with_entity_type(EntityType::Symbol)
            .with_query(name)
    } else {
        Envelope::success(symbol_contexts)
            .with_entity_type(EntityType::Symbol)
            .with_count(count)
            .with_query(name)
            .with_message(format!("Found {count} symbol(s)"))
    };
    if let Some(lang) = lang {
        envelope = envelope.with_lang(lang);
    }
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `find_symbols` batch envelope from a per-name resolution map.
pub(crate) fn find_symbols_envelope(
    facade: &IndexFacade,
    results: std::collections::BTreeMap<String, crate::mcp::tools::symbols::FindSymbolsEntry>,
    lang: Option<&str>,
) -> Envelope<std::collections::BTreeMap<String, crate::mcp::tools::symbols::FindSymbolsEntry>> {
    let count = results.len();
    let found = results
        .values()
        .filter(|e| {
            matches!(
                e,
                crate::mcp::tools::symbols::FindSymbolsEntry::Found { .. }
            )
        })
        .count();
    let hint =
        generate_guidance_from_config(&facade.settings().guidance, "find_symbols", None, found);
    let mut envelope = Envelope::success(results)
        .with_entity_type(EntityType::Symbol)
        .with_count(count)
        .with_message(format!("Resolved {found}/{count} name(s)"));
    if let Some(lang) = lang {
        envelope = envelope.with_lang(lang);
    }
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `get_calls` success envelope.
pub fn get_calls_success_envelope(
    facade: &IndexFacade,
    identifier: &str,
    calls: Vec<CallRelation>,
) -> Envelope<Vec<CallRelation>> {
    let count = calls.len();
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "get_calls",
        Some(identifier),
        count,
    );
    let mut envelope = Envelope::success(calls)
        .with_entity_type(EntityType::Calls)
        .with_count(count)
        .with_query(identifier)
        .with_message(format!("Calls {count} function(s)"));
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `get_calls` not-found envelope.
pub fn get_calls_not_found_envelope(
    facade: &IndexFacade,
    identifier: &str,
) -> Envelope<Vec<CallRelation>> {
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "get_calls",
        Some(identifier),
        0,
    );
    let mut envelope: Envelope<Vec<CallRelation>> =
        Envelope::not_found(format!("Function '{identifier}' not found"))
            .with_entity_type(EntityType::Calls)
            .with_query(identifier);
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `find_callers` `count_only` envelope. The per-role breakdown is
/// computed over the UNFILTERED caller set (`unfiltered`) regardless of any
/// `filter` the caller applied to the listing elsewhere: `filter` narrows
/// the returned *listing*, never the counted breakdown, so
/// `filter:production count_only:true` still reports the true `test` count
/// rather than zeroing it out. `total` is likewise the unfiltered total.
pub fn find_callers_counts_envelope(
    facade: &IndexFacade,
    identifier: &str,
    unfiltered: &[CallerRelation],
) -> Envelope<CallerCounts> {
    let counts = count_callers_by_role(unfiltered);
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "find_callers",
        Some(identifier),
        counts.total,
    );
    let mut envelope = Envelope::success(counts.clone())
        .with_entity_type(EntityType::Callers)
        .with_count(counts.total)
        .with_query(identifier)
        .with_message(format!("Called by {} function(s)", counts.total));
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `find_callers` listing envelope from an already-`filter`-narrowed list.
pub fn find_callers_list_envelope(
    facade: &IndexFacade,
    identifier: &str,
    filtered: Vec<CallerRelation>,
) -> Envelope<Vec<CallerRelation>> {
    let count = filtered.len();
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "find_callers",
        Some(identifier),
        count,
    );
    let mut envelope = Envelope::success(filtered)
        .with_entity_type(EntityType::Callers)
        .with_count(count)
        .with_query(identifier)
        .with_message(format!("Called by {count} function(s)"));
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `find_callers` not-found envelope.
pub fn find_callers_not_found_envelope(
    facade: &IndexFacade,
    identifier: &str,
) -> Envelope<Vec<CallerRelation>> {
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "find_callers",
        Some(identifier),
        0,
    );
    let mut envelope: Envelope<Vec<CallerRelation>> =
        Envelope::not_found(format!("Function '{identifier}' not found"))
            .with_entity_type(EntityType::Callers)
            .with_query(identifier);
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// `analyze_impact` `count_only` envelope data: total symbol count plus the
/// number of distinct files spanned by the impact radius.
#[derive(Debug, Clone, Serialize)]
pub struct ImpactCounts {
    pub total: usize,
    pub files: usize,
}

/// Build the `count_only` breakdown for an impact-radius symbol list.
pub fn count_impact(impacted: &[Symbol]) -> ImpactCounts {
    let files: std::collections::HashSet<&str> =
        impacted.iter().map(|s| s.file_path.as_ref()).collect();
    ImpactCounts {
        total: impacted.len(),
        files: files.len(),
    }
}

/// Reorder an impact-radius listing per [`GroupBy`]. `Kind` (the default)
/// is a no-op — it preserves the exact BFS order `get_impact_radius`
/// already returns, matching pre-`group_by` behavior byte-for-byte. `File`
/// stably regroups the same symbols by `file_path`.
pub fn group_impact(impacted: Vec<Symbol>, group_by: GroupBy) -> Vec<Symbol> {
    match group_by {
        GroupBy::Kind => impacted,
        GroupBy::File => {
            let mut grouped = impacted;
            grouped.sort_by(|a, b| a.file_path.cmp(&b.file_path));
            grouped
        }
    }
}

/// Partition an impact-radius listing into labeled display sections for the
/// text-mode renderer, per [`GroupBy`]. `Kind` groups by `Symbol.kind`
/// (the pre-`group_by` text-mode behavior); `File` groups by
/// `Symbol.file_path` instead. A `BTreeMap` (rather than a `HashMap`) keeps
/// section iteration order deterministic — sorted by the section label —
/// so `group_by: file` text output is ordered by `file_path` like the JSON
/// listing, instead of an arbitrary hash-bucket order.
pub fn group_impact_sections(
    impacted: Vec<Symbol>,
    group_by: GroupBy,
) -> std::collections::BTreeMap<String, Vec<Symbol>> {
    let mut sections: std::collections::BTreeMap<String, Vec<Symbol>> =
        std::collections::BTreeMap::new();
    for sym in impacted {
        let key = match group_by {
            GroupBy::Kind => format!("{:?}", sym.kind),
            GroupBy::File => sym.file_path.to_string(),
        };
        sections.entry(key).or_default().push(sym);
    }
    sections
}

/// Apply `group_by` ordering FIRST, then `max_results` truncation, in that
/// order — so JSON and text renderings truncate the identical subset
/// (previously the JSON path grouped-then-truncated while the text path
/// truncated the raw BFS order before grouping, producing different
/// symbols for the same request). Returns the resulting listing and
/// whether truncation occurred.
pub fn group_and_truncate_impact(
    impacted: Vec<Symbol>,
    group_by: GroupBy,
    max_results: u32,
) -> (Vec<Symbol>, bool) {
    let mut listing = group_impact(impacted, group_by);
    let truncated = max_results > 0 && (max_results as usize) < listing.len();
    if truncated {
        listing.truncate(max_results as usize);
    }
    (listing, truncated)
}

/// Build the `analyze_impact` `count_only` envelope.
pub fn analyze_impact_counts_envelope(
    facade: &IndexFacade,
    identifier: &str,
    max_depth: u32,
    impacted: &[Symbol],
) -> Envelope<ImpactCounts> {
    let total = impacted.len();
    let counts = count_impact(impacted);
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "analyze_impact",
        Some(identifier),
        total,
    );
    let mut envelope = Envelope::success(counts)
        .with_entity_type(EntityType::ImpactGraph)
        .with_count(total)
        .with_query(identifier)
        .with_depth(max_depth)
        .with_message(format!("{total} symbol(s) would be impacted"));
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `analyze_impact` listing envelope. Applies
/// [`group_and_truncate_impact`] internally so callers pass the raw
/// impact-radius listing and never re-derive the group-then-truncate order.
pub fn analyze_impact_listing_envelope(
    facade: &IndexFacade,
    identifier: &str,
    max_depth: u32,
    impacted: Vec<Symbol>,
    group_by: GroupBy,
    max_results: u32,
) -> Envelope<Vec<Symbol>> {
    let total = impacted.len();
    let (listing, truncated) = group_and_truncate_impact(impacted, group_by, max_results);
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "analyze_impact",
        Some(identifier),
        total,
    );
    let mut envelope = Envelope::success(listing)
        .with_entity_type(EntityType::ImpactGraph)
        .with_count(total)
        .with_query(identifier)
        .with_depth(max_depth)
        .with_message(format!("{total} symbol(s) would be impacted"));
    if truncated {
        envelope = envelope.with_truncated(true);
    }
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `analyze_impact` not-found envelope.
pub fn analyze_impact_not_found_envelope(
    facade: &IndexFacade,
    identifier: &str,
) -> Envelope<Vec<Symbol>> {
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "analyze_impact",
        Some(identifier),
        0,
    );
    let mut envelope: Envelope<Vec<Symbol>> =
        Envelope::not_found(format!("Symbol '{identifier}' not found"))
            .with_entity_type(EntityType::ImpactGraph)
            .with_query(identifier);
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `search_symbols` envelope. Empty results render as `not_found`.
pub fn search_symbols_envelope(
    facade: &IndexFacade,
    query: &str,
    lang: Option<&str>,
    results: Vec<SearchSymbolResult>,
) -> Envelope<Vec<SearchSymbolResult>> {
    let count = results.len();
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "search_symbols",
        Some(query),
        count,
    );
    let mut envelope = if count == 0 {
        Envelope::<Vec<SearchSymbolResult>>::not_found(format!("No symbols found for '{query}'"))
            .with_entity_type(EntityType::SearchResult)
            .with_query(query)
    } else {
        Envelope::success(results)
            .with_entity_type(EntityType::SearchResult)
            .with_count(count)
            .with_query(query)
            .with_message(format!("Found {count} symbol(s)"))
    };
    if let Some(lang) = lang {
        envelope = envelope.with_lang(lang);
    }
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `semantic_search_docs` envelope. Empty results render as
/// `not_found`; canonical message text (previously drifted between the CLI
/// JSON path and the MCP tool's own json branch — see §BASIC.2).
pub fn semantic_search_docs_envelope(
    facade: &IndexFacade,
    query: &str,
    lang: Option<&str>,
    results: Vec<SemanticSearchResult>,
) -> Envelope<Vec<SemanticSearchResult>> {
    let count = results.len();
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "semantic_search_docs",
        Some(query),
        count,
    );
    let mut envelope = if count == 0 {
        Envelope::<Vec<SemanticSearchResult>>::not_found(format!(
            "No semantically similar documentation found for '{query}'"
        ))
        .with_entity_type(EntityType::Symbol)
        .with_query(query)
    } else {
        Envelope::success(results)
            .with_entity_type(EntityType::Symbol)
            .with_count(count)
            .with_query(query)
            .with_message(format!("Found {count} similar symbol(s)"))
    };
    if let Some(lang) = lang {
        envelope = envelope.with_lang(lang);
    }
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the error envelope for a `semantic_search_docs`/
/// `semantic_search_with_context` backend failure (including "semantic
/// search is not enabled").
pub fn semantic_search_error_envelope(query: &str, message: impl Into<String>) -> Envelope<()> {
    Envelope::error(crate::io::envelope::ResultCode::IndexError, message)
        .with_entity_type(EntityType::Symbol)
        .with_query(query)
        .with_hint("Enable semantic search in settings.toml and rebuild the index")
}

/// Build the `semantic_search_with_context` envelope. Empty results render
/// as `not_found`.
pub fn semantic_search_with_context_envelope(
    facade: &IndexFacade,
    query: &str,
    lang: Option<&str>,
    results: Vec<SemanticSearchWithContextResult>,
) -> Envelope<Vec<SemanticSearchWithContextResult>> {
    let count = results.len();
    let hint = generate_guidance_from_config(
        &facade.settings().guidance,
        "semantic_search_with_context",
        Some(query),
        count,
    );
    let mut envelope = if count == 0 {
        Envelope::<Vec<SemanticSearchWithContextResult>>::not_found(format!(
            "No similar symbols found for '{query}'"
        ))
        .with_entity_type(EntityType::Symbol)
        .with_query(query)
    } else {
        Envelope::success(results)
            .with_entity_type(EntityType::Symbol)
            .with_count(count)
            .with_query(query)
            .with_message(format!("Found {count} symbol(s) with context"))
    };
    if let Some(lang) = lang {
        envelope = envelope.with_lang(lang);
    }
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// Build the `get_index_info` envelope.
pub fn index_info_envelope(facade: &IndexFacade) -> Envelope<IndexInfo> {
    let info = index_info_data(facade);
    let hint =
        generate_guidance_from_config(&facade.settings().guidance, "get_index_info", None, 1);
    let mut envelope = Envelope::success(info).with_message("Index statistics");
    if let Some(hint) = hint {
        envelope = envelope.with_hint(hint);
    }
    envelope
}

/// `reindex` envelope data: files reindexed, symbols, and elapsed time.
/// Shared by the `reindex` MCP tool's `output_format: json` path and the
/// CLI's `--json` `reindex` path so the two cannot drift (§BASIC.2, gap #6a).
#[derive(Debug, Clone, Serialize)]
pub struct ReindexInfo {
    pub reindexed: usize,
    pub symbols: usize,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documents: Option<crate::mcp::server::DocReindexTotals>,
}

/// Build the `reindex` envelope data payload from a [`crate::mcp::server::ReindexRunOutcome`].
pub(crate) fn reindex_info_data(outcome: &crate::mcp::server::ReindexRunOutcome) -> ReindexInfo {
    ReindexInfo {
        reindexed: outcome.reindexed,
        symbols: outcome.symbols,
        duration_ms: outcome.duration_ms,
        documents: outcome.documents,
    }
}

/// Build the `reindex` envelope.
pub(crate) fn reindex_envelope(
    outcome: &crate::mcp::server::ReindexRunOutcome,
) -> Envelope<ReindexInfo> {
    Envelope::success(reindex_info_data(outcome)).with_message("Reindex complete")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiver_context_parses_both_forms() {
        assert_eq!(
            parse_receiver_context("receiver:Foo,static:true"),
            Some(("Foo", true))
        );
        assert_eq!(
            parse_receiver_context("receiver:bar,static:false"),
            Some(("bar", false))
        );
        assert_eq!(parse_receiver_context("receiver:,static:true"), None);
        assert_eq!(parse_receiver_context("unrelated context"), None);
    }

    #[test]
    fn qualified_call_separator_follows_static_flag() {
        assert_eq!(qualified_call("Foo", true, "new"), "Foo::new");
        assert_eq!(qualified_call("foo", false, "run"), "foo.run");
    }

    fn method_symbol(id: u32, name: &str, class: Option<&str>, module_path: &str) -> Symbol {
        let mut sym = Symbol::new(
            crate::SymbolId::new(id).unwrap(),
            name,
            crate::SymbolKind::Method,
            crate::FileId::new(1).unwrap(),
            crate::Range::new(1, 0, 1, 10),
        );
        sym.scope_context = Some(crate::symbol::ScopeContext::ClassMember {
            class_name: class.map(Into::into),
        });
        sym.module_path = Some(module_path.into());
        sym
    }

    #[test]
    fn dotted_lookup_filters_by_class_member() {
        let a = method_symbol(1, "model_dump", Some("BaseModel"), "pydantic.main");
        let b = method_symbol(2, "model_dump", Some("RootModel"), "pydantic.root_model");
        let found = find_dotted_members("BaseModel.model_dump", |n| {
            if n == "model_dump" {
                vec![a.clone(), b.clone()]
            } else {
                vec![]
            }
        });
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, a.id);
    }

    #[test]
    fn dotted_lookup_matches_module_path_suffix() {
        // Languages recording the containing type via module_path
        let mut sym = method_symbol(1, "new", None, "crate::types::RawSymbol");
        sym.scope_context = None;
        let found = find_dotted_members("RawSymbol.new", |n| {
            if n == "new" {
                vec![sym.clone()]
            } else {
                vec![]
            }
        });
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn dotted_lookup_rejects_module_scoped_symbols() {
        // "components.Button" where Button is a class in module
        // src.components: module-scoped queries are not Type.member
        // vocabulary and stay NOT_FOUND.
        let mut sym = method_symbol(1, "Button", None, "src.components");
        sym.scope_context = None;
        sym.kind = crate::SymbolKind::Class;
        let found = find_dotted_members("components.Button", |n| {
            if n == "Button" {
                vec![sym.clone()]
            } else {
                vec![]
            }
        });
        assert!(found.is_empty());
    }

    #[test]
    fn dotted_lookup_ignores_undotted_and_empty_segments() {
        assert!(find_dotted_members("plain", |_| unreachable!("no dot, no lookup")).is_empty());
        assert!(find_dotted_members(".x", |_| Vec::new()).is_empty());
        assert!(find_dotted_members("x.", |_| Vec::new()).is_empty());
    }

    #[test]
    fn symbol_kind_vocabulary_is_complete() {
        use crate::types::SymbolKind;
        for (input, expected) in [
            ("class", SymbolKind::Class),
            ("enum", SymbolKind::Enum),
            ("interface", SymbolKind::Interface),
            ("variable", SymbolKind::Variable),
            ("typealias", SymbolKind::TypeAlias),
            ("Function", SymbolKind::Function),
        ] {
            assert_eq!(input.parse::<SymbolKind>().unwrap(), expected);
        }
        assert!("widget".parse::<SymbolKind>().is_err());
    }

    /// The six default `test_path_patterns` from
    /// `config::defaults::default_test_path_patterns` (W-2), duplicated here
    /// as literals so this test fails loudly if the config defaults ever
    /// drift out of sync with what the classifier is exercised against.
    fn default_test_path_patterns() -> Vec<String> {
        vec![
            "tests/".to_string(),
            "/test/".to_string(),
            "*_test.*".to_string(),
            "test_*.py".to_string(),
            "*.spec.*".to_string(),
            "__tests__/".to_string(),
        ]
    }

    #[test]
    fn classify_caller_role_matches_each_default_pattern() {
        let patterns = default_test_path_patterns();
        for path in [
            "tests/integration_test.rs", // "tests/"
            "src/test/helpers.rs",       // "/test/"
            "src/widget_test.rs",        // "*_test.*"
            "scripts/test_helpers.py",   // "test_*.py"
            "src/widget.spec.ts",        // "*.spec.*"
            "src/__tests__/widget.tsx",  // "__tests__/"
        ] {
            assert_eq!(
                classify_caller_role(path, &patterns),
                CallerRole::Test,
                "expected {path} to classify as Test"
            );
        }
    }

    #[test]
    fn classify_caller_role_defaults_to_production() {
        let patterns = default_test_path_patterns();
        for path in ["src/widget.rs", "src/mcp/service.rs", "lib/handler.py"] {
            assert_eq!(
                classify_caller_role(path, &patterns),
                CallerRole::Production,
                "expected {path} to classify as Production"
            );
        }
    }

    #[test]
    fn filter_callers_partitions_by_role() {
        let callers = vec![
            CallerRelation {
                call: CallRelation {
                    symbol: method_symbol(1, "prod_caller", None, "src::widget"),
                    call_line: None,
                    call_column: None,
                },
                role: CallerRole::Production,
            },
            CallerRelation {
                call: CallRelation {
                    symbol: method_symbol(2, "test_caller", None, "tests::widget"),
                    call_line: None,
                    call_column: None,
                },
                role: CallerRole::Test,
            },
        ];

        let all = filter_callers(callers.clone(), CallerFilter::All);
        assert_eq!(all.len(), 2);

        let production_only = filter_callers(callers.clone(), CallerFilter::Production);
        assert_eq!(production_only.len(), 1);
        assert_eq!(production_only[0].role, CallerRole::Production);

        let test_only = filter_callers(callers, CallerFilter::Test);
        assert_eq!(test_only.len(), 1);
        assert_eq!(test_only[0].role, CallerRole::Test);
    }

    #[test]
    fn count_callers_by_role_totals_sum_to_all() {
        let callers = vec![
            CallerRelation {
                call: CallRelation {
                    symbol: method_symbol(1, "a", None, "src::a"),
                    call_line: None,
                    call_column: None,
                },
                role: CallerRole::Production,
            },
            CallerRelation {
                call: CallRelation {
                    symbol: method_symbol(2, "b", None, "src::b"),
                    call_line: None,
                    call_column: None,
                },
                role: CallerRole::Production,
            },
            CallerRelation {
                call: CallRelation {
                    symbol: method_symbol(3, "c", None, "tests::c"),
                    call_line: None,
                    call_column: None,
                },
                role: CallerRole::Test,
            },
        ];

        let counts = count_callers_by_role(&callers);
        assert_eq!(counts.total, 3);
        assert_eq!(counts.production, 2);
        assert_eq!(counts.test, 1);
        assert_eq!(counts.production + counts.test, counts.total);
    }
}
