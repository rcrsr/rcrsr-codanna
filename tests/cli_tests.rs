// Gateway for CLI-related integration tests

#[path = "cli/support.rs"]
mod support;

#[path = "cli/test_plugin_commands.rs"]
mod test_plugin_commands;

#[path = "cli/test_mcp_index_info_remote_status.rs"]
mod test_mcp_index_info_remote_status;

#[path = "cli/test_serve_proxy_discovery.rs"]
mod test_serve_proxy_discovery;

#[path = "cli/test_idle_shutdown.rs"]
mod test_idle_shutdown;

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
