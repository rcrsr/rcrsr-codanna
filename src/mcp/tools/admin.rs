//! Admin-target tools: reindex.

use rmcp::model::ErrorData as McpError;
use rmcp::model::*;
use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use crate::mcp::requests::{OutputFormat, ReindexRequest};
use crate::mcp::server::CodeIntelligenceServer;
use crate::mcp::service::{self, json_result};

#[tool_router(router = admin_router, vis = "pub(crate)")]
impl CodeIntelligenceServer {
    #[tool(
        description = "Reindex the codebase: specific paths or all configured paths. \
        `force` clears and rebuilds the entire index only for a full reindex (no `paths`); \
        with scoped `paths`, `force` re-parses/re-indexes just those paths without a global clear. \
        `documents` additionally reindexes every configured document collection."
    )]
    pub async fn reindex(
        &self,
        Parameters(ReindexRequest {
            paths,
            force,
            output_format,
            documents,
        }): Parameters<ReindexRequest>,
    ) -> Result<CallToolResult, McpError> {
        let outcome = self.run_reindex(paths, force, documents).await?;

        if output_format == OutputFormat::Json {
            return Ok(json_result(service::reindex_envelope(&outcome)));
        }

        let mut lines = vec![format!(
            "Reindexed {} files, {} symbols in {}ms",
            outcome.reindexed, outcome.symbols, outcome.duration_ms
        )];
        if let Some(doc_totals) = outcome.documents {
            lines.push(format!(
                "Reindexed {} document collection(s): {} files processed, {} chunks created, {} chunks removed",
                doc_totals.collections,
                doc_totals.files_processed,
                doc_totals.chunks_created,
                doc_totals.chunks_removed
            ));
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            lines.join("\n"),
        )]))
    }
}
