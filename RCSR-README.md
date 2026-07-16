# rcrsr-codanna — Fork Changes

This is a fork of [`bartolli/codanna`](https://github.com/bartolli/codanna). This
file lists what the fork adds or changes for you as a user, on top of its
upstream base. For the how, see the commit history.

- **Upstream base:** the latest `codanna` release the fork is built on
- **Fork build:** the upstream version with a `+rcrsr.N` suffix (see [Identifying the fork](#identifying-the-fork))

## Installing the fork

The fork is distributed through its own [GitHub Releases](https://github.com/rcrsr/rcrsr-codanna/releases),
not crates.io or Homebrew. Each release is cut by pushing a `v<version>` tag; CI
builds Linux, macOS (x64 + arm64), and Windows binaries and attaches them.

Prebuilt binary via [`cargo binstall`](https://github.com/cargo-bins/cargo-binstall)
(reads this repo's binstall metadata, so it must be pointed at the fork with `--git`):

```bash
cargo binstall --git https://github.com/rcrsr/rcrsr-codanna codanna
```

Plain `cargo binstall codanna` resolves the **upstream** crate from crates.io — use
the `--git` form above to get the fork.

From source:

```bash
cargo install --git https://github.com/rcrsr/rcrsr-codanna --all-features codanna
```

Or download a platform archive directly from the [releases page](https://github.com/rcrsr/rcrsr-codanna/releases)
and put the `codanna` binary on your `PATH`. The binary is named `codanna` (same as
upstream), so it will shadow an upstream install on the same `PATH`.

## Proxy mode: one backing server per workspace

`codanna serve --proxy` lets several MCP clients share a single backing server
for a workspace instead of each starting its own. Point every client at the
proxy: the first one starts (or discovers) a backing server for the workspace,
and the rest attach to it.

The backing server is started as a detached background process and keeps running
after the clients disconnect, so the next client reattaches to the warm index
instead of paying startup again. It is not shut down automatically — stop it
yourself when you're done with the workspace (or leave it running).

### Configuration

The workspace must be initialized (`codanna init` writes `.codanna/settings.toml`);
the proxy refuses to auto-spawn a backing server for a tree that has no config.
Proxy behavior is controlled by the `[server]` section of that file:

```toml
[server]
auto_spawn = true       # let the proxy start a backing server when none is found;
                        # set false to require starting `codanna serve --http --watch` yourself
spawn_timeout_ms = 8000 # how long to wait for a spawned server to become ready
health_poll_ms = 100    # how often to poll for readiness while waiting
```

The defaults shown above apply when the keys are absent, so an initialized
workspace works with no `[server]` block at all.

### Ports

When the proxy auto-spawns a backing server, it binds a random free port on
`127.0.0.1` (the OS assigns it). You never choose or need that port: the server
records it, and the proxy reads the record to connect. Your MCP clients only ever
talk to the proxy over stdio, so nothing on your side depends on the number.

If you start a backing server yourself instead (`codanna serve --http` /
`--https`), it uses the normal bind address — `--bind`, or `[server] bind` in
`settings.toml`, defaulting to `127.0.0.1:8080` for HTTP and `127.0.0.1:8443` for
HTTPS. All backing servers listen on loopback only; nothing is exposed off-host.

```bash
codanna serve --proxy
```

Use it when more than one tool or editor talks to codanna for the same project
and you don't want a separate index loaded into memory for each. Both HTTP and
HTTPS backing servers are supported; with `--https` the connection is verified
against codanna's own certificate.

If you only ever run a single client, you don't need this — plain `codanna serve`
is unchanged.

## Reindexing on demand (`reindex` MCP tool)

The fork exposes reindexing as a first-class MCP tool named `reindex`,
discoverable through `list_tools` in every serve mode — stdio, HTTP, HTTPS, and
proxy. Upstream reindexing is a CLI-only operation, so an MCP client (an editor
or agent) could not trigger it over the protocol; with the fork it can, without
restarting the server or reloading the index.

A client calls it like any other tool:

```jsonc
// reindex everything configured (incremental — unchanged files are skipped)
{ "name": "reindex", "arguments": {} }

// reindex specific files and/or directories
{ "name": "reindex", "arguments": { "paths": ["src/foo.rs", "src/bar/"] } }

// force a full clear-and-rebuild
{ "name": "reindex", "arguments": { "force": true } }
```

It is also reachable from the CLI as `codanna mcp reindex`.

### Arguments

- `paths` (optional array of strings) — files or directories to reindex. Omit to
  reindex all configured `indexed_paths`. Explicit paths must resolve **inside
  the workspace root**; anything outside is rejected. At most 1024 paths per call.
- `force` (optional bool, default `false`) — for a **full** reindex (no `paths`),
  clears the entire index before rebuilding it. For **scoped** `paths`, re-indexes
  just those paths without a global clear: files are re-parsed even when their
  content hash is unchanged, and directories bypass the incremental hash-skip.

The call returns a short summary — files reindexed, symbols, and elapsed
milliseconds. Like every other tool, `reindex` accepts `output_format: "json"`
for a structured `Envelope` response instead of the default text summary.

Reindexing does not block reads: the walk-and-parse work runs without holding the
index write lock, so concurrent read-only tools (`find_symbol`, `search_symbols`,
`semantic_search_docs`, and the rest) keep serving while a reindex is in flight.

## Catch-up reindex on watch-queue overflow

When you run a watching server (`codanna serve --watch`, in any serve mode), the
OS file-watch backend has a bounded event queue. A bulk operation — `git rebase`,
`git checkout` across many files, a branch switch, a large `git pull` — can change
more files at once than the queue holds, and the backend drops events (an inotify
`IN_Q_OVERFLOW`, or the equivalent on macOS/Windows). Upstream codanna silently
misses those changes: the index stays out of sync with disk until you reindex by
hand.

The fork detects the overflow signal and, once file activity settles, fires a
single catch-up reindex automatically so the index re-converges with disk without
any manual step. Behavior details:

- It waits for a quiet window after the overflow before reindexing, and coalesces
  a burst of overflow signals (a rebase with hook pauses, say) into one catch-up
  rather than firing mid-operation.
- The catch-up runs off the watcher's event loop, so incoming file events keep
  draining while it works — a long reindex can't cause a second overflow.
- If a catch-up fails (transient lock/IO error), staleness is kept and retried on
  the next quiet window (bounded) instead of being silently dropped.
- Successive catch-ups are throttled by a short cooldown, so sustained bursty git
  activity can't thrash repeated full rebuilds.

### Configuration

It is **on by default**. Controlled by the `[file_watch]` section of
`.codanna/settings.toml`:

```toml
[file_watch]
refresh_on_overflow = true  # catch-up reindex on watch-queue overflow (default: true)
                            # set false to restore upstream behavior (missed changes stay missed)
```

The `churn_threshold` key is parsed and accepted but **reserved** — it is not yet
consumed by the watcher and has no effect (setting it to a non-zero value logs a
one-time startup warning).

If you don't run with `--watch`, this feature is inert; the `reindex` tool above
is the way to re-sync on demand.

## MCP tool enhancements for agent workflows

The fork extends the MCP tool surface so agents can machine-parse results, batch
lookups, and read symbol bodies without pulling whole files. Every change is
additive — omit the new parameters and behavior is identical to upstream.

### Structured JSON output (`output_format`)

Every MCP tool accepts `output_format: "text" | "json"` (default `"text"`, so the
compact prose output is unchanged). With `"json"`, the tool emits a structured
envelope carrying `status`, `code`, `exit_code`, `message`, `data`, and `meta`
(with a `schema_version`). The status taxonomy distinguishes `success`,
`not_found`, `ambiguous`, and `error` — so a consumer can tell "no such symbol"
apart from "the query failed" instead of parsing prose. This is the same envelope
the CLI `--json` path already emitted; the two paths now share one builder per
tool.

### Batch symbol lookup (`find_symbols`)

A new `find_symbols` tool takes `names: [ ... ]` and returns a per-name map —
each entry is `found` (with location, kind, signature, line range), `not_found`,
or `ambiguous` (with candidates). One round-trip instead of one per name. Batches
are capped at 1024 names, matching `reindex`.

### Canonical `name` parameter across symbol tools

`find_symbol`, `get_calls`, `find_callers`, and `analyze_impact` now all accept a
single canonical `name` parameter. The old parameter names (`function_name` on
`get_calls`/`find_callers`, `symbol_name` on `analyze_impact`) still work as
serde aliases, so no existing client breaks. `find_symbol` also gains a typed
`symbol_id` parameter (previously only the `symbol_id:NNN` string prefix worked).

### Test/production classification on `find_callers`

`find_callers` tags each caller with a `role` of `production` or `test`, and
accepts `filter: all | production | test` (default `all`) plus `count_only: bool`
(returns totals with a per-role breakdown). "Is this safe to delete" becomes
"zero *production* callers" without a manual second grep over test directories.
Classification is a path heuristic; the patterns are configurable:

```toml
[caller_classification]
test_path_patterns = ["tests/", "/test/", "*_test.*", "test_*.py", "*.spec.*", "__tests__/"]
```

### Symbol-scoped reads (`get_file_outline`, `read_symbol`)

Two new tools let an agent judge and read a symbol without loading its whole file:

- `get_file_outline(path)` — every symbol in a file with kind, signature,
  visibility, and start/end lines.
- `read_symbol(name | symbol_id)` — the exact source span of one symbol plus its
  metadata. It guards against a stale index: if the file's current hash differs
  from what was indexed, it reports that instead of returning a possibly-shifted
  span.

### Slimmer `analyze_impact`

`analyze_impact` gains three parameters: `count_only: bool` (just the symbol count
and distinct-file count, no listing — for scope gates), `max_results` (truncates
the listing and flags `truncated` in the envelope meta), and
`group_by: kind | file` (default `kind`, the current behavior).

## Identifying the fork

Fork builds carry a `+rcrsr.N` suffix on the upstream version, so you can tell a
fork build from an upstream one:

```bash
codanna --version        # e.g. codanna <upstream-version>+rcrsr.N
```

MCP clients see the same string in the `initialize` handshake, so a connected
client can confirm which build it is talking to. The `+rcrsr.N` suffix is build
metadata — it does not change how the version compares, so a fork build counts as
the same release as the upstream version it is built on. `N` is just a running
count of fork additions on the current upstream base.

Everything not listed here behaves as it does in upstream codanna.
