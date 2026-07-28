//! Rust-aware refinement of `find_callers`' production/test classification.
//!
//! [`service::classify_caller_role`] tags a caller by matching its
//! `Symbol.file_path` against configured glob/substring patterns. That
//! heuristic can't see Rust's `#[cfg(test)] mod tests { ... }` convention,
//! because the annotated module lives inside an otherwise-production file
//! (e.g. `src/serve_discovery.rs`) and matches none of the path patterns.
//!
//! [`classify_prepared`] adds a second, opt-in pass for Rust callers only:
//! when the path heuristic says "production", it parses the caller's source
//! file with a bare `tree_sitter` parser, extracts the line spans covered by
//! `#[cfg(test)]`-annotated items, and re-classifies any caller whose symbol
//! falls inside one of those spans as `Test`.
//!
//! ## Two-phase classification: facade access vs. blocking work
//!
//! `find_callers`' caller set is unbounded in principle — it's every distinct
//! file that calls the target symbol, with no `limit`/pagination on
//! `FindCallersRequest` today — so file reads and tree-sitter parses cannot
//! run synchronously while the facade's async read lock is held, nor
//! directly on a tokio worker. [`prepare_classification`]/[`ClassificationPrep`]
//! and [`classify_prepared`] split the work accordingly:
//!
//! - **Phase 1** ([`prepare_classification`]) needs the facade (for
//!   `IndexFacade::get_file_hash_for_path` and `Settings::workspace_root`)
//!   but does no I/O. It computes the path heuristic for every caller,
//!   narrows to the callers that actually need source inspection (Rust
//!   language *and* path heuristic == production), and resolves each
//!   *distinct* such file's absolute path and indexed hash exactly once —
//!   never per caller, never for non-Rust callers, never for callers already
//!   `Test` by path. The result is an owned, facade-free, `Send` bundle.
//! - **Phase 2** ([`classify_prepared`]) takes that bundle and does the
//!   blocking read + hash-check + parse, once per distinct file, with no
//!   facade access at all. It is a plain sync function: callers run it
//!   inline (the synchronous CLI path, `src/cli/commands/mcp.rs:536`, which
//!   has no async runtime to starve) or inside
//!   `tokio::task::spawn_blocking` (the async MCP tool path,
//!   `src/mcp/tools/symbols.rs`/`src/mcp/service.rs`), after dropping the
//!   facade's read guard.
//!
//! Every failure path (missing hash, hash mismatch, unreadable file, parse
//! failure, malformed attribute) still degrades to the path-heuristic answer
//! computed in phase 1 — this module is infallible and never panics; a
//! panicked `spawn_blocking` task is handled by its caller falling back to
//! the same phase-1 path-heuristic roles.

use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser};

use crate::Symbol;
use crate::indexing::facade::IndexFacade;
use crate::mcp::paths::resolve_workspace_relative_path;
use crate::mcp::service::{self, CallerRole};

/// Inclusive, 0-indexed line span covering a `#[cfg(test)]`-annotated item,
/// using the same line numbering as `Range::start_line`/`end_line`.
type LineSpan = std::ops::RangeInclusive<u32>;

fn role_from_spans(spans: &[LineSpan], line: u32, fallback: CallerRole) -> CallerRole {
    if spans.iter().any(|span| span.contains(&line)) {
        CallerRole::Test
    } else {
        fallback
    }
}

/// Resolve `path_str` to an absolute path, joining against `workspace_root`
/// when relative. Thin wrapper over the shared
/// [`crate::mcp::paths::resolve_workspace_relative_path`] helper, also used
/// by `src/mcp/tools/symbols.rs::resolve_symbol_read_target`.
fn resolve_file_path(path_str: &str, workspace_root: Option<&Path>) -> PathBuf {
    resolve_workspace_relative_path(path_str, workspace_root)
}

/// A single distinct file identified in phase 1 as needing `#[cfg(test)]`
/// source inspection: its absolute path plus the indexed hash to guard
/// against staleness. Facade-free and `Send` — safe to move into
/// `spawn_blocking`.
struct FileToInspect {
    full_path: PathBuf,
    indexed_hash: String,
}

/// Phase-1 output for a single caller: the path-heuristic role (always
/// computed) plus, when this caller needs source inspection, the index into
/// [`ClassificationPrep::files`] holding its file's read target.
struct PreparedCaller {
    path_heuristic: CallerRole,
    file_index: Option<usize>,
    start_line: u32,
}

