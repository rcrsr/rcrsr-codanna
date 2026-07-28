//! Rust-aware refinement of `find_callers`' production/test classification.
//!
//! [`service::classify_caller_role`] tags a caller by matching its
//! `Symbol.file_path` against configured glob/substring patterns. That
//! heuristic can't see Rust's `#[cfg(test)] mod tests { ... }` convention,
//! because the annotated module lives inside an otherwise-production file
//! (e.g. `src/serve_discovery.rs`) and matches none of the path patterns.
//!
//! [`classify_caller_role_in_source`] adds a second, opt-in pass for Rust
//! callers only: when the path heuristic says "production", it parses the
//! caller's source file with a bare `tree_sitter` parser, extracts the line
//! spans covered by `#[cfg(test)]`-annotated items, and re-classifies any
//! caller whose symbol falls inside one of those spans as `Test`.
//!
//! ## Deliberate tradeoff: blocking I/O under the async caller
//!
//! This module reads and parses source files synchronously. `find_callers`
//! runs on the async MCP request path, so this is blocking I/O under an
//! async caller rather than being dispatched through
//! `tokio::task::spawn_blocking`. This is a deliberate choice, not an
//! oversight: `spawn_blocking` would require reshaping `find_callers_data`
//! (and the synchronous CLI path at `src/cli/commands/mcp.rs:536` that also
//! calls it) to thread a spawned future through both call sites, which is
//! out of scope for this fix. The blast radius is bounded — parsing runs at
//! most once per distinct caller file per `find_callers` call (typically
//! 1-3 files), gated by a cheap `contains("#[cfg(")` substring check before
//! any tree-sitter work runs, and every failure path falls back to the
//! existing path heuristic rather than blocking indefinitely or erroring.

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::Symbol;
use crate::mcp::service::{self, CallerRole};

/// Inclusive, 0-indexed line span covering a `#[cfg(test)]`-annotated item,
/// using the same line numbering as `Range::start_line`/`end_line`.
type LineSpan = std::ops::RangeInclusive<u32>;

/// Per-file cache of `#[cfg(test)]` line spans extracted from Rust source,
/// keyed by the caller-provided file path string.
///
/// Backed by a linear-scan `Vec<(Box<str>, Vec<LineSpan>)>`, not a
/// `HashMap`: a single `find_callers` call touches at most a handful of
/// distinct files (typically 1-3), so a hash index would add bookkeeping
/// cost with no measured benefit (§BASIC.8.3).
pub struct TestSpanCache {
    entries: Vec<(Box<str>, Vec<LineSpan>)>,
}

impl TestSpanCache {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn get(&self, path: &str) -> Option<&[LineSpan]> {
        self.entries
            .iter()
            .find(|(cached_path, _)| cached_path.as_ref() == path)
            .map(|(_, spans)| spans.as_slice())
    }

    fn insert(&mut self, path: &str, spans: Vec<LineSpan>) {
        self.entries.push((path.into(), spans));
    }
}

impl Default for TestSpanCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Classify a `find_callers` caller as production or test code, refining the
/// path-only heuristic with `#[cfg(test)]` span analysis for Rust callers.
///
/// `indexed_hash` is the caller's precomputed
/// `IndexFacade::get_file_hash_for_path` result (the same accessor used at
/// `src/mcp/tools/symbols.rs:279-286`); this function takes no facade
/// reference so it can run without holding the facade's async read lock.
/// `None` (no indexed file-info entry) falls back to the path heuristic
/// without touching the filesystem, mirroring the `NoFileInfo` fallback at
/// that call site.
///
/// Every failure path (missing hash, hash mismatch, unreadable file, parse
/// failure, malformed attribute) degrades to the existing path-heuristic
/// answer — this function is infallible and never panics.
pub fn classify_caller_role_in_source(
    symbol: &Symbol,
    test_path_patterns: &[String],
    workspace_root: Option<&Path>,
    indexed_hash: Option<&str>,
    cache: &mut TestSpanCache,
) -> CallerRole {
    let path_heuristic = service::classify_caller_role(&symbol.file_path, test_path_patterns);

    if symbol.language_id.map(|id| id.as_str()) != Some("rust") {
        return path_heuristic;
    }
    if path_heuristic == CallerRole::Test {
        return path_heuristic;
    }

    let path_str: &str = &symbol.file_path;

    if let Some(spans) = cache.get(path_str) {
        return role_from_spans(spans, symbol.range.start_line, path_heuristic);
    }

    let Some(indexed_hash) = indexed_hash else {
        tracing::debug!(
            target: "mcp",
            path = path_str,
            "no indexed file hash; falling back to path heuristic for cfg(test) classification"
        );
        return path_heuristic;
    };

    let full_path = resolve_file_path(path_str, workspace_root);

    let spans = match extract_cfg_test_spans(&full_path, indexed_hash) {
        Some(spans) => spans,
        None => {
            tracing::debug!(
                target: "mcp",
                path = path_str,
                "falling back to path heuristic for cfg(test) classification"
            );
            return path_heuristic;
        }
    };

    let role = role_from_spans(&spans, symbol.range.start_line, path_heuristic);
    cache.insert(path_str, spans);
    role
}

