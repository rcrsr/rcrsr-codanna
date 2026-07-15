//! Admin-target tools: reindex.

use std::time::Instant;

use rmcp::model::ErrorData as McpError;
use rmcp::model::*;
use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use crate::mcp::requests::ReindexRequest;
use crate::mcp::server::CodeIntelligenceServer;

#[tool_router(router = admin_router, vis = "pub(crate)")]
impl CodeIntelligenceServer {
    #[tool(
        description = "Reindex the codebase: specific paths or all configured paths. Use force to clear and rebuild."
    )]
    pub async fn reindex(
        &self,
        Parameters(ReindexRequest { paths, force }): Parameters<ReindexRequest>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        let (reindexed, symbols) = self.run_reindex(paths, force).await?;
        let duration_ms = start.elapsed().as_millis();

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Reindexed {reindexed} files, {symbols} symbols in {duration_ms}ms"
        ))]))
    }
}
