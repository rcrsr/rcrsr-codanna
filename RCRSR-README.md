# rcrsr-codanna — Fork Changes

This is a fork of [`bartolli/codanna`](https://github.com/bartolli/codanna). This
file lists what the fork adds or changes for you as a user, on top of its
upstream base. For the how, see the commit history.

- **Upstream base:** the latest `codanna` release the fork is built on
- **Fork build:** the upstream version with a `+rcrsr.N` suffix (see [Identifying the fork](#identifying-the-fork))

## Contents

- [Installing the fork](#installing-the-fork)
  - [Quick install (recommended)](#quick-install-recommended)
  - [Updating](#updating)
  - [Alternatives](#alternatives)
  - [PATH and shadowing](#path-and-shadowing)
- [Improvements](#improvements)
  - [Proxy mode: one backing server per workspace](#proxy-mode-one-backing-server-per-workspace)
    - [Idle shutdown](#idle-shutdown)
    - [Configuration](#configuration)
    - [Ports](#ports)
    - [Hot-reload notifications through the proxy](#hot-reload-notifications-through-the-proxy)
  - [Reindexing on demand (`reindex` MCP tool)](#reindexing-on-demand-reindex-mcp-tool)
    - [Arguments](#arguments)
    - [Concurrency contract](#concurrency-contract)
  - [Catch-up reindex on watch-queue overflow and after downtime](#catch-up-reindex-on-watch-queue-overflow-and-after-downtime)
    - [Startup catch-up](#startup-catch-up)
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

**Before tagging, prepare `CHANGELOG.md`:** rename `## [Unreleased]` to
`## [<version>] - <date>`, where `<version>` is the *full* `Cargo.toml`
version **including the `+rcrsr.N` suffix and its literal `+`** — e.g.
`## [0.12.0+rcrsr.1] - 2026-07-28`. The release body is extracted from the
section whose heading matches that version exactly, and the match is a literal
string compare, so a bare upstream heading (`## [0.12.0]`) will not be found.
Every heading currently in the file predates the fork's release pipeline and is
a bare upstream version, so the file's visible convention is the wrong one to
copy here. A tag push with no matching, non-empty section fails in CI before
anything is built.

To validate the whole pipeline locally before tagging — version derivation,
the release build, packaging and checksums, manifest generation, and an
offline end-to-end run of `scripts/install.sh` — run
`contributing/scripts/test-release-workflow.sh` from the repository root. It
mirrors `.github/workflows/release.yml` and asserts against the shipping
workflow, so it also catches drift between the two. Running the release
workflow via `workflow_dispatch` exercises the same pipeline on CI as a dry
run: it builds and uploads artifacts for inspection but never publishes a
release, even when dispatched against a tag.

### Quick install (recommended)

The installer script downloads the right archive for your platform from the
latest (or a pinned) GitHub release, verifies its checksum, and puts the
`codanna` binary on your `PATH`.

macOS / Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/rcrsr/rcrsr-codanna/main/scripts/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/rcrsr/rcrsr-codanna/main/scripts/install.ps1 | iex
```

Before running the Windows installer, stop any running `codanna serve`
process first — the installer copies the new binary over the old one
(`Copy-Item -Force`), which cannot overwrite a `codanna.exe` that a running
process still has locked.

The script pulls its content from the `main` branch, not a tagged commit, so
the *installer logic* can change between the time you run it and any future
run — only the *binary it downloads* is checksum-verified per release. If you
want the installer script itself pinned to an immutable ref (e.g. for CI or a
provisioning pipeline), replace `main` in the URL above with a specific tag,
such as `v0.12.0+rcrsr.1`:

```bash
curl -fsSL https://raw.githubusercontent.com/rcrsr/rcrsr-codanna/v0.12.0+rcrsr.1/scripts/install.sh | sh
```

Two environment variables configure the installer; both are optional:

- `CODANNA_INSTALL_DIR` — where to install the binary (default: `~/.local/bin`,
  or `%USERPROFILE%\.local\bin` on Windows).
- `CODANNA_VERSION` — install a specific fork version (e.g. `v0.12.0+rcrsr.1`,
  matching the release's `tag_name`) instead of the latest release.

The installer downloads a per-platform archive named
`codanna-<sanitized-version>-<platform>.tar.xz` (`.zip` on Windows). The
archive filename never contains a literal `+` — in the filename only, the
`+` is replaced with `-`, so `0.12.0+rcrsr.1` becomes `0.12.0-rcrsr.1`. The
build metadata itself is preserved; only the separator changes, since a
literal `+` is not portable across every download/extraction tool and
GitHub rewrites release-asset filenames containing it. (The version
still appears with `+rcrsr.N` intact in the release tag itself, e.g.
`v0.12.0+rcrsr.1`, and in `codanna --version` output — only the asset
filename is sanitized.) The installer then verifies the downloaded archive's
`sha256` checksum against the release manifest before extracting it. If
verification fails, the install aborts instead of installing an unverified
binary.

### Updating

Re-run the same install command to upgrade to the latest release — there is
no separate `codanna update` or `codanna upgrade` subcommand. This matches the
model used by installers like rustup, uv, and deno: the one-liner is
idempotent, so running it again simply replaces your existing install with
the current latest (or, with `CODANNA_VERSION` set, a specific pinned)
release.

### Alternatives

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
and put the `codanna` binary on your `PATH` yourself; this is what the
installer script above automates, including the checksum check.

### PATH and shadowing

The binary is named `codanna` (same as upstream), so it will shadow an
upstream install on the same `PATH`. This also applies to the installer's own
target directory: if `CODANNA_INSTALL_DIR` (or its per-platform default, such
as `~/.local/bin`) comes earlier on your `PATH` than wherever an existing
`codanna` lives (e.g. a Homebrew or crates.io install in `/usr/local/bin` or
`~/.cargo/bin`), the fork build will take precedence — and vice versa if it
comes later. Run `which codanna` (or `where.exe codanna` on Windows) after
installing to confirm which binary resolves first, and `codanna --version`
to confirm you're running the fork build (see
[Identifying the fork](#identifying-the-fork)).

## Upstream base

The fork now tracks upstream **v0.12.0** (merged from the prior v0.11.1 base).
Moving the upstream base does not touch the fork build counter, which only ever
counts up (see [Identifying the fork](#identifying-the-fork)).

One upstream v0.10.0 change is user-visible for existing MCP clients:
**unknown `key:value` arguments on an MCP tool call now reject** instead of
being silently ignored — this applies on every surface (positional CLI args,
`--args`, and serve-mode `tools/call`, where the rejection surfaces as
`isError: true`); tool schemas also now advertise `additionalProperties:
false`. A misspelled or stale argument key that previously passed through
unnoticed now fails the call. If you have automation or scripts calling
codanna's MCP tools, check argument names against the current tool schemas
after upgrading.

Upstream v0.10.1 changes what a **stale index** looks like to an MCP client.
When the index was built by a binary with different emission semantics,
`codanna serve` (stdio) no longer fails to start — it completes the MCP
handshake and advertises **zero tools**, with instructions beginning `INDEX
STALE - ALL TOOLS DISABLED` that name `codanna index` as the fix and remind you
to restart the MCP server afterwards. Clients that launch codanna themselves
usually discard its error output, so the old behavior showed up as an opaque
connection failure with no hint of the cause; now the reason arrives over the
protocol. Nothing becomes readable — the refusal is still absolute, the process
still exits with code 7 once the session ends, and running in a terminal still
prints `index emission semantics changed`.

**Fork note — this does not cover proxy mode.** `codanna serve --proxy` (and a
bare `codanna serve` when `mode = "proxy"` is set in `settings.toml`) is exempt
from the staleness check by design, because a proxy holds no index of its own.
It delegates to a backing server started as `codanna serve --http`, which
upstream's degraded-handshake path deliberately excludes, and the fork starts
that backing process with its error output detached. So if a proxy has to start
a *fresh* backing server against a stale index, you get a readiness timeout —
`backing 'codanna serve --http' did not become healthy within …ms` — rather
than the stale-index explanation. An already-running backing server is
unaffected. Run `codanna index` in the workspace to heal it.

Upstream v0.11.1 changes what happens when you ask for output fields that
don't exist. `--fields` now **rejects** unknown field names instead of
silently returning stripped-empty items, and it does so on every surface that
accepts the flag — `codanna mcp <tool> --json --fields`, `codanna retrieve
<query> --json --fields`, and `codanna documents search --json --fields`, all
of which share one rejection path. A rejection is a JSON error envelope on stdout with
`code: INVALID_QUERY` and exit code 2, carrying a hint that lists the
available top-level fields. `--fields` also now understands dotted paths, so
you can project into a nested field instead of only picking top-level keys.
If you have scripts that pass `--fields`, check the field names you're
asking for after upgrading — a misspelling that used to come back as an
empty object now fails the command outright. Note that the accepted names
are derived from the records a tool actually returns, so they differ between
tools: `role` is a valid field on `find_callers`, for instance, but not on
`analyze_impact`. The hint in the rejection always lists what the tool you
called will accept.

**Fork note.** The new rejection applies to the fork-only tools too —
`find_symbols` and `reindex` reject an unknown `--fields` name exactly the
same way. An ambiguous symbol name still exits **3** with `code: AMBIGUOUS`
regardless of what you passed to `--fields`, because the fork decides
ambiguity before it renders anything, so the new exit-2 rejection never
swallows the fork's ambiguity handling.

**Upstream v0.12.0 forces a one-time re-index of every existing index.**
Upstream changed how symbol relationships are emitted, and bumped the
emission-semantics version from 1 to 3 to say so. The gate that guards this is
absolute, not advisory: any index written by an older binary is refused. In a
terminal you get `index emission semantics changed (index: v1, binary: v3)` and
exit code **7**; over stdio MCP you get the degraded zero-tool handshake
described above, with `INDEX STALE - ALL TOOLS DISABLED` in the instructions.
Run `codanna index` once per workspace to heal it, and restart any MCP server
afterwards. This is upstream's intended behavior, not a fork decision — but it
lands on upgrade with no warning beforehand, so re-index before you rely on a
workspace. The proxy-mode caveat in the fork note above applies here too: a
proxy forced to spawn a fresh backing server against a stale index reports a
readiness timeout rather than the stale-index reason.

The reason for the bump is worth knowing, because it changes result counts.
Upstream made cross-file resolution **fail closed**: a call that cannot be tied
to a definition by actual evidence is now left unresolved instead of being
guessed from the first plausible candidate. Import bindings resolve only on an
exact module match or an exactly-one survivor, and a symbol with no module
identity no longer matches everything by accident. Expect relationship counts
to *drop* on re-index — upstream measured -7.6% on one corpus and -48% on
another — with the lost recall being mostly wrong answers. If you have tooling
that asserts on caller or impact counts, re-baseline it against a freshly built
index rather than assuming a regression.

Upstream v0.12.0 also adds `builder_commit` to `index.meta` and to
`get_index_info --json`: the commit the building binary came from, suffixed
`-dirty` when built from a modified tree. It is descriptive only — nothing
reads it back — and it is absent for tarball builds and for indexes written
before the stamp existed. **Fork note:** upstream declares this field on its
own `IndexInfo` in `cli/commands/mcp.rs`; the fork long ago relocated that
struct to `mcp/service.rs`, so the field is carried there instead. The emitted
JSON is the same either way.

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

## Catch-up reindex on watch-queue overflow and after downtime

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
  so a sustained streak is a signal worth surfacing above debug logging. That
  `WARN` is not a single, one-time event: once it first fires, re-emission is
  rate-limited on a widening interval rather than repeating on every
  contention rejection — 10 minutes, then 20, then 40, then capped at hourly
  and staying hourly indefinitely, mirroring the phase-2 watchdog cadence
  described below. It widens and caps, but it never stops recurring while the
  contention persists.
- That `WARN` only fires when the watcher is the one being rejected. A reindex
  that wedges with no watcher running (or with no file activity to trigger a
  catch-up) is covered separately by a watchdog on the reindex walk itself: if
  the walk runs longer than ten minutes, an `ERROR` is logged naming the elapsed
  time, noting that every further reindex is being rejected with
  `REINDEX_IN_PROGRESS` meanwhile, and that a process restart is currently the
  only recovery. The watchdog re-logs on a widening interval while the walk
  stays stuck — 10 minutes, then 20, then 40, then capped at hourly and
  staying hourly indefinitely, so a multi-day wedge stays visible without
  re-paging on a flat ten-minute cadence forever. It is observability only — it
  does **not** cancel the walk or release the serialization gate. The walk runs
  on a blocking thread that cannot be
  interrupted, and releasing the gate while that thread is still writing would
  re-open the very race the gate exists to prevent, so holding it is correct.
  Recovering a genuinely wedged reindex still requires a restart.

### Startup catch-up

The same overflow catch-up machinery also arms once, automatically, the moment
a watching server's event loop starts — not only in response to a later
overflow signal. Files may have changed while the watcher was not running (a
process restart, a machine sleep, a deploy), so the index can already be stale
before any filesystem event is observed; this closes that gap without a
separate code path or a separate config key. It shares the same quiet window,
cooldown, and bounded-retry behavior described above, and it honours the same
`refresh_on_overflow` setting: set it to `false` and neither the overflow
catch-up nor the startup catch-up runs. As with overflow catch-up, this is a
full clear-and-rebuild reindex, so expect degraded/empty MCP query results
until it completes on a large index.

This interacts with proxy mode's `server.idle_shutdown_minutes` (see
[Idle shutdown](#idle-shutdown)): the setting is **`0` (never) by default, so
it's opt-in**, but if you've set it to a positive value on a large workspace,
every auto-respawn of the backing server (after it idles out and a new
request wakes it back up) now also pays for a full startup catch-up rebuild.
A short idle timeout on a large index can therefore turn into
respawn-triggers-rebuild churn rather than a clean, cheap restart; size the
timeout with that cost in mind, or leave it at the default.

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
reindex, and watch-triggered reindex). That now includes upstream v0.12.0's
created-directory handling: when `serve --watch` sees a new directory appear
under a watched root, the subtree it registers watches for and the files it
catches up are decided by the same walk, so `ignore_patterns` prunes them
exactly as it prunes a batch index. A directory you have excluded never gets
watched.

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

**As of upstream v0.12.0 this setting is fork-only.** Upstream resolved the
same issue #22 in the opposite direction — it deleted `ignore_patterns`
outright, on the grounds that nothing consumed it and the settings surface
was promising an exclusion that never happened. That reasoning does not hold
here, because the fork had already made it real. The fork keeps the setting
and its behavior. The practical consequence: a `settings.toml` written for
this fork is not portable to upstream codanna — upstream will load the file
without complaint and silently ignore the key, so anything you exclude only
via `ignore_patterns` would get indexed there. Move those patterns to
`.codannaignore` if you need a config that behaves identically on both.

## Indexing no longer depends on the working directory

Two read paths opened workspace-relative paths as-is, which resolves them
against the process working directory rather than `workspace_root`. The batch
READ stage gets relative paths from the discovery stage, which has to normalize
them to compare against the index's stored rows; single-file re-index gets them
from the watch handler. Both now resolve against `workspace_root` before
opening.

Running `codanna` from the command line was never affected, because the CLI and
the server are launched from the workspace root, where the two agree. It bit
anything that did not do that:

- **Embedding `IndexFacade` in another process.** With `workspace_root` set and
  a different CWD, every file read failed and the run still reported success —
  `index_directory` returned `Ok` with `files_indexed` counted and
  `symbols_found` zero, producing a silently empty index with no error to
  catch.
- **`serve --watch` started from elsewhere.** Every re-index failed with
  `No such file or directory` against a path that plainly existed.

Both are covered by regression tests that fail on the pre-fix behavior: one
asserting a non-empty index when CWD differs from `workspace_root`, and an
end-to-end watcher test that creates a directory under a watched root and
requires the file inside it to become retrievable through the real watch loop.

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
Classification starts from a path heuristic; the patterns are configurable:

```toml
[caller_classification]
test_path_patterns = ["tests/", "/test/", "*_test.*", "test_*.py", "*.spec.*", "__tests__/"]
```

**Rust `#[cfg(test)]` modules are detected.** Every pattern above is
path-shaped, which cannot see Rust's idiomatic inline
`#[cfg(test)] mod tests { ... }` — that module lives *inside* the production
file, so a unit test calling `foo()` from `src/thing.rs` matched no pattern and
was reported `production`. On a Rust codebase this inverted the feature's main
use case: a symbol called only by its own unit tests looked load-bearing, and
`filter: production` was actively misleading rather than merely incomplete.

For Rust callers, codanna now takes a second pass whenever the path heuristic
says `production`: it parses the caller's current source with tree-sitter,
collects the line spans covered by `#[cfg(test)]`-annotated items, and
re-classifies any caller falling inside one as `test`. So the six unit tests
calling a helper from an inline `mod tests` now report `6 test, 0 production`
instead of the reverse.

Details worth knowing:

- **No reindex needed.** This is computed at query time from the source on
  disk, so it fixes existing indexes immediately — nothing is persisted and the
  emission-semantics version is untouched.
- **Rust only.** Callers in every other language take the path heuristic
  unchanged. Nothing about non-Rust results changes.
- **Staleness-guarded.** The file on disk must still hash to what was indexed
  (the same guard `read_symbol` uses). If the file changed, is unreadable, or
  fails to parse, classification silently falls back to the path heuristic —
  the pre-existing answer. It never errors, and every failure path degrades
  toward reporting `production`, so a stale file can't turn "unsafe to delete"
  into "safe".
- **`#[cfg(feature = "test")]` is correctly not a test span** — the attribute
  argument is a string literal there, not the `test` identifier.
- **`#[cfg(any(test, feature = "x"))]` is correctly not a test span**, even
  though it contains the `test` identifier. `any(...)` is a disjunction: this
  attribute compiles in a normal (non-test) build whenever the sibling
  feature is enabled, so treating it as test-only would misclassify a
  production-reachable caller as `test` — the unsafe direction for a "safe to
  delete" answer. `#[cfg(not(test))]` is disqualified the same way.
  `#[cfg(all(test, not(windows)))]` **is** still treated as a test span:
  `all(...)` is a conjunction, so `test` inside it still means "test builds
  only" — only a disqualifying `any(...)`/`not(...)` around the `test`
  identifier itself flips the answer.
- **No configuration.** There is no toggle; `test_path_patterns` remains the
  only knob, and it still governs the path heuristic for every language.

The cost is one file read plus one parse attempt per *distinct* Rust caller
file per call — never more than once per file, even when that attempt fails
(stale hash, unreadable file, parse error): failure isn't retried per caller,
it degrades every caller in that file to the path heuristic in one pass. A
cheap substring check for `cfg` skips the parse entirely for files that
cannot contain a `#[cfg(...)]` attribute. That check deliberately
over-matches rather than looking for the exact `#[cfg(` byte sequence, so
valid-but-unusual formatting (`#[cfg (test)]`, or a line break before the
token tree) still gets parsed: a spurious parse costs little, whereas
skipping one would silently misclassify a caller. The read and parse never
run under the facade's async read
lock or directly on a tokio worker thread: classification is prepared (path
heuristics, file de-duplication) while the lock is briefly held, then the
lock is released and the actual file I/O + parsing runs inside
`tokio::task::spawn_blocking` (or inline for the synchronous CLI path, which
has no async runtime to starve).

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
the same release as the upstream version it is built on. `N` is a running count
of fork additions over the whole life of the fork, not per upstream base: it only
ever counts up, and moving to a newer upstream base does not reset it. So a
higher `N` always means more fork work, and an unchanged `N` always means none
was added — but `N` still says nothing about which upstream release you are on.
Read the base version for that.

Everything not listed here behaves as it does in upstream codanna.