fn role_from_spans(spans: &[LineSpan], line: u32, fallback: CallerRole) -> CallerRole {
    if spans.iter().any(|span| span.contains(&line)) {
        CallerRole::Test
    } else {
        fallback
    }
}

/// Resolve `path_str` to an absolute path, joining against `workspace_root`
/// when relative — the same pattern as
/// `src/mcp/tools/symbols.rs::resolve_symbol_read_target` (:239-249).
fn resolve_file_path(path_str: &str, workspace_root: Option<&Path>) -> std::path::PathBuf {
    let candidate = Path::new(path_str);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(candidate)
    }
}

/// Read `full_path`, verify it matches `indexed_hash` (the same staleness
/// check as `src/mcp/tools/symbols.rs:279-286`), then extract `#[cfg(test)]`
/// line spans. Returns `None` on any failure (unreadable file, stale
/// content, parse failure) so the caller can fall back to the path
/// heuristic.
fn extract_cfg_test_spans(full_path: &Path, indexed_hash: &str) -> Option<Vec<LineSpan>> {
    let content = std::fs::read_to_string(full_path).ok()?;

    let current_hash = crate::indexing::file_info::calculate_hash(&content);
    if current_hash != indexed_hash {
        return None;
    }

    // Cheap gate: skip the tree-sitter parse entirely when the file has no
    // `#[cfg(...)]` attributes at all.
    if !content.contains("#[cfg(") {
        return Some(Vec::new());
    }

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(&content, None)?;

    let mut spans = Vec::new();
    collect_cfg_test_spans(tree.root_node(), &content, &mut spans);
    Some(spans)
}

/// Single preorder pass over the tree: for every `attribute_item` node,
/// check whether it is a `#[cfg(test)]` (and not `#[cfg(not(test))]`-guarded)
/// attribute, and if so record the line span of the item it annotates.
fn collect_cfg_test_spans(node: Node, code: &str, spans: &mut Vec<LineSpan>) {
    if node.kind() == "attribute_item"
        && let Some(target) = cfg_test_target_item(node, code)
    {
        let start = target.start_position().row as u32;
        let end = target.end_position().row as u32;
        spans.push(start..=end);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_cfg_test_spans(child, code, spans);
    }
}

/// If `attribute_item` is a `#[cfg(test)]` (without a `not(...)` guard),
/// walk forward past any further `attribute_item` siblings to the item it
/// annotates and return it. Returns `None` for any other attribute, or if
/// the node shape doesn't match what the Rust grammar guarantees.
fn cfg_test_target_item<'a>(attribute_item: Node<'a>, code: &str) -> Option<Node<'a>> {
    let attribute = attribute_item.named_child(0)?;
    if attribute.kind() != "attribute" {
        return None;
    }

    let name = attribute.named_child(0)?;
    if name.kind() != "identifier" {
        return None;
    }
    if name.utf8_text(code.as_bytes()).ok()? != "cfg" {
        return None;
    }

    let arguments = attribute.child_by_field_name("arguments")?;
    if arguments.kind() != "token_tree" {
        return None;
    }

    let mut has_test = false;
    let mut has_not = false;
    scan_token_tree(arguments, code, &mut has_test, &mut has_not);
    if !has_test || has_not {
        return None;
    }

    let mut current = attribute_item.next_named_sibling()?;
    while current.kind() == "attribute_item" {
        current = current.next_named_sibling()?;
    }
    Some(current)
}

