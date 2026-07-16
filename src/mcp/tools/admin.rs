//! Admin-target tools: reindex.

use rmcp::model::ErrorData as McpError;
use rmcp::model::*;
use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use crate::mcp::requests::{OutputFormat, ReindexRequest};
use crate::mcp::server::CodeIntelligenceServer;
use crate::mcp::service;

/// Render a JSON [`crate::io::envelope::Envelope`] as a single-block tool
/// result. Mirrors the identically-named helpers in `mcp/tools/symbols.rs`
/// and `mcp/tools/search.rs`.
fn json_result<T: serde::Serialize>(envelope: crate::io::envelope::Envelope<T>) -> CallToolResult {
    let text = serde_json::to_string(&envelope).unwrap_or_else(|e| {
        format!(r#"{{"type":"error","message":"envelope serialization failed: {e}"}}"#)
    });
    CallToolResult::success(vec![ContentBlock::text(text)])
}

#[tool_router(router = admin_router, vis = "pub(crate)")]
impl CodeIntelligenceServer {
    #[tool(
        description = "Reindex the codebase: specific paths or all configured paths. \
        `force` clears and rebuilds the entire index only for a full reindex (no `paths`); \
        with scoped `paths`, `force` re-parses/re-indexes just those paths without a global clear."
    )]
    pub async fn reindex(
        &self,
        Parameters(ReindexRequest {
            paths,
            force,
            output_format,
        }): Parameters<ReindexRequest>,
    ) -> Result<CallToolResult, McpError> {
        let outcome = self.run_reindex(paths, force).await?;

        if output_format == OutputFormat::Json {
            return Ok(json_result(service::reindex_envelope(&outcome)));
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Reindexed {} files, {} symbols in {}ms",
            outcome.reindexed, outcome.symbols, outcome.duration_ms
        ))]))
    }
}
