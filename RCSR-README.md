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
instead of paying startup again. By default it stays up until you stop it (or the
host reboots); set `idle_shutdown_minutes` (see below) to have it exit on its own
after a spell of inactivity — the next tool call transparently respawns it.

### Idle shutdown

**Scope: `--http` backing servers only.** The idle timer (activity tracking,
the poll loop, and the shutdown trigger) is implemented in `serve_http`; a
backing server started with `--https` has none of this plumbing, so
`idle_shutdown_minutes` is silently inert for it. If you auto-spawn backing
servers through the proxy (the common case), they are started with `--http`
and this section applies as written. If you manually start a backing server
with `codanna serve --https`, `idle_shutdown_minutes` has no effect on it.

By default a backing server runs indefinitely, so every workspace you touch
accumulates a resident process. Set `idle_shutdown_minutes` in `[server]` to a
non-zero value and the (`--http`) server exits cleanly after that many minutes
with no MCP request activity, removing its `.codanna/serve.json` record exactly
as a Ctrl+C shutdown does. The next tool call through the proxy finds no record
and auto-spawns a fresh server (paying only startup latency), so idle shutdown
is transparent to clients.

Only real inbound MCP requests count as activity — SSE keep-alive pings do not
reset the idle clock, so a merely *connected* client does not keep the server
alive forever. The default is `0` (never shut down), preserving upstream
behavior.

### Configuration

The workspace must be initialized (`codanna init` writes `.codanna/settings.toml`);
the proxy refuses to auto-spawn a backing server for a tree that has no config.
Proxy behavior is controlled by the `[server]` section of that file:

```toml
[server]
auto_spawn = true          # let the proxy start a backing server when none is found;
                           # set false to require starting `codanna serve --http --watch` yourself
spawn_timeout_ms = 8000    # how long to wait for a spawned server to become ready
health_poll_ms = 100       # how often to poll for readiness while waiting
idle_shutdown_minutes = 0  # exit the backing server after N idle minutes (0 = never)
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
Note that `idle_shutdown_minutes` (above) only applies to `--http` backing
servers — a manually-started `--https` server runs indefinitely regardless of
that setting.

```bash
codanna serve --proxy
```

Use it when more than one tool or editor talks to codanna for the same project
and you don't want a separate index loaded into memory for each. Both HTTP and
HTTPS backing servers are supported; with `--https` the connection is verified
against codanna's own certificate.

### Hot-reload notifications through the proxy

Codanna's custom hot-reload notifications (`notifications/codanna/file-reindexed`,
`file-created`, `file-deleted`, `index-reloaded`) are forwarded verbatim from the
backing server to each stdio client, so a client behind the proxy stays as
hot-reload-aware as one connected directly. Notifications the backing server
emits before a client finishes its `initialize` handshake are buffered (up to the
last 100, oldest dropped on overflow) and flushed once the client is ready,
rather than being lost in the connection window.

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

// also refresh every configured document collection (discovers new markdown files)
{ "name": "reindex", "arguments": { "documents": true } }
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
- `documents` (optional bool, default `false`) — in addition to the code index,
  reindex every configured document collection, discovering markdown files added
  since the last run (upstream reindexing and the watcher only refresh files
  already in a collection). The code index is always reindexed; this flag adds the
  document pass on top. Returned totals report the two separately, and a failing
  collection surfaces as an error naming it rather than being silently skipped.

The call returns a short summary — files reindexed, symbols, and elapsed
milliseconds (plus per-collection document totals when `documents: true`). Like
every other tool, `reindex` accepts `output_format: "json"` for a structured
`Envelope` response instead of the default text summary.

Reindexing does not block reads: the walk-and-parse work runs without holding the
index write lock, so concurrent read-only tools (`find_symbol`, `search_symbols`,
`semantic_search_docs`, and the rest) keep serving while a reindex is in flight.

### Concurrency contract

Read-only MCP tools — including `search_documents` — are safe to call in
parallel from multiple clients, in every `serve` mode (stdio, `--http`,
`--https`, `--proxy`). Only two operations briefly take an exclusive write
guard, and both scope it as narrowly as possible:

- **`search_documents`'s collection auto-sync.** Every call first checks
  configured document collections for file changes under a brief write
  guard, scoped to just that scan, then drops it before searching.
  `DocumentStore::search` itself only needs read access, so the search step
  runs under a read guard and concurrent `search_documents` calls make
  progress against each other at the `DocumentStore` level instead of
  serializing there.
- **A force reindex's brief write-lock phases** (see above): phase 1 and
  phase 3 each hold the index write lock briefly; the walk in between runs
  off-lock. While the walk is in flight, readers may transiently observe a
  repopulating index (some symbols reindexed, others not yet) until phase 3
  completes.

**Known limitation — vector-layer read lock.** The "make progress against
each other" claim above is scoped to `DocumentStore`'s own locking; it does
not extend through the vector storage layer underneath. When an embedding
generator is configured (the production default), `DocumentStore`'s
similarity scoring reads vectors via `ConcurrentVectorStorage::read_vector`,
which takes an *exclusive* lock per call (see that method's doc comment in
`src/vector/storage.rs`) rather than a shared one, so concurrent
`search_documents` calls serialize on that lock while scoring candidates.
Separately, `FastEmbedGenerator::generate_embeddings` runs blocking ONNX
inference under a `std::sync::Mutex` with no `spawn_blocking`, so concurrent
embedding generation also serializes and can block the async runtime worker
thread while it runs. Both are known limitations of the current vector layer,
tracked for a future fix rather than addressed in this change.

**Known limitation — `reindex documents:true` runs synchronously per
collection.** The `reindex` tool's document pass takes the same exclusive
write guard as the auto-sync above, scoped per collection (acquired and
dropped once per collection rather than once for the whole reindex), but each
collection's own work — reading files from disk, committing to Tantivy, and
generating embeddings — still runs synchronously on the async task, without
`spawn_blocking`. A large collection can therefore hold the write guard for
that collection's full duration and block the async runtime worker thread
while doing so. This is bounded to one collection at a time rather than the
entire reindex, but is not "brief" in the same sense as the two operations
above; tracked for a future fix.

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
