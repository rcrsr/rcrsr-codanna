// Gateway for CLI-related integration tests

#[path = "cli/support.rs"]
mod support;

#[path = "cli/common.rs"]
mod common;

#[path = "cli/test_plugin_commands.rs"]
mod test_plugin_commands;

#[path = "cli/test_mcp_index_info_remote_status.rs"]
mod test_mcp_index_info_remote_status;

#[path = "cli/test_serve_proxy_discovery.rs"]
mod test_serve_proxy_discovery;

#[path = "cli/test_idle_shutdown.rs"]
mod test_idle_shutdown;

#[path = "cli/test_serve_registry.rs"]
mod test_serve_registry;

#[path = "cli/test_spawn_timeout_dedup.rs"]
mod test_spawn_timeout_dedup;

#[path = "cli/test_spawn_exit_detection.rs"]
mod test_spawn_exit_detection;

#[path = "cli/test_workspace_disappears.rs"]
mod test_workspace_disappears;

#[path = "cli/test_backing_reap.rs"]
mod test_backing_reap;

#[path = "cli/test_mcp_exit_code_matrix.rs"]
mod test_mcp_exit_code_matrix;

#[path = "cli/test_mcp_line_convention.rs"]
mod test_mcp_line_convention;

#[path = "cli/test_mcp_call_metadata_matrix.rs"]
mod test_mcp_call_metadata_matrix;

#[path = "cli/test_emission_version_gate.rs"]
mod test_emission_version_gate;

#[path = "cli/test_file_path_portable.rs"]
mod test_file_path_portable;

#[path = "cli/test_index_seed_ownership.rs"]
mod test_index_seed_ownership;

#[path = "cli/test_fields_projection.rs"]
mod test_fields_projection;

#[path = "cli/test_index_force_invalid_path.rs"]
mod test_index_force_invalid_path;

#[path = "cli/test_serve_stdio_dual_generation.rs"]
mod test_serve_stdio_dual_generation;

#[path = "cli/test_serve_http_sessionless.rs"]
mod test_serve_http_sessionless;

#[path = "cli/test_mcp_test_client_generation.rs"]
mod test_mcp_test_client_generation;

#[path = "cli/test_version_stamp.rs"]
mod test_version_stamp;

/// `codanna dump` envelope stream and stale gate
#[path = "cli/test_dump.rs"]
mod test_dump;

/// `codanna ls` merged server/proxy listing
#[path = "cli/test_ls.rs"]
mod test_ls;
