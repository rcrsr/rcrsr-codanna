# rcrsr-codanna — Fork Changes

This is a fork of [`bartolli/codanna`](https://github.com/bartolli/codanna). This
file lists what the fork adds or changes for you as a user, on top of its
upstream base. For the how, see the commit history.

- **Upstream base:** the latest `codanna` release the fork is built on
- **Fork build:** the upstream version with a `+rcrsr.N` suffix (see [Identifying the fork](#identifying-the-fork))

## Contents

- [Installing the fork](#installing-the-fork)
- [Improvements](#improvements)
  - [Proxy mode: one backing server per workspace](#proxy-mode-one-backing-server-per-workspace)
    - [Idle shutdown](#idle-shutdown)
    - [Configuration](#configuration)
    - [Ports](#ports)
    - [Hot-reload notifications through the proxy](#hot-reload-notifications-through-the-proxy)
  - [Reindexing on demand (`reindex` MCP tool)](#reindexing-on-demand-reindex-mcp-tool)
    - [Arguments](#arguments)
    - [Concurrency contract](#concurrency-contract)
  - [Catch-up reindex on watch-queue overflow](#catch-up-reindex-on-watch-queue-overflow)
    - [Configuration](#configuration-1)
  - [`ignore_patterns` now excludes files during indexing](#ignore_patterns-now-excludes-files-during-indexing)
  - [Document collection controls (`search_documents`)](#document-collection-controls-search_documents)
    - [Per-collection default visibility (`default` / `--no-default`)](#per-collection-default-visibility-default----no-default)
    - [Negated glob patterns in collection `patterns`](#negated-glob-patterns-in-collection-patterns)
    - [Multi-select `--collection` / `--exclude-collection`](#multi-select---collection----exclude-collection)
    - [Clarified tool descriptions: `semantic_search_docs` vs `search_documents`](#clarified-tool-descriptions-semantic_search_docs-vs-search_documents)
  - [MCP tool enhancements for agent workflows](#mcp-tool-enhancements-for-agent-workflows)
    - [Structured JSON output (`output_format`)](#structured-json-output-output_format)
    - [Batch symbol lookup (`find_symbols`)](#batch-symbol-lookup-find_symbols)
    - [Canonical `name` parameter across symbol tools](#canonical-name-parameter-across-symbol-tools)
    - [Test/production classification on `find_callers`](#testproduction-classification-on-find_callers)
    - [Symbol-scoped reads (`get_file_outline`, `read_symbol`)](#symbol-scoped-reads-get_file_outline-read_symbol)
    - [Slimmer `analyze_impact`](#slimmer-analyze_impact)
- [Identifying the fork](#identifying-the-fork)

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

# Improvements

The sections below are the fork's additions over upstream codanna — new
capabilities and behavior changes you get on top of the upstream base.

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
  serializing there. This holds through the vector storage layer underneath
  too: `ConcurrentVectorStorage::read_vector` takes its inner lock shared in
  the common (already-mapped) case, so concurrent similarity scoring no
  longer serializes on an exclusive vector-storage lock, and the embedding
  call ahead of it runs inside `spawn_blocking` rather than directly on the
  async task, so it no longer blocks the runtime worker thread while it
  runs. Concurrent embedding generation itself still serializes — one
  `FastEmbedGenerator` holds a single `TextEmbedding` behind one `Mutex`
  (`src/vector/embedding.rs`), so only one caller can run inference at a
  time — but callers now queue on a blocking-pool thread instead of stalling
  the async runtime.
- **A force reindex's brief write-lock phases** (see above): phase 1 and
  phase 3 each hold the index write lock briefly; the walk in between runs
  off-lock. While the walk is in flight, readers may transiently observe a
  repopulating index (some symbols reindexed, others not yet) until phase 3
  completes.

**Concurrent code reindexes are serialized, not queued.** Only one
`reindex` run (any call that reaches the three-phase orchestration above —
scoped `paths`, a full `force: true` rebuild, or the watcher's own catch-up
reindex on queue overflow) may be in flight against an index at a time. A
second call that arrives while one is still running is rejected immediately
rather than being queued or allowed to race the first: it gets a
`REINDEX_IN_PROGRESS` error —
"Another full reindex is already in progress; retry shortly. Wait for the
current reindex to finish, then retry. Avoid triggering concurrent full
reindexes on the same index." — which is a client-visible, retryable
condition, not an internal fault. Simply retry the call once the earlier
reindex has finished ([issue #44](https://github.com/rcrsr/rcrsr-codanna/issues/44)).
This also protects a `reindex(force: true)` call from racing the watcher's
catch-up reindex below: whichever one starts second is rejected rather than
one clearing the index out from under the other's in-flight work.

**Known limitation — `reindex documents:true` holds a write guard per
collection.** The `reindex` tool's document pass takes the same exclusive
write guard as the auto-sync above, scoped per collection (acquired and
dropped once per collection rather than once for the whole reindex). Each
collection's own work — reading files from disk, committing to Tantivy, and
generating embeddings — runs inside `spawn_blocking`, so it no longer blocks
an async runtime worker thread; unrelated async work continues to make
progress while a reindex is in flight. The write guard itself, however, is
still held for that collection's full duration (`index_collection` needs
`&mut DocumentStore`), so document searches against *that* collection wait
until it finishes. This is bounded to one collection at a time rather than
the entire reindex, but is not "brief" in the same sense as the two
operations above.

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
- If a catch-up loses the race to an in-flight `reindex` MCP call (see
  [Concurrency contract](#concurrency-contract) above), that rejection does
  not count against the bounded retry budget and does not clear the stale
  marker — it is not treated as a failure, since the index is already being
  brought up to date by the other reindex. The catch-up simply re-fires after
  the cooldown and finds the index current.
- If that rejection repeats for many consecutive cooldowns (roughly a minute),
  a `WARN`-level log is emitted noting that another reindex appears wedged and
  a restart may be needed — normal handoffs resolve within a cooldown or two,
  so a sustained streak is a signal worth surfacing above debug logging.
- That `WARN` only fires when the watcher is the one being rejected. A reindex
  that wedges with no watcher running (or with no file activity to trigger a
  catch-up) is covered separately by a watchdog on the reindex walk itself: if
  the walk runs longer than ten minutes, an `ERROR` is logged naming the elapsed
  time, noting that every further reindex is being rejected with
  `REINDEX_IN_PROGRESS` meanwhile, and that a process restart is currently the
  only recovery. The watchdog re-logs periodically while the walk stays stuck.
  It is observability only — it does **not** cancel the walk or release the
  serialization gate. The walk runs on a blocking thread that cannot be
  interrupted, and releasing the gate while that thread is still writing would
  re-open the very race the gate exists to prevent, so holding it is correct.
  Recovering a genuinely wedged reindex still requires a restart.

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

## `ignore_patterns` now excludes files during indexing

`indexing.ignore_patterns` in `.codanna/settings.toml` previously deserialized
but was never consulted by any walk — upstream, setting it had no effect on
what got indexed ([issue #22](https://github.com/rcrsr/rcrsr-codanna/issues/22)).
The fork wires it into every walk (`codanna index`, `--dry-run`, incremental
reindex, and watch-triggered reindex).

`ignore_patterns` uses the **same gitignore dialect as `.codannaignore`**:
`!` negation, trailing `/` for directory-only matches, `**`, and the usual
anchoring rules all apply. Patterns are additive to `.gitignore`/`.codannaignore`
and are applied after them, so a leading `!` in `ignore_patterns` can only
re-include a file excluded by an *earlier* `ignore_patterns` entry — it cannot
re-include a file already excluded by `.gitignore` or `.codannaignore`. If you
need to re-include something a gitignore file excludes, do it in that
gitignore file (a custom `.codannaignore` outranks `.gitignore` there).

```toml
[indexing]
ignore_patterns = ["fixtures/**", "!fixtures/keep.rs"]
```

The four patterns codanna used to hard-code as the default (`target/**`,
`node_modules/**`, `.git/**`, `*.generated.*`) are no longer part of the
`Default` for `IndexingConfig` — new `settings.toml` files ship
`ignore_patterns = []`. This is a no-op in practice: those four patterns are
already excluded by the default `.codannaignore` that `codanna init` writes.
Existing `settings.toml` files are left untouched; any patterns already on
disk in `ignore_patterns` now take effect.

## Document collection controls (`search_documents`)

The fork adds per-collection default-visibility, negated glob patterns for
collection file selection, and multi-select filtering to `search_documents`
and `codanna documents search`.

### Per-collection default visibility (`default` / `--no-default`)

Each collection in `[documents.collections.<name>]` (`.codanna/settings.toml`)
now takes an optional `default` key:

```toml
[documents.collections.internal-notes]
paths = ["docs/internal"]
patterns = ["**/*.md"]
default = false   # opt this collection out of unscoped searches
```

`default` defaults to `true`, so existing collections (and any `settings.toml`
written before this key existed) keep the prior always-searched behavior with
no changes required. When it is set to `false`, the collection is skipped by a
`search_documents` call that names no `collection` at all — but it is still
searched if you name it explicitly. This lets you keep, say, an internal-only
or scratch collection out of an agent's general-purpose queries while still
letting a caller reach it on demand.

Set it from the CLI when creating a collection with `codanna documents
add-collection --no-default`; the human-readable `codanna documents list`
output annotates non-default collections with `(non-default)`. The `list
--json` output is a plain array of collection names and does not currently
carry default/non-default information.

### Negated glob patterns in collection `patterns`

`patterns` entries for a collection now support gitignore-style `!`-prefixed
negation, resolved with the same `ignore` crate machinery (`ignore::overrides`)
used elsewhere in codanna, instead of a plain `glob::glob` union:

```toml
[documents.collections.docs]
paths = ["docs"]
patterns = ["**/*.md", "!docs/internal/**", "!**/DRAFT-*.md"]
```

A later `!`-prefixed pattern actually excludes files matched by an earlier
pattern (not merely flags them) — the same negation semantics as
`.codannaignore`/`ignore_patterns`. Non-negated pattern sets behave exactly as
before: every file under the collection's `paths` matching any pattern is
indexed.

### Multi-select `--collection` / `--exclude-collection`

`search_documents` and `codanna documents search` accept more than one
collection at once:

- `codanna documents search --collection docs --collection api-notes "query"`
  searches the union of the named collections (allowlist).
- `codanna documents search --exclude-collection scratch "query"` searches
  every collection except the named one(s) (denylist), on top of whatever the
  allowlist and default-visibility resolve to.
- Both flags are repeatable. Naming a collection explicitly with `--collection`
  always searches it, even if its `default` key is `false`.

Over MCP, `search_documents`'s `collection` argument now accepts either a bare
string (unchanged, for existing clients) or a JSON array of strings for
multi-select; a new `exclude_collections` argument (array of strings) is the
MCP equivalent of `--exclude-collection`. `codanna mcp search_documents` on the
CLI accepts the same `collection:`/`exclude_collections:` forms, including a
JSON array value.

### Clarified tool descriptions: `semantic_search_docs` vs `search_documents`

The two tools search different corpora and were easy to confuse from their
descriptions alone:

- `semantic_search_docs` searches **doc comments extracted from code
  symbols** (the same corpus as upstream) — its description now says so
  explicitly and points to `search_documents` for markdown files.
- `search_documents` searches **indexed markdown document collections**
  (`[documents.collections.*]`) — its description now says so explicitly and
  points back to `semantic_search_docs` for doc comments.

This is a documentation-only change (tool names, arguments, and behavior are
unchanged); it exists so an agent choosing between the two tools from
`list_tools` output alone picks the right one on the first try.

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
