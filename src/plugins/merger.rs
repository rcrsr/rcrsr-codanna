//! MCP.json merging logic for plugin installation

use super::error::{PluginError, PluginResult};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;

/// Result of merging MCP servers
#[derive(Debug)]
pub struct McpMergeOutcome {
    pub added_keys: Vec<String>,
    pub previous_content: Option<String>,
    pub file_existed: bool,
}

/// Merge plugin MCP servers into project's .mcp.json
pub fn merge_mcp_servers(
    project_mcp_path: &Path,
    plugin_servers: &Value,
    force: bool,
) -> PluginResult<McpMergeOutcome> {
    let previous_content = if project_mcp_path.exists() {
        Some(std::fs::read_to_string(project_mcp_path)?)
    } else {
        None
    };
    let file_existed = previous_content.is_some();

    // Load existing .mcp.json or create new one
    let mut project_mcp = match &previous_content {
        Some(content) => serde_json::from_str(content)?,
        None => json!({ "mcpServers": {} }),
    };

    // Get mcpServers object
    let servers = project_mcp
        .as_object_mut()
        .ok_or_else(|| PluginError::InvalidPluginManifest {
            reason: "Invalid .mcp.json structure".to_string(),
        })?
        .entry("mcpServers")
        .or_insert_with(|| json!({}));

    let servers_obj =
        servers
            .as_object_mut()
            .ok_or_else(|| PluginError::InvalidPluginManifest {
                reason: "mcpServers must be an object".to_string(),
            })?;

    // Track plugin-owned keys and whether anything changed
    let mut owned_keys = Vec::new();
    let mut changed = false;

    // Merge plugin servers
    if let Some(plugin_servers_obj) = plugin_servers.as_object() {
        for (key, value) in plugin_servers_obj {
            owned_keys.push(key.clone());

            // Check for conflicts
            if let Some(existing) = servers_obj.get(key) {
                if existing == value {
                    continue;
                }
                if !force {
                    return Err(PluginError::McpServerConflict { key: key.clone() });
                }
            }

            // Add server configuration
            servers_obj.insert(key.clone(), value.clone());
            changed = true;
        }
    }

    if changed {
        let json = serde_json::to_string_pretty(&project_mcp)?;
        std::fs::write(project_mcp_path, json)?;
    }

    Ok(McpMergeOutcome {
        added_keys: owned_keys,
        previous_content,
        file_existed,
    })
}

/// Check for MCP server conflicts without modifying files
pub fn check_mcp_conflicts(
    project_mcp_path: &Path,
    plugin_servers: &Value,
    force: bool,
    allowed_keys: &HashSet<String>,
) -> PluginResult<()> {
    if !project_mcp_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(project_mcp_path)?;
    let project_mcp: Value = serde_json::from_str(&content)?;

    let servers = project_mcp
        .get("mcpServers")
        .and_then(|value| value.as_object())
        .ok_or_else(|| PluginError::InvalidPluginManifest {
            reason: "Invalid .mcp.json structure".to_string(),
        })?;

    if let Some(plugin_servers_obj) = plugin_servers.as_object() {
        for key in plugin_servers_obj.keys() {
            if servers.contains_key(key) && !force && !allowed_keys.contains(key) {
                return Err(PluginError::McpServerConflict { key: key.clone() });
            }
        }
    }

    Ok(())
}

/// Remove plugin MCP servers from project's .mcp.json
pub fn remove_mcp_servers(project_mcp_path: &Path, server_keys: &[String]) -> PluginResult<()> {
    if !project_mcp_path.exists() {
        return Ok(());
    }

    // Load existing .mcp.json
    let content = std::fs::read_to_string(project_mcp_path)?;
    let mut project_mcp: Value = serde_json::from_str(&content)?;

    // Get mcpServers object
    if let Some(servers) = project_mcp.as_object_mut()
        && let Some(servers_obj) = servers.get_mut("mcpServers")
        && let Some(servers_map) = servers_obj.as_object_mut()
    {
        // Remove specified keys
        for key in server_keys {
            servers_map.remove(key);
        }
    }

    // Save updated .mcp.json
    let json = serde_json::to_string_pretty(&project_mcp)?;
    std::fs::write(project_mcp_path, json)?;

    Ok(())
}

