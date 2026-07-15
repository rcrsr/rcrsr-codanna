//! Admin-target tools: reindex.

use rmcp::model::ErrorData as McpError;
use rmcp::model::*;
use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use crate::mcp::requests::ReindexRequest;
use crate::mcp::server::CodeIntelligenceServer;

#[tool_router(router = admin_router, vis = "pub(crate)")]
impl CodeIntelligenceServer {
    #[tool(
        description = "Reindex the codebase: specific paths or all configured paths. \
        `force` clears and rebuilds the entire index only for a full reindex (no `paths`); \
        with scoped `paths`, `force` re-parses/re-indexes just those paths without a global clear."
    )]
    pub async fn reindex(
        &self,
        Parameters(ReindexRequest { paths, force }): Parameters<ReindexRequest>,
    ) -> Result<CallToolResult, McpError> {
        let outcome = self.run_reindex(paths, force).await?;

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Reindexed {} files, {} symbols in {}ms",
            outcome.reindexed, outcome.symbols, outcome.duration_ms
        ))]))
    }
}