/// Owned, facade-free bundle produced by [`prepare_classification`] and
/// consumed by [`classify_prepared`]. Safe to move into `spawn_blocking` or
/// use directly on a synchronous call path.
pub(crate) struct ClassificationPrep {
    per_caller: Vec<PreparedCaller>,
    files: Vec<FileToInspect>,
}

impl ClassificationPrep {
    /// The path-heuristic-only role for every caller, in the same order as
    /// [`classify_prepared`]'s output. Used as the degraded fallback when a
    /// `spawn_blocking` task running `classify_prepared` panics.
    pub(crate) fn path_heuristic_roles(&self) -> Vec<CallerRole> {
        self.per_caller.iter().map(|c| c.path_heuristic).collect()
    }
}

/// PHASE 1: needs the facade (`get_file_hash_for_path`, `workspace_root`)
/// but performs no file I/O — safe to run while the facade's async read
/// lock is held.
///
/// For every caller, computes the path heuristic via
/// [`service::classify_caller_role`]. Narrows to the callers that actually
/// need `#[cfg(test)]` source inspection — Rust language *and* path
/// heuristic == production — and resolves each *distinct* such file's
/// absolute path and indexed hash exactly once, regardless of how many
/// callers reference it.
pub(crate) fn prepare_classification<'a>(
    facade: &IndexFacade,
    callers: impl IntoIterator<Item = &'a Symbol>,
    test_path_patterns: &[String],
) -> ClassificationPrep {
    let workspace_root = facade.settings().workspace_root.clone();
    let workspace_root = workspace_root.as_deref();

    let mut files: Vec<FileToInspect> = Vec::new();
    // Linear-scan dedup, not a `HashMap`: bounded by the number of distinct
    // caller files in one `find_callers` call, which is small in practice
    // (§BASIC.8.3).
    let mut file_index_by_path: Vec<(Box<str>, Option<usize>)> = Vec::new();

    let per_caller = callers
        .into_iter()
        .map(|symbol| {
            let path_heuristic =
                service::classify_caller_role(&symbol.file_path, test_path_patterns);
            let is_rust = symbol.language_id.map(|id| id.as_str()) == Some("rust");

            if !is_rust || path_heuristic == CallerRole::Test {
                return PreparedCaller {
                    path_heuristic,
                    file_index: None,
                    start_line: symbol.range.start_line,
                };
            }

            let path_str: &str = &symbol.file_path;

            if let Some((_, file_index)) = file_index_by_path
                .iter()
                .find(|(cached_path, _)| cached_path.as_ref() == path_str)
            {
                return PreparedCaller {
                    path_heuristic,
                    file_index: *file_index,
                    start_line: symbol.range.start_line,
                };
            }

            let file_index = facade.get_file_hash_for_path(path_str).map(|indexed_hash| {
                let full_path = resolve_file_path(path_str, workspace_root);
                let idx = files.len();
                files.push(FileToInspect {
                    full_path,
                    indexed_hash,
                });
                idx
            });
            file_index_by_path.push((path_str.into(), file_index));

            PreparedCaller {
                path_heuristic,
                file_index,
                start_line: symbol.range.start_line,
            }
        })
        .collect();

    ClassificationPrep { per_caller, files }
}

