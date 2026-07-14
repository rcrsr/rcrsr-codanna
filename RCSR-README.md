# rcrsr-codanna — Fork Modifications

This repository is a fork of [`bartolli/codanna`](https://github.com/bartolli/codanna).
This file is the single index of every change the fork carries on top of its
upstream base, so a reader (or a future rebase) can see at a glance what is ours
and why it exists.

- **Upstream base:** `codanna` v0.9.23
- **Fork version:** `0.9.23+rcrsr.1` (see [Versioning](#versioning))
- **Upstream remote:** `git remote add upstream https://github.com/bartolli/codanna.git`

Everything below is a delta against upstream `v0.9.23`. Where a change is on an
open PR rather than merged, its PR link is noted.

## Versioning

The fork version is `<upstream base>+rcrsr.<N>`, currently `0.9.23+rcrsr.1`.

- `<upstream base>` is the upstream release the fork is built from.
- `N` increments **once per private addition** landed on that base. It resets to
  `.1` when the fork is rebased onto a new upstream release.
- `+rcrsr.N` is semver **build metadata**, which is ignored in version
  precedence: every `0.9.23+rcrsr.N` compares *equal* to a bare `0.9.23`. A
  `-rcrsr` *pre-release* was deliberately rejected because it would order the
  fork *below* the upstream release it strictly extends.

The marker rides `CARGO_PKG_VERSION`, so `codanna --version` and the MCP
`initialize` handshake identity both report it — an MCP client can see which
build it is talking to. `Cargo.toml` documents the convention inline.

## Modifications

| # | Area | Change | Kind | Status |
|---|------|--------|------|--------|
| 1 | `serve` | Proxy mode with HTTP/HTTPS backing-server discovery | Feature | [PR #1](https://github.com/rcrsr/rcrsr-codanna/pull/1) |
| 2 | Build | `+rcrsr.N` fork version marker | Chore | On branch |
| 3 | Audit | Deterministic parser audit reports (no wall-clock stamp) | Fix | On branch |
| 4 | Tooling | Track `.claude/` tool configs; ignore `internal/`, `conduct/` | Chore | On branch |

### 1. `serve --proxy` — proxy mode with backing-server discovery

`codanna serve --proxy` is a stdio↔HTTP delegate that discovers a running
backing MCP server for a workspace, or spawns one if absent, so multiple MCP
clients converge on a single shared backing server per workspace instead of each
launching its own.

- **Discovery protocol** (`src/serve_discovery.rs`): a `ServeRecord {pid, port,
  scheme}` published as `.codanna/serve.json`, guarded by an `http.lock`
  `O_EXCL` single-flight so concurrent proxies converge on one backing server
  rather than racing to spawn duplicates. The record directory is derived from
  the workspace root (`resolve_workspace_root` + `discovery_dir`) so the writer
  and reader agree under any custom `index_path`.
- **HTTPS backing servers** participate: `serve --https` binds its own listener
  to observe its real port under `--bind :0`, publishes `scheme: https`, and the
  proxy dials the recorded scheme. HTTPS upstreams are reached through a client
  pinned to codanna's persisted self-signed cert via `tls_certs_only` — no
  system roots, no verification bypass. `serve_tls::cert_paths` is the single
  source of the cert location so writer and pinner cannot drift.
- **Cargo:** rmcp's `StreamableHttpClient` is implemented for reqwest 0.13, a
  different crate instance than the direct 0.12 dep, so a renamed `rmcp_reqwest`
  handle is added with the `rustls-no-provider` TLS feature (not `rustls`) to
  avoid a ring + aws-lc-rs provider ambiguity that panics at runtime.

**Key files:** `src/mcp/proxy.rs`, `src/serve_discovery.rs`, `src/serve_tls.rs`,
`src/cli/commands/serve.rs`, `src/mcp/{http_server,https_server,mod}.rs`,
`src/config/*`, `src/main.rs`. Tests in `tests/cli/test_serve_proxy_discovery.rs`.

### 2. `+rcrsr.N` fork version marker

Marks fork builds so they are distinguishable from an upstream build at a glance
and over the MCP handshake. See [Versioning](#versioning) for the convention and
the rationale for build metadata over a pre-release suffix.

**Key files:** `Cargo.toml`, `Cargo.lock`.

### 3. Deterministic parser audit reports

The parser audit generators wrote a `*Generated: <UTC now>*` header into the
tracked `AUDIT_REPORT.md`, `GRAMMAR_ANALYSIS.md`, and `node_discovery.txt` files.
Because those files are tracked, every `cargo test` run rewrote 42 of them with a
new timestamp and left the working tree dirty — churn that buried real coverage
changes in review. Dropping the stamp makes the output a pure function of the
grammar and parser, so a diff appears only when coverage actually changes. No
provenance is lost — git already records when a file was generated.
`format_utc_timestamp` (`src/io/format.rs`) stays; it is public API with tests.

**Key files:** `src/parsing/*/audit.rs`, `contributing/parsers/**`,
`tests/exploration/abi15_grammar_audit/helpers.rs`.

### 4. Tooling config tracking

Tracks the Conduct/Checkmate tool configs (`.claude/checkmate.json`,
`.claude/conduct.json`) and ignores fork-local working directories (`internal/`,
`conduct/`). Session transcripts under `.claude/transcripts/` stay untracked via
that directory's own `.gitignore`.

**Key files:** `.gitignore`, `.claude/checkmate.json`, `.claude/conduct.json`.

## Maintaining this file

- Add a row to the table and a subsection for every new fork-private change,
  and bump `+rcrsr.N` in `Cargo.toml` per the [Versioning](#versioning) rule.
- On rebase onto a new upstream release: update the **Upstream base**, reset the
  version to `<new base>+rcrsr.1`, and drop any modification that upstream has
  since absorbed.