/// Load MCP servers from plugin
pub fn load_plugin_mcp_servers(
    plugin_dir: &Path,
    mcp_spec: &crate::plugins::plugin::McpServerSpec,
) -> PluginResult<Value> {
    use crate::plugins::plugin::McpServerSpec;

    match mcp_spec {
        McpServerSpec::Path(path) => {
            let mcp_path = plugin_dir.join(path);
            let content = std::fs::read_to_string(&mcp_path)?;
            let mcp_json: Value = serde_json::from_str(&content)?;

            // Extract mcpServers object
            if let Some(servers) = mcp_json.get("mcpServers") {
                Ok(servers.clone())
            } else {
                Ok(json!({}))
            }
        }
        McpServerSpec::Inline(value) => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_merge_mcp_servers() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let mcp_path = temp_dir.path().join(".mcp.json");

        // Create initial .mcp.json
        let initial = json!({
            "mcpServers": {
                "existing": {
                    "command": "existing-server"
                }
            }
        });
        std::fs::write(&mcp_path, serde_json::to_string(&initial)?)?;

        // Merge new servers
        let plugin_servers = json!({
            "plugin-server": {
                "command": "plugin-command"
            }
        });

        let outcome = merge_mcp_servers(&mcp_path, &plugin_servers, false)?;
        assert_eq!(outcome.added_keys, vec!["plugin-server".to_string()]);
        assert!(outcome.file_existed);

        // Verify merged content
        let content = std::fs::read_to_string(&mcp_path)?;
        let merged: Value = serde_json::from_str(&content)?;
        assert!(merged["mcpServers"]["existing"].is_object());
        assert!(merged["mcpServers"]["plugin-server"].is_object());

        Ok(())
    }

    #[test]
    fn test_conflict_detection() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let mcp_path = temp_dir.path().join(".mcp.json");

        // Create .mcp.json with existing server
        let initial = json!({
            "mcpServers": {
                "conflicting": {
                    "command": "existing"
                }
            }
        });
        std::fs::write(&mcp_path, serde_json::to_string(&initial)?)?;

        // Try to merge conflicting server
        let plugin_servers = json!({
            "conflicting": {
                "command": "new"
            }
        });

        let result = merge_mcp_servers(&mcp_path, &plugin_servers, false);
        assert!(matches!(result, Err(PluginError::McpServerConflict { .. })));

        // Check helper detects conflict without modifying file
        let check = check_mcp_conflicts(&mcp_path, &plugin_servers, false, &HashSet::new());
        assert!(matches!(check, Err(PluginError::McpServerConflict { .. })));

        Ok(())
    }

    #[test]
    fn test_conflict_allowed_keys() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let mcp_path = temp_dir.path().join(".mcp.json");

        let initial = json!({
            "mcpServers": {
                "codanna": {
                    "command": "existing"
                }
            }
        });
        std::fs::write(&mcp_path, serde_json::to_string(&initial)?)?;

        let plugin_servers = json!({
            "codanna": {
                "command": "replacement"
            }
        });

        let mut allowed = HashSet::new();
        allowed.insert("codanna".to_string());

        let check = check_mcp_conflicts(&mcp_path, &plugin_servers, false, &allowed);
        assert!(check.is_ok());

        Ok(())
    }

    #[test]
    fn test_merge_noop_preserves_file() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let mcp_path = temp_dir.path().join(".mcp.json");

        let initial = r#"{
  "mcpServers": {
    "codanna": {
      "command": "npx",
      "args": ["start"]
    },
    "context7": {
      "command": "npx",
      "args": ["@upstash/context7-mcp"]
    }
  }
}
"#;
        std::fs::write(&mcp_path, initial)?;

        let plugin_servers = json!({
            "codanna": {
                "command": "npx",
                "args": ["start"]
            }
        });

        let outcome = merge_mcp_servers(&mcp_path, &plugin_servers, false)?;
        assert_eq!(outcome.added_keys, vec!["codanna".to_string()]);

        let final_content = std::fs::read_to_string(&mcp_path)?;
        assert_eq!(final_content, initial);

        Ok(())
    }
}
