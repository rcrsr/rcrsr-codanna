//! Stdio transport that answers bare `server/discover` probes.
//!
//! Shared by every stdio-serving entry point (`codanna serve`, the degraded
//! stale-index server, and the stdio<->HTTP proxy) so all three observe the
//! same back-compat probe wire shape.

/// Stdio transport that answers bare `server/discover` probes.
///
/// The 2026-07-28 back-compat probe arrives with no `_meta`; rmcp
/// deserializes it as a CustomRequest and `serve()` exits with
/// `ExpectedInitializeRequest` before dispatch. Discover requests that
/// carry `_meta` -- and every other message of either protocol
/// generation -- pass through untouched; rmcp serves both natively.
/// Probes are answered with the same `DiscoverResult` the native
/// handler returns, so both discover forms observe one wire shape.
/// After the first forwarded line every byte passes untouched. Writing
/// to stdout here is safe: rmcp produces no output before its first
/// inbound message.
pub(crate) fn probe_tolerant_stdio(
    discover_result: serde_json::Value,
) -> (tokio::io::DuplexStream, tokio::io::Stdout) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const BUFFER_BYTES: usize = 64 * 1024;
    let (mut inbound, transport_side) = tokio::io::duplex(BUFFER_BYTES);

    tokio::spawn(async move {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut line = String::new();
        let mut handoff = false;

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }

            if !handoff {
                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
                    let bare_probe = msg.get("method").and_then(|m| m.as_str())
                        == Some("server/discover")
                        && msg
                            .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
                            .is_none();
                    if bare_probe {
                        // Notification-form probes carry nothing to answer.
                        if let Some(id) = msg.get("id") {
                            let response = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": discover_result,
                            });
                            let mut stdout = tokio::io::stdout();
                            // Best-effort: a failed write means the client is
                            // gone; the next read observes EOF and the task
                            // ends.
                            let _ = stdout.write_all(format!("{response}\n").as_bytes()).await;
                            let _ = stdout.flush().await;
                        }
                        continue;
                    }
                }
                handoff = true;
            }

            if inbound.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    (transport_side, tokio::io::stdout())
}
