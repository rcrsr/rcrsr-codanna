//! MCP tool request types.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindSymbolRequest {
    /// Name of the symbol to find
    pub name: String,
    /// Filter by programming language (e.g., "rust", "python", "typescript", "php")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetCallsRequest {
    /// Name of the function to analyze (use symbol_id for unambiguous lookup)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,
    /// Symbol ID for direct lookup (recommended to avoid ambiguity)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_id: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindCallersRequest {
    /// Name of the function to find callers for (use symbol_id for unambiguous lookup)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,
    /// Symbol ID for direct lookup (recommended to avoid ambiguity)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_id: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AnalyzeImpactRequest {
    /// Name of the symbol to analyze impact for (use symbol_id for unambiguous lookup)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    /// Symbol ID for direct lookup (recommended to avoid ambiguity)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_id: Option<u32>,
    /// Maximum depth to search (default: 3)
    #[serde(default = "default_depth")]
    pub max_depth: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchSymbolsRequest {
    /// Search query (supports fuzzy matching)
    pub query: String,
    /// Maximum number of results (default: 10)
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Filter by symbol kind (e.g., "Function", "Struct", "Trait")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Filter by module path
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Filter by programming language (e.g., "rust", "python", "typescript", "php")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SemanticSearchRequest {
    /// Natural language search query
    pub query: String,
    /// Maximum number of results (default: 10)
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Minimum similarity score (0-1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,
    /// Filter by programming language (e.g., "rust", "python", "typescript", "php")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SemanticSearchWithContextRequest {
    /// Natural language search query
    pub query: String,
    /// Maximum number of results (default: 5, as each includes full context)
    #[serde(default = "default_context_limit")]
    pub limit: u32,
    /// Minimum similarity score (0-1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,
    /// Filter by programming language (e.g., "rust", "python", "typescript", "php")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GetIndexInfoRequest {}

impl schemars::JsonSchema for GetIndexInfoRequest {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("GetIndexInfoRequest")
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::GetIndexInfoRequest"))
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // MCP spec recommends `{"type":"object","additionalProperties":false}` for
        // no-parameter tools. We also include an empty `properties` map because
        // OpenAI's strict function-calling validation rejects object schemas that
        // lack `properties` entirely.
        schemars::Schema::from(
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
            .as_object()
            .unwrap()
            .clone(),
        )
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchDocumentsRequest {
    /// Natural language search query
    pub query: String,
    /// Filter by collection name (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    /// Maximum number of results (default: 5)
    #[serde(default = "default_context_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReindexRequest {
    /// Paths (files or directories) to reindex; omit to reindex all configured indexed_paths
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// For a full reindex (no `paths`), clears the entire index before
    /// rebuilding it. For scoped `paths`, re-indexes the given paths
    /// without a global clear: files are re-parsed even if their content
    /// hash is unchanged, and directories bypass the incremental hash-skip
    /// check. Default: false.
    #[serde(default)]
    pub force: bool,
}

impl ReindexRequest {
    /// Extracts `(paths, force)` from a loosely-typed JSON object. Shared by
    /// every call site that parses reindex arguments out of a raw JSON map
    /// rather than through typed `Parameters` extraction (the
    /// `force-reindex` custom request handler and the CLI's
    /// `codanna mcp reindex` dispatch), so the `paths`/`force` extraction
    /// logic lives in one place instead of being copied at each site.
    ///
    /// A missing `paths` field is `Ok((None, _))` (full reindex, the
    /// documented default). A *present but malformed* `paths` field (e.g.
    /// an array containing a non-string element) is a hard error rather
    /// than silently falling back to `None`: `paths: None` means "reindex
    /// every configured indexed_path", so silently swallowing a malformed
    /// `paths` value would silently widen a scoped reindex request into a
    /// full one instead of surfacing the caller's mistake.
    pub fn parse_args(
        args: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> crate::error::McpResult<(Option<Vec<String>>, bool)> {
        let paths = match args.and_then(|m| m.get("paths")) {
            Some(value) => {
                let parsed: Vec<String> = serde_json::from_value(value.clone()).map_err(|e| {
                    crate::error::McpError::InvalidArguments {
                        reason: format!("`paths` must be an array of strings: {e}"),
                    }
                })?;
                Some(parsed)
            }
            None => None,
        };

        let force = args
            .and_then(|m| m.get("force"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok((paths, force))
    }
}

fn default_depth() -> u32 {
    3
}

fn default_limit() -> u32 {
    10
}

fn default_context_limit() -> u32 {
    5
}