/// Recursively scan a `#[cfg(...)]` attribute's `token_tree` arguments for
/// bare `identifier` nodes named `test` or `not`. String literals (e.g.
/// `feature = "test"`) never match, since they parse as `string_literal`,
/// not `identifier`.
fn scan_token_tree(node: Node, code: &str, has_test: &mut bool, has_not: &mut bool) {
    if node.kind() == "identifier"
        && let Ok(text) = node.utf8_text(code.as_bytes())
    {
        match text {
            "test" => *has_test = true,
            "not" => *has_not = true,
            _ => {}
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_token_tree(child, code, has_test, has_not);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_for(code: &str) -> Vec<LineSpan> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("tree-sitter-rust language should load");
        let tree = parser
            .parse(code, None)
            .expect("valid Rust source should parse");
        let mut spans = Vec::new();
        collect_cfg_test_spans(tree.root_node(), code, &mut spans);
        spans
    }

    #[test]
    fn test_cfg_test_mod_covers_its_body_but_not_lines_above() {
        let code = "fn above() {}\n#[cfg(test)]\nmod tests {\n    fn a() {}\n}\n";
        let spans = spans_for(code);
        assert_eq!(spans.len(), 1);
        // Line 0 ("fn above() {}") is outside the mod's span.
        assert!(!spans.iter().any(|s| s.contains(&0)));
        // Line 3 ("fn a() {}") is inside the mod's span.
        assert!(spans.iter().any(|s| s.contains(&3)));
    }

    #[test]
    fn test_cfg_feature_test_string_is_not_a_test_span() {
        let code = "#[cfg(feature = \"test\")]\nmod helpers {\n    fn h() {}\n}\n";
        assert!(spans_for(code).is_empty());
    }

    #[test]
    fn test_cfg_not_test_is_not_a_test_span() {
        let code = "#[cfg(not(test))]\nfn prod() {}\n";
        assert!(spans_for(code).is_empty());
    }

    #[test]
    fn test_consecutive_attributes_resolve_to_annotated_item() {
        let code = "#[cfg(test)]\n#[allow(dead_code)]\nmod tests {\n    fn a() {}\n}\n";
        let spans = spans_for(code);
        assert_eq!(spans.len(), 1);
        assert!(spans.iter().any(|s| s.contains(&3)));
    }

    #[test]
    fn test_braces_in_strings_and_comments_do_not_shift_span() {
        let code = concat!(
            "// a comment with a brace } in it\n",
            "const S: &str = \"a { brace and a } too\";\n",
            "const C: char = '}';\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    fn a() {}\n",
            "}\n",
        );
        let spans = spans_for(code);
        assert_eq!(spans.len(), 1);
        // The mod_item starts at line 4 (0-indexed) and ends at line 6.
        assert!(spans[0].contains(&4));
        assert!(spans[0].contains(&6));
        assert!(!spans[0].contains(&0));
    }

    #[test]
    fn test_non_rust_language_uses_path_heuristic_unchanged() {
        let mut symbol = Symbol::new(
            crate::types::SymbolId::new(1).expect("nonzero id"),
            "helper",
            crate::types::SymbolKind::Function,
            crate::types::FileId::new(1).expect("nonzero id"),
            crate::types::Range {
                start_line: 5,
                start_column: 0,
                end_line: 5,
                end_column: 10,
            },
        );
        symbol.file_path = "src/foo.py".into();
        symbol.language_id = Some(crate::parsing::LanguageId::new("python"));

        let patterns = vec!["tests/".to_string()];
        let mut cache = TestSpanCache::new();
        let role = classify_caller_role_in_source(&symbol, &patterns, None, None, &mut cache);
        assert_eq!(
            role,
            service::classify_caller_role(&symbol.file_path, &patterns)
        );
        assert_eq!(role, CallerRole::Production);
    }
}
