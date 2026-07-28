//! Shared path-resolution helper for MCP tools that read symbol source from
//! disk, guarded by the indexed file hash (`read_symbol`, `find_callers`'s
//! `#[cfg(test)]` classification).

use std::path::{Path, PathBuf};

/// Resolve `path_str` to an absolute path, joining against `workspace_root`
/// when `path_str` is relative.
///
/// Trusts an absolute `Symbol.file_path` verbatim (no containment check
/// against `workspace_root`), and falls back to the process's current
/// working directory (`PathBuf::from(".")`) when `workspace_root` is `None`.
/// This matches pre-existing behavior at both call sites
/// (`src/mcp/tools/symbols.rs::resolve_symbol_read_target` and
/// `src/mcp/caller_scope.rs::resolve_file_path`) — this helper only
/// deduplicates the logic, it does not change it.
pub(crate) fn resolve_workspace_relative_path(
    path_str: &str,
    workspace_root: Option<&Path>,
) -> PathBuf {
    let candidate = Path::new(path_str);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(candidate)
    }
}