/// PHASE 2: no facade access, pure blocking file I/O + tree-sitter parsing.
/// Safe to call from `spawn_blocking` or directly on a synchronous call
/// path (the CLI's `find_callers_data`).
///
/// Reads and parses each distinct file in [`ClassificationPrep::files`]
/// exactly once; any failure (unreadable file, stale hash, parse failure)
/// degrades that file's callers to their phase-1 path-heuristic role rather
/// than erroring. This function is infallible and never panics: no failure
/// mode reports a caller as anything other than its path-heuristic answer.
pub(crate) fn classify_prepared(prep: ClassificationPrep) -> Vec<CallerRole> {
    let file_spans: Vec<Vec<LineSpan>> = prep
        .files
        .iter()
        .map(|file| extract_cfg_test_spans(&file.full_path, &file.indexed_hash).unwrap_or_default())
        .collect();

    prep.per_caller
        .into_iter()
        .map(|caller| match caller.file_index {
            None => caller.path_heuristic,
            Some(idx) => {
                role_from_spans(&file_spans[idx], caller.start_line, caller.path_heuristic)
            }
        })
        .collect()
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
    // `cfg` attributes at all. Matching the bare substring "cfg" (rather
    // than the exact `"#[cfg("` byte sequence) tolerates valid-but-unusual
    // formatting like `#[cfg (test)]` or a line break between `cfg` and its
    // token tree; this gate is only an optimization, so over-matching just
    // costs an extra (fruitless) parse, while under-matching would silently
    // misclassify a caller.
    if !content.contains("cfg") {
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
/// check whether it is a `#[cfg(test)]` (and not `#[cfg(not(test))]`- or
/// `#[cfg(any(test, ...))]`-guarded) attribute, and if so record the line
/// span of the item it annotates.
fn collect_cfg_test_spans(node: Node, code: &str, spans: &mut Vec<LineSpan>) {
    let mut claimed = std::collections::HashSet::new();
    collect_cfg_test_spans_impl(node, code, spans, &mut claimed);
}

/// Recursive worker for [`collect_cfg_test_spans`]. `claimed` tracks the ids
/// of item nodes already recorded as a `#[cfg(test)]` target, so a nested
/// `#[cfg(test)]` inside an already-recorded span (e.g. a test fn inside a
/// `#[cfg(test)] mod tests { ... }`) doesn't get recursed into and pushed
/// again as a redundant sub-span.
fn collect_cfg_test_spans_impl(
    node: Node,
    code: &str,
    spans: &mut Vec<LineSpan>,
    claimed: &mut std::collections::HashSet<usize>,
) {
    if claimed.contains(&node.id()) {
        return;
    }

    if node.kind() == "attribute_item"
        && let Some(target) = cfg_test_target_item(node, code)
    {
        let start = target.start_position().row as u32;
        let end = target.end_position().row as u32;
        spans.push(start..=end);
        claimed.insert(target.id());
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_cfg_test_spans_impl(child, code, spans, claimed);
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
    let mut disqualified = false;
    scan_token_tree(arguments, code, &mut has_test, &mut disqualified);
    if !has_test || disqualified {
        return None;
    }

    let mut current = attribute_item.next_named_sibling()?;
    // A comment between `#[cfg(test)]` and its item is a NAMED sibling in
    // tree-sitter-rust (`line_comment`/`block_comment`), so it must be
    // skipped alongside further `attribute_item` siblings or it collapses
    // the recorded span to just the comment's own line.
    while matches!(
        current.kind(),
        "attribute_item" | "line_comment" | "block_comment"
    ) {
        current = current.next_named_sibling()?;
    }
    Some(current)
}

/// Recursively scan a `#[cfg(...)]` attribute's `token_tree` arguments for
/// a bare `identifier` named `test`, tracking whether that `test` occurs
/// under a disqualifying combinator.
///
/// `#[cfg(not(test))]` and `#[cfg(any(test, feature = "x"))]` are both
/// disqualifying: the former is the standard "only when NOT testing"
/// guard, and the latter compiles in a normal (non-test) build whenever
/// the sibling feature is enabled, so treating it as test-only would
/// misclassify a production-reachable caller as test. `#[cfg(all(test,
/// ...))]` is not disqualifying — `all(...)` is a conjunction, so `test`
/// inside it still means "test builds only".
///
/// String literals (e.g. `feature = "test"`) never match, since they parse
/// as `string_literal`, not `identifier`.
fn scan_token_tree(node: Node, code: &str, has_test: &mut bool, disqualified: &mut bool) {
    // Once disqualified, no further scanning can change the outcome: the
    // caller's final check is `!has_test || disqualified`, and `disqualified`
    // only ever flips from false to true.
    if *disqualified {
        return;
    }

    let child_count = node.child_count();
    let mut i: usize = 0;
    while i < child_count {
        let Some(child) = node.child(i as u32) else {
            i += 1;
            continue;
        };

        if child.kind() == "identifier"
            && let Ok(text) = child.utf8_text(code.as_bytes())
        {
            match text {
                "test" => *has_test = true,
                "any" | "not" => {
                    // The combinator's argument list is the next
                    // `token_tree` sibling among this node's remaining
                    // children (tree-sitter-rust represents parenthesized
                    // groups as nested `token_tree` nodes, flat otherwise).
                    let mut j = i + 1;
                    let mut nested = None;
                    while j < child_count {
                        if let Some(candidate) = node.child(j as u32) {
                            if candidate.kind() == "token_tree" {
                                nested = Some(candidate);
                                break;
                            }
                        }
                        j += 1;
                    }
                    if let Some(nested_tree) = nested {
                        let mut nested_has_test = false;
                        let mut nested_disqualified = false;
                        scan_token_tree(
                            nested_tree,
                            code,
                            &mut nested_has_test,
                            &mut nested_disqualified,
                        );
                        if nested_has_test || nested_disqualified {
                            *disqualified = true;
                            return;
                        }
                        // Skip past the nested tree we just scanned so the
                        // generic `token_tree` branch below doesn't rescan it.
                        i = j;
                    }
                }
                _ => {}
            }
        } else if child.kind() == "token_tree" {
            scan_token_tree(child, code, has_test, disqualified);
            if *disqualified {
                return;
            }
        }

        i += 1;
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

    fn test_symbol(file_path: &str, start_line: u32) -> Symbol {
        let mut symbol = Symbol::new(
            crate::types::SymbolId::new(1).expect("nonzero id"),
            "helper",
            crate::types::SymbolKind::Function,
            crate::types::FileId::new(1).expect("nonzero id"),
            crate::types::Range {
                start_line,
                start_column: 0,
                end_line: start_line,
                end_column: 10,
            },
        );
        symbol.file_path = file_path.into();
        symbol
    }

    fn test_facade(dir: &std::path::Path) -> IndexFacade {
        let settings = crate::config::Settings {
            index_path: dir.join("index"),
            workspace_root: None,
            ..Default::default()
        };
        IndexFacade::new(std::sync::Arc::new(settings)).expect("construct test facade")
    }

    #[test]
    fn test_non_rust_language_uses_path_heuristic_unchanged() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let facade = test_facade(dir.path());

        let mut symbol = test_symbol("src/foo.py", 5);
        symbol.language_id = Some(crate::parsing::LanguageId::new("python"));

        let patterns = vec!["tests/".to_string()];
        let callers = [symbol.clone()];
        let prep = prepare_classification(&facade, &callers, &patterns);
        assert_eq!(
            prep.per_caller.len(),
            1,
            "one caller in must produce one prepared entry"
        );
        assert_eq!(
            prep.per_caller[0].file_index, None,
            "a non-Rust caller must never be queued for source inspection"
        );
        assert!(
            prep.files.is_empty(),
            "no file should be resolved for a non-Rust caller"
        );

        let roles = classify_prepared(prep);
        assert_eq!(
            roles,
            vec![service::classify_caller_role(&symbol.file_path, &patterns)]
        );
        assert_eq!(roles[0], CallerRole::Production);
    }

    #[test]
    fn test_prepare_classification_dedups_distinct_rust_production_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let source = dir.path().join("lib.rs");
        std::fs::write(
            &source,
            "fn above() {}\n#[cfg(test)]\nmod tests {\n    fn in_test() {}\n}\n",
        )
        .expect("write fixture file");

        let mut facade = test_facade(dir.path());
        facade.index_file(&source).expect("index fixture file");

        let indexed_path = facade
            .get_all_indexed_paths()
            .into_iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()) == Some("lib.rs"))
            .expect("fixture file must be indexed");
        let indexed_path_str = indexed_path.to_string_lossy().into_owned();

        let mut above = test_symbol(&indexed_path_str, 0);
        above.language_id = Some(crate::parsing::LanguageId::new("rust"));
        let mut in_test = above.clone();
        in_test.range.start_line = 3;
        in_test.range.end_line = 3;

        let patterns: Vec<String> = Vec::new();
        let callers = [above, in_test];
        let prep = prepare_classification(&facade, &callers, &patterns);

        assert_eq!(
            prep.files.len(),
            1,
            "two callers in the same file must resolve to one distinct file entry"
        );
        assert!(
            prep.per_caller.iter().all(|c| c.file_index == Some(0)),
            "both callers must reference the single deduped file entry"
        );

        let roles = classify_prepared(prep);
        assert_eq!(
            roles,
            vec![CallerRole::Production, CallerRole::Test],
            "the caller above the cfg(test) mod stays production; the one inside it is test"
        );
    }

    #[test]
    fn test_comment_between_attribute_and_item_does_not_collapse_span() {
        // A `line_comment` is a NAMED sibling in tree-sitter-rust, so it
        // must be skipped the same way `attribute_item` siblings are, or
        // the recorded span collapses to just the comment's own line.
        let code = "#[cfg(test)]\n// unit tests\nmod tests {\n    fn a() {}\n}\n";
        let spans = spans_for(code);
        assert_eq!(spans.len(), 1);
        // The mod_item spans lines 2-4 (0-indexed), not just the comment
        // on line 1.
        assert!(spans[0].contains(&2));
        assert!(spans[0].contains(&4));
        assert!(!spans[0].contains(&1));
    }

    #[test]
    fn test_comment_between_attribute_and_item_block_comment_variant() {
        let code = "#[cfg(test)]\n/* unit tests */\nmod tests {\n    fn a() {}\n}\n";
        let spans = spans_for(code);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].contains(&2));
        assert!(spans[0].contains(&4));
        assert!(!spans[0].contains(&1));
    }

    #[test]
    fn test_cfg_any_test_is_not_a_test_span() {
        // `#[cfg(any(test, feature = "x"))]` compiles in a normal
        // (non-test) build whenever the sibling feature is enabled, so it
        // must NOT be treated as test-only — the safe/conservative answer.
        let code = "#[cfg(any(test, feature = \"x\"))]\npub fn helper() {}\n";
        assert!(spans_for(code).is_empty());
    }

    #[test]
    fn test_cfg_all_test_is_a_test_span() {
        // `all(...)` is a conjunction: `test` inside it still means
        // "test builds only", so this should still be recorded.
        let code = "#[cfg(all(test, feature = \"x\"))]\nfn only_in_test() {}\n";
        let spans = spans_for(code);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].contains(&1));
    }

    #[test]
    fn test_cfg_with_space_before_paren_is_recognized() {
        // The cheap pre-parse gate must not produce false negatives on
        // valid-but-unusual formatting like a space between `cfg` and `(`.
        let code = "#[cfg (test)]\nmod tests {\n    fn a() {}\n}\n";
        let spans = spans_for(code);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].contains(&1));
        assert!(spans[0].contains(&3));
    }

    #[test]
    fn test_extract_cfg_test_spans_gate_tolerates_space_before_paren() {
        // Exercise the actual gate in `extract_cfg_test_spans` (not just
        // `collect_cfg_test_spans`, which is only reached after the gate).
        let content = "#[cfg (test)]\nmod tests {\n    fn a() {}\n}\n";
        let hash = crate::indexing::file_info::calculate_hash(content);
        let dir =
            std::env::temp_dir().join(format!("codanna_cfg_gate_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file_path = dir.join("lib.rs");
        std::fs::write(&file_path, content).expect("write temp file");

        let spans = extract_cfg_test_spans(&file_path, &hash);
        assert_eq!(spans.map(|s| s.len()), Some(1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_missing_indexed_hash_skips_inspection_without_reading_the_file() {
        // A Rust, path-heuristic-production caller whose file was never
        // indexed (no `get_file_hash_for_path` entry) must be queued for
        // inspection by neither phase — no file read is even attempted, and
        // classification degrades straight to the path heuristic.
        let dir = tempfile::tempdir().expect("create temp dir");
        let facade = test_facade(dir.path());

        let mut symbol = test_symbol("src/foo.rs", 5);
        symbol.language_id = Some(crate::parsing::LanguageId::new("rust"));

        let patterns: Vec<String> = Vec::new();
        let callers = [symbol];
        let prep = prepare_classification(&facade, &callers, &patterns);
        assert_eq!(
            prep.per_caller[0].file_index, None,
            "a caller with no indexed file hash must not be queued for inspection"
        );
        assert!(prep.files.is_empty());

        let roles = classify_prepared(prep);
        assert_eq!(roles, vec![CallerRole::Production]);
    }

    #[test]
    fn test_extract_cfg_test_spans_returns_none_on_hash_mismatch() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("lib.rs");
        std::fs::write(&file_path, "fn helper() {}\n").expect("write temp file");

        // A hash that will never match the file's real contents simulates a
        // stale index entry; the caller (`classify_prepared`) treats `None`
        // here as "no spans", degrading to the path heuristic.
        let spans = extract_cfg_test_spans(&file_path, "stale-hash-that-never-matches");
        assert!(
            spans.is_none(),
            "a hash mismatch must be reported as extraction failure, not empty spans"
        );
    }
}
