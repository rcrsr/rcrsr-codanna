# rcrsr-codanna — Fork Changes

This is a fork of [`bartolli/codanna`](https://github.com/bartolli/codanna).
This file lists what the fork adds or changes for you as a user, on top of its
upstream base. Everything not listed here behaves as it does upstream; for the
how, see the commit history.

- **Upstream base:** v0.16.0 — the `codanna` release the fork is built on
- **Fork build:** the upstream version with a `+rcrsr.N` suffix (see [Identifying the fork](#identifying-the-fork))

## Contents

- [Installing the fork](#installing-the-fork)
  - [Quick install (recommended)](#quick-install-recommended)
  - [Updating](#updating)
  - [Alternatives](#alternatives)
  - [PATH and shadowing](#path-and-shadowing)
  - [Cutting a release (maintainers)](#cutting-a-release-maintainers)
- [Upstream base](#upstream-base)
- [Improvements](#improvements)
  - [Proxy mode: one backing server per workspace](#proxy-mode-one-backing-server-per-workspace)
    - [Idle shutdown](#idle-shutdown)
    - [Upstream revival](#upstream-revival)
    - [Discovery record identity](#discovery-record-identity)
    - [Configuration](#configuration)
    - [Ports](#ports)
    - [Server registry and `codanna ls`](#server-registry-and-codanna-ls)
    - [Hot-reload notifications through the proxy](#hot-reload-notifications-through-the-proxy)
  - [Reindexing on demand (`reindex` MCP tool)](#reindexing-on-demand-reindex-mcp-tool)
    - [Arguments](#arguments)
    - [Concurrency contract](#concurrency-contract)
  - [Catch-up reindex on watch-queue overflow and after downtime](#catch-up-reindex-on-watch-queue-overflow-and-after-downtime)
    - [Startup catch-up (opt-in)](#startup-catch-up-opt-in)
    - [Configuration](#configuration-1)
  - [`ignore_patterns` now excludes files during indexing](#ignore_patterns-now-excludes-files-during-indexing)
  - [Indexing no longer depends on the working directory](#indexing-no-longer-depends-on-the-working-directory)
  - [Document collection controls (`search_documents`)](#document-collection-controls-search_documents)
  - [MCP tool enhancements for agent workflows](#mcp-tool-enhancements-for-agent-workflows)
- [Identifying the fork](#identifying-the-fork)

## Installing the fork

The fork is distributed through its own [GitHub Releases](https://github.com/rcrsr/rcrsr-codanna/releases),
not crates.io or Homebrew. CI builds Linux, macOS (x64 + arm64), and Windows
binaries for every `v<version>` tag.

### Quick install (recommended)

The installer downloads the right archive for your platform, verifies its
`sha256` against the release manifest (aborting on mismatch), and puts the
`codanna` binary on your `PATH`.

macOS / Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/rcrsr/rcrsr-codanna/main/scripts/install.sh | sh
```

Windows (PowerShell) — stop any running `codanna serve` first, since the
installer cannot overwrite a locked `codanna.exe`:

```powershell
irm https://raw.githubusercontent.com/rcrsr/rcrsr-codanna/main/scripts/install.ps1 | iex
```

Optional environment variables:

- `CODANNA_INSTALL_DIR` — install location (default `~/.local/bin`, or
  `%USERPROFILE%\.local\bin` on Windows).
- `CODANNA_VERSION` — install a specific release by tag (e.g.
  `v0.12.0+rcrsr.1`) instead of the latest.

The script is fetched from `main`, so the *installer logic* can change between
runs; only the binary it downloads is checksum-verified per release. To pin the
script itself, replace `main` in the URL with a release tag.

Release archives are named `codanna-<version>-<platform>.tar.xz` (`.zip` on
Windows) with the `+` in the version replaced by `-` (`0.12.0-rcrsr.1`) — a
literal `+` is not portable across download tools and GitHub rewrites asset
names containing it. The tag and `codanna --version` keep the `+` intact.

### Updating

Re-run the same install command — there is no `codanna update` subcommand.
The one-liner is idempotent and replaces your existing install with the latest
(or `CODANNA_VERSION`-pinned) release, the same model rustup, uv, and deno use.

### Alternatives

```bash
# prebuilt binary via cargo-binstall (must be pointed at the fork with --git;
# plain `cargo binstall codanna` resolves the upstream crate from crates.io)
cargo binstall --git https://github.com/rcrsr/rcrsr-codanna codanna

# from source
cargo install --git https://github.com/rcrsr/rcrsr-codanna --all-features codanna
```

Or download an archive from the [releases page](https://github.com/rcrsr/rcrsr-codanna/releases)
and put the binary on your `PATH` yourself.

### PATH and shadowing

The binary is named `codanna`, same as upstream, so whichever install comes
first on your `PATH` wins — the fork's `~/.local/bin` vs a Homebrew or
crates.io install in `/usr/local/bin` or `~/.cargo/bin`. After installing, run
`which codanna` (`where.exe codanna` on Windows) to see which resolves, and
`codanna --version` to confirm the `+rcrsr.N` suffix.

### Cutting a release (maintainers)

Before pushing a `v<version>` tag, rename `## [Unreleased]` in `CHANGELOG.md`
to `## [<version>] - <date>` using the *full* `Cargo.toml` version including
the literal `+rcrsr.N` (e.g. `## [0.12.0+rcrsr.1] - 2026-07-28`). The release
body is extracted by exact heading match, so a bare upstream heading is not
found and the tag push fails in CI before anything is built.

`contributing/scripts/test-release-workflow.sh` validates the whole pipeline
locally (version derivation, build, packaging, checksums, manifest, and an
offline run of `install.sh`) and asserts against `.github/workflows/release.yml`
so drift between the two is caught. Running the release workflow via
`workflow_dispatch` is a CI dry run: it builds and uploads artifacts but never
publishes a release, even against a tag.

## Upstream base

The fork tracks upstream **v0.16.0**. Moving the base never touches the
`+rcrsr.N` counter (see [Identifying the fork](#identifying-the-fork)).

Two fork-private capabilities are kept alongside upstream's newer equivalents
rather than retired, because upstream's version does not cover the case the
fork's exists for:

- **Writer transient-error retry.** Upstream's `create_writer_with_retry`
  classifies transient errors by downcasting to `io::Error`, which never
  matches tantivy's `LockError::LockBusy` — the exact case the fork's retry
  survives. The fork's retry stays the live path; upstream's segment-merge
  wait before releasing the writer was adopted on top.
- **JSON envelope `ambiguous` status.** Upstream added `begin`/`summary`
  dump-streaming markers; the fork's `ambiguous` constructor is kept beside
  them.

Upstream changes between the previous and current base that are user-visible,
with the fork's caveats:

- **Unknown `key:value` arguments on MCP tool calls are rejected** (v0.10.0)
  instead of silently ignored, on every surface (positional CLI args, `--args`,
  and serve-mode `tools/call`, where it surfaces as `isError: true`). Check
  argument names in any automation after upgrading.
- **A stale index no longer fails `codanna serve` at startup** (v0.10.1). Over
  stdio the server completes the MCP handshake and advertises **zero tools**,
  with instructions beginning `INDEX STALE - ALL TOOLS DISABLED` naming
  `codanna index` as the fix; it still exits with code 7 after the session, and
  a terminal still prints `index emission semantics changed`. **Fork note:**
  proxy mode is exempt because a proxy holds no index. If a proxy has to spawn
  a *fresh* backing server against a stale index you get a readiness timeout
  (`backing 'codanna serve --http' did not become healthy within …ms`) rather
  than the stale-index explanation; run `codanna index` to heal it.
- **`--fields` rejects unknown field names** (v0.11.1) with a JSON error
  envelope, `code: INVALID_QUERY`, exit code 2, and a hint listing the fields
  the tool actually returns (they differ per tool). Dotted paths are now
  accepted. **Fork note:** applies identically to the fork-only `find_symbols`
  and `reindex` tools; an ambiguous symbol name still exits 3 with
  `code: AMBIGUOUS` regardless of `--fields`.
- **v0.12.0 forces a one-time re-index of every existing index.** Symbol
  relationships are emitted differently (emission-semantics version 1 → 3),
  and the gate is absolute: an index from an older binary is refused (exit 7,
  or the zero-tool handshake over stdio). Run `codanna index` once per
  workspace and restart any MCP server. Cross-file resolution now **fails
  closed** — unresolvable calls are left unresolved instead of guessed — so
  expect relationship counts to *drop* on re-index (upstream measured −7.6% to
  −48%, mostly lost wrong answers). Re-baseline any tooling that asserts on
  caller or impact counts. `index.meta` and `get_index_info --json` also gain a
  descriptive `builder_commit` field.
- **v0.13.0 migrates every transport to the MCP 2026-07-28 spec** (rmcp
  3.1.0), serving the new stateless generation alongside the legacy one through
  its deprecation window. `server/discover` is answered; stateless HTTP requests
  require the `MCP-Protocol-Version` header (and `Mcp-Name` on name-bearing
  methods); `subscriptions/listen` lets stateless clients opt into
  resource-change notifications; tool/prompt lists carry a cache contract.
  `codanna mcp-test` connects Discover-only and diagnoses a server that dies on
  the probe. No rebuild. **Fork note:** upstream removed its custom
  `notifications/codanna/*` wire in favor of standard MCP notifications, and the
  fork followed — see [Hot-reload notifications](#hot-reload-notifications-through-the-proxy).
- **v0.13.1 converges incremental indexing with a fresh index** across renames,
  edits, and deletions (files referencing a moved target are re-analyzed in the
  same run; already-lost relationships restore on the next edit or with
  `codanna index --force`). Watched file creation and deletion emit
  `notifications/resources/list_changed`; modification emits a URI-filtered
  `notifications/resources/updated`. `serve --watch` no longer livelocks on
  Linux. Known limitation carried forward: Windows path handling under-reports
  some relationships.

## Improvements

The sections below are the fork's additions over upstream — new capabilities
and behavior changes you get on top of the base.

## Proxy mode: one backing server per workspace

`codanna serve --proxy` lets several MCP clients share one backing server per
workspace instead of each loading its own index. Point every client at the
proxy: the first starts (or discovers) a backing server, the rest attach to it.
Use it when more than one tool or editor talks to codanna for the same project.

```bash
codanna serve --proxy
```

The backing server is a detached background process that outlives its clients,
so the next client reattaches to a warm index. It exits on its own after 4
hours idle by default (see [Idle shutdown](#idle-shutdown)). Both `--http` and
`--https` backing servers are supported; with `--https` the connection is
verified against codanna's own certificate.

### Idle shutdown

A backing server (`--http` or `--https`) exits cleanly after
`idle_shutdown_minutes` (default 240) with no MCP request activity, removing
its `.codanna/serve.json` record and registry entry exactly as Ctrl+C does. Set
the key to `0` to disable idle shutdown. Only real inbound MCP requests count as
activity — SSE keep-alive pings do not reset the clock, so a merely *connected*
client does not keep the server alive forever.

Idle shutdown is transparent to clients: the next tool call through a proxy
finds no live server and auto-spawns a fresh one, paying only startup latency
(see [Upstream revival](#upstream-revival)).

### Upstream revival

A connected `codanna serve --proxy` does not need restarting when its backing
server goes away (idle shutdown, crash, manual kill). The proxy holds one
connection to its upstream and never polls it; the *next* delegated tool call
notices the dead connection, re-runs the same discover-or-spawn logic used at
startup (reusing a live backing server if one exists, else spawning one), and
retries the call exactly once before returning anything to the client.

Limits worth knowing before you rely on it:

- **Exactly one retry.** If the revived connection also fails, the error is
  returned as-is — no loop, no backoff. Two back-to-back failures mean the
  backing server is not coming up.
- **Single-flight per proxy, including on failure.** Concurrent requests
  hitting the dead connection share one dial; a failed dial is cached for the
  round so waiters get the same error instead of each re-spawning (and each
  paying a full spawn-and-health-check timeout).
- **Never on a timeout.** A backing server that is merely slow (a large
  reindex, say) is not treated as dead and is not replaced; only a genuinely
  closed transport triggers revival.
- **`auto_spawn = false` fails closed, not silently.** With no live backing
  server and spawning disallowed, the call fails with an actionable error
  naming the workspace and pointing at `codanna serve --http --watch` /
  `auto_spawn = true`.

### Discovery record identity

The proxy does not trust a `.codanna/serve.json` record naming an `--http`
backing server just because its PID is alive and looks like `codanna serve`.
Each `--http` server writes a fresh random per-launch token into the record and
echoes it from `/health`; discovery re-verifies it on every read, including on
every revive. A record whose token is missing or does not match is treated as
"no live server" and discovery spawns a fresh one on a new port. This hardens
`serve --proxy` against a same-user process racing a plausible record into
place. `--https` records are not probed this way — their identity already rests
on the pinned certificate at the real connection, and a probe rejection would
otherwise fall through to spawning plaintext `--http`.

One consequence: a backing server started by a codanna binary older than this
change (no token) is not discovered by a current proxy, which spawns its own
instead. That self-heals with no migration, but leaves the stale server
resident until it is stopped or idles out.

### Configuration

The workspace must be initialized (`codanna init`); the proxy refuses to
auto-spawn for a tree with no `.codanna/settings.toml`. All keys below are
optional and shown at their defaults:

```toml
[server]
auto_spawn = true           # let the proxy start a backing server when none is found;
                            # false = you start `codanna serve --http --watch` yourself
spawn_timeout_ms = 8000     # how long to wait for a spawned server to become ready
health_poll_ms = 100        # readiness poll interval while waiting
idle_shutdown_minutes = 240 # exit the backing server after N idle minutes (0 = never)
```

### Ports

An auto-spawned backing server binds a random free port on `127.0.0.1`; the
server records it and the proxy reads it back, so nothing on your side depends
on the number — clients only ever talk to the proxy over stdio. A backing
server you start yourself uses `--bind` or `[server] bind`, defaulting to
`127.0.0.1:8080` (HTTP) / `127.0.0.1:8443` (HTTPS). All backing servers listen
on loopback only.

### Server registry and `codanna ls`

Every `codanna serve --http`/`--https` process — auto-spawned or manual —
publishes itself to a per-user registry, separate from the per-workspace
`serve.json` discovery record: `serve.json` is for discovery within one
workspace, the registry is for lifecycle management across all of them. It
lives at `codanna/servers/` under your state directory (`$XDG_STATE_HOME` or
`~/.local/state` on Linux; `~/Library/Application Support` on macOS;
`%APPDATA%` on Windows), one file per running server named by pid, so nothing
contends. Proxies write a lightweight `role: proxy` entry on connect and remove
it on graceful exit. Each entry records the `codanna` version that wrote it.

`codanna ls` is the top-level command for listing every codanna server process
visible to you, merging three sources into one table (PID / KIND / SOURCE /
PORT / SCHEME / STATUS / WORKSPACE / VERSION): registered backing servers,
registered proxies attributed to the backing server sharing their workspace
root, and *rogue* `codanna serve` processes found by a process-table scan with
no live registry entry (best-effort enriched from the process cwd,
`serve.json`, or a `--bind` argument). `ls` never reaps, signals, or rewrites
anything. `codanna serve --list` is **deprecated for one release cycle**: it
prints a notice to stderr and delegates to `ls`.

```bash
codanna ls                                  # list every codanna server process you own
codanna serve --stop <pid|workspace-path>   # SIGTERM a registered server
codanna serve --stop <pid> --force          # SIGKILL instead
codanna serve --stop <pid> --include-rogue  # allow a rogue pid (still must look like `codanna serve`)
codanna serve --reap                        # prune registry entries whose pid is dead
codanna serve --kill-all                    # SIGTERM every registered backing server
codanna serve --kill-all --include-proxies  # ...and every registered proxy too
codanna serve --kill-all --force            # SIGKILL every target
```

These are lifecycle operations and cannot be combined with
`--http`/`--https`/`--proxy`/`--bind`.

- `--stop` sends SIGTERM by default, so the server runs the same shutdown path
  as Ctrl+C and idle shutdown, removing its `serve.json` and registry entry.
  A `--force`d (SIGKILL) process cannot clean up, so its registry entry can be
  left behind; `--reap` prunes those. `ls` skips dead-pid entries but never
  deletes them, keeping "what does the registry say" and "clean up" separate.
- `--kill-all` sweeps every live registry entry. Every target is attempted even
  if an earlier one fails; exit code is `0` only if all stopped.

**VERSION column.** Registered rows show the version the server recorded at
startup (`-` for entries written by a pre-`version` build). Rogue rows have no
self-reported version, so the column instead compares the rogue binary on disk
to the `codanna ls` binary — by canonical path first, then by size and bytes if
the paths differ, so the same build installed at two locations (a mise dir vs
`~/.local/bin`) still reads as `same`. The rogue process is never exec'd or
`--version`-probed.

- `same` — identical binary, at the same path or a byte-identical copy.
- `other` — different path and different size/contents (or unreadable for
  comparison).
- `deleted` — the rogue's binary is gone from disk (the Linux in-place-upgrade
  signature: `/proc/<pid>/exe` ending in `(deleted)`) or unreadable.
- `-` — the `codanna ls` binary's own path could not be resolved.

**STATUS column.** Registered servers show their self-reported `spawning` or
`healthy`; rogue rows show `running`. A proxy whose backing server is gone,
dead, or itself rogue shows `orphaned` instead of the `healthy` it recorded at
connect time (a proxy's entry is written once and never updated, so it would
otherwise stay `healthy` forever). An orphaned proxy is still live and will
revive a backing server on its next delegated call; stop it with
`--kill-all --include-proxies` if you don't want that.

### Hot-reload notifications through the proxy

The watch lane rides standard MCP notifications: a modified watched file emits
a URI-filtered `notifications/resources/updated`; a created or deleted file
emits `notifications/resources/list_changed`. The proxy forwards these — along
with tool/prompt list-changed, progress, and logging notifications — verbatim
to each stdio client, so a client behind the proxy is as hot-reload-aware as one
connected directly. If you only run a single client, plain `codanna serve` is
unchanged.

## Reindexing on demand (`reindex` MCP tool)

Upstream reindexing is CLI-only. The fork exposes it as a `reindex` MCP tool,
discoverable via `list_tools` in every serve mode (stdio, HTTP, HTTPS, proxy),
so an editor or agent can trigger it over the protocol without restarting the
server. It is also reachable as `codanna mcp reindex`.

```jsonc
{ "name": "reindex", "arguments": {} }                                 // incremental; unchanged files skipped
{ "name": "reindex", "arguments": { "paths": ["src/foo.rs", "src/bar/"] } }
{ "name": "reindex", "arguments": { "force": true } }                  // full clear-and-rebuild
{ "name": "reindex", "arguments": { "documents": true } }              // also refresh document collections
```

### Arguments

- `paths` — files or directories to reindex (default: all configured
  `indexed_paths`). Must resolve inside the workspace root; at most 1024.
- `force` (default `false`) — for a full reindex, clears the index before
  rebuilding. For scoped `paths`, re-parses those files even when their content
  hash is unchanged, without a global clear.
- `documents` (default `false`) — additionally reindex every configured
  document collection, discovering markdown files added since the last run
  (which upstream reindexing and the watcher never do). Totals are reported
  separately, and a failing collection is an error naming it, not a silent skip.

The call returns files reindexed, symbols, and elapsed milliseconds (plus
per-collection totals with `documents: true`); `output_format: "json"` gives a
structured envelope.

### Concurrency contract

Read-only MCP tools, including `search_documents`, are safe to call in parallel
from multiple clients in every serve mode. Reindexing does not block reads: the
walk-and-parse work runs off the index write lock, which is held only briefly
before and after it. While the walk is in flight, readers may transiently see a
repopulating index.

`search_documents` takes a brief write guard per call to auto-sync collections
against disk, then searches under a read guard, so concurrent calls make
progress against each other. Embedding inference itself serializes on a single
model instance, but callers queue on a blocking-pool thread rather than
stalling the async runtime.

**Concurrent code reindexes are serialized, not queued.** Only one `reindex`
run — scoped, `force`, or the watcher's own catch-up — may be in flight at a
time. A second is rejected immediately with a retryable `REINDEX_IN_PROGRESS`
error ("Another full reindex is already in progress; retry shortly…") rather
than queued or allowed to race the first
([#44](https://github.com/rcrsr/rcrsr-codanna/issues/44)).

**Known limitation:** `reindex documents:true` holds the exclusive write guard
for each collection's full duration (one collection at a time). The work runs
on a blocking thread so unrelated async work continues, but document searches
against *that* collection wait until it finishes.

## Catch-up reindex on watch-queue overflow and after downtime

The unified file watcher runs whenever `--watch` is passed *or*
`[file_watch].enabled` is true — and it defaults to `true`, so a bare
`codanna serve` runs it too. The OS watch backend has a bounded event queue; a
bulk operation (`git rebase`, a branch switch, a large `git pull`) can overflow
it and drop events. Upstream silently misses those changes until you reindex
by hand.

The fork detects the overflow signal and, once file activity settles, fires one
catch-up reindex automatically:

- It waits for a quiet window and coalesces a burst of overflow signals into
  one catch-up rather than firing mid-operation.
- It runs off the watcher's event loop, so events keep draining while it works.
- A failed catch-up (transient lock/IO error) is retried on the next quiet
  window, bounded to five attempts per episode; successive catch-ups are
  throttled by a short cooldown.
- If it loses the race to an in-flight `reindex` MCP call, that rejection is
  not counted as a failure — the index is already being brought current — and
  it simply re-fires after the cooldown. If that rejection persists for roughly
  a minute, a `WARN` log notes that the other reindex appears wedged, re-emitted
  on a widening interval (10 → 20 → 40 min, then hourly).
- Separately, a watchdog on the reindex walk itself logs an `ERROR` if the walk
  runs longer than ten minutes (re-logged on the same widening cadence). It is
  observability only — it does not cancel the walk or release the serialization
  gate, because the walk runs on a blocking thread that cannot be interrupted
  and releasing the gate mid-write would re-open the race the gate prevents.
  Recovering a genuinely wedged reindex requires a restart.

### Startup catch-up (opt-in)

The same machinery can be armed once when the watcher starts, so files changed
while nothing was watching (a restart, a machine sleep, a deploy) re-converge
without waiting for an overflow. It is **off by default** (`startup_catch_up`)
and independent of `refresh_on_overflow` — the two keys are two triggers for
the same machinery, not one gated by both.

Know what you're opting into: this is a full clear-and-rebuild, so expect
degraded or empty query results until it completes on a large index. The clear
and rebuild are not atomic — if the process is killed between them (OOM,
`kill -9`, host crash) the on-disk index is left empty with no signal on next
start; run `codanna index <path>` to rebuild. That window exists regardless,
but `startup_catch_up` opens it on every start and every proxy auto-respawn.
Combined with a short `idle_shutdown_minutes` on a large workspace, every
respawn pays a full rebuild — size the timeout with that in mind. With no
`indexed_paths` registered, each episode logs five `ERROR` lines and gives up;
that is expected, not a wedge.

### Configuration

```toml
[file_watch]
refresh_on_overflow = true  # catch-up on watch-queue overflow (false = upstream behavior)
startup_catch_up = false    # arm one catch-up at watcher startup
```

`churn_threshold` is parsed but **reserved** — it has no effect, and a non-zero
value logs a one-time startup warning.

## `ignore_patterns` now excludes files during indexing

`[indexing] ignore_patterns` in `settings.toml` used to deserialize but was
never consulted ([#22](https://github.com/rcrsr/rcrsr-codanna/issues/22)). The
fork wires it into every walk — `codanna index`, `--dry-run`, incremental and
watch-triggered reindex, and the subtree registration when `serve --watch` sees
a new directory, so an excluded directory never gets watched.

It uses the **same gitignore dialect as `.codannaignore`** (`!` negation,
trailing `/`, `**`). Patterns are applied *after* `.gitignore`/`.codannaignore`,
so a `!` here can only re-include something excluded by an earlier
`ignore_patterns` entry, never something a gitignore file excluded — do that in
the gitignore file instead.

```toml
[indexing]
ignore_patterns = ["fixtures/**", "!fixtures/keep.rs"]
```

The four patterns codanna used to hard-code (`target/**`, `node_modules/**`,
`.git/**`, `*.generated.*`) are no longer in the default — the default
`.codannaignore` from `codanna init` already excludes them. Existing
`settings.toml` files are untouched; any patterns already there now take effect.

**This setting is fork-only.** Upstream resolved #22 by deleting the key. A
`settings.toml` written for the fork loads on upstream without complaint but
silently ignores `ignore_patterns`; move those patterns to `.codannaignore` if
you need identical behavior on both.

## Indexing no longer depends on the working directory

Two read paths (batch READ and single-file watch reindex) opened
workspace-relative paths against the process cwd instead of `workspace_root`.
Running from the workspace root was never affected, but embedding `IndexFacade`
from another cwd produced a silently empty index that still reported success,
and `serve --watch` started from elsewhere failed every reindex with
`No such file or directory`. Both now resolve against `workspace_root` and are
covered by regression tests.

## Document collection controls (`search_documents`)

Additions to `search_documents` and `codanna documents search`:

- **Per-collection default visibility.** `default = false` on a
  `[documents.collections.<name>]` opts it out of searches that name no
  `collection`; naming it explicitly still searches it. Set it at creation with
  `codanna documents add-collection --no-default`; `documents list` annotates
  such collections `(non-default)` (the `--json` form does not carry this yet).
- **Negated glob patterns.** `patterns` accepts gitignore-style `!` entries
  (`["**/*.md", "!docs/internal/**"]`), resolved with the same `ignore` crate
  machinery as `.codannaignore`, so a later `!` actually excludes.
- **Multi-select.** `--collection` and `--exclude-collection` are repeatable
  (allowlist union / denylist on top of the resolved defaults). Over MCP,
  `collection` accepts a string or array and `exclude_collections` an array;
  `codanna mcp search_documents` accepts the same `collection:` /
  `exclude_collections:` / `threshold:` keys.
- **Input validation.** An unknown collection name in `collection` or
  `exclude_collections` is `code: INVALID_QUERY` naming the bad value and
  listing configured names — not a `NOT_FOUND` indistinguishable from a real
  miss. `limit: 0` is likewise rejected (deliberately *not* the `0 = unlimited`
  convention `get_file_outline` uses).
- **`threshold`** (cosine similarity, `[-1, 1]`, same meaning as
  `semantic_search_docs`) drops scored hits below it before `limit`; an empty
  result after the cut is `status: not_found`. No effect on the no-embedding
  fallback path, which scores every hit 0.0.
- **`meta.collections` / `meta.excluded_collections`** on every JSON `success`
  and `not_found` envelope echo the *resolved* filter actually searched.
- **JSON `content_preview` is plain text** — no ANSI escapes or `>>`/`<<`
  markers; the text format keeps them.
- **Clearer tool descriptions.** `semantic_search_docs` (doc comments extracted
  from code symbols) and `search_documents` (indexed markdown collections) now
  each say which corpus they search and point at the other, so an agent picks
  the right one from `list_tools` alone.

Known follow-ups: `codanna documents search --json` still emits highlighted
previews, and KWIC highlighting matches substrings rather than whole words.

## MCP tool enhancements for agent workflows

Every change is additive — omit the new parameters and behavior is identical to
upstream.

- **Structured JSON output.** Every tool accepts `output_format: "text" |
  "json"` (default `"text"`). JSON is an envelope with `status`, `code`,
  `exit_code`, `message`, `data`, and `meta.schema_version`; `status`
  distinguishes `success`, `not_found`, `ambiguous`, and `error`. It is the same
  envelope the CLI `--json` path emits.
- **Batch symbol lookup.** `find_symbols` takes `names: [...]` (up to 1024) and
  returns a per-name map of `found` / `not_found` / `ambiguous` (with
  candidates) in one round-trip.
- **Canonical `name` parameter.** `find_symbol`, `get_calls`, `find_callers`,
  and `analyze_impact` all accept `name`; the old `function_name` /
  `symbol_name` still work as aliases. `find_symbol` also takes a typed
  `symbol_id`.
- **Symbol-scoped reads.** `get_file_outline(path)` lists every symbol in a
  file with kind, signature, visibility, and line range; `read_symbol(name |
  symbol_id)` returns one symbol's exact source span, refusing with a staleness
  report if the file's hash no longer matches the index.
- **Slimmer `analyze_impact`.** `count_only` (symbol and file counts only),
  `max_results` (truncates and flags `truncated` in `meta`), and `group_by:
  kind | file`.

### Test/production classification on `find_callers`

`find_callers` tags each caller `production` or `test`, accepts `filter: all |
production | test` and `count_only`, so "is this safe to delete" becomes "zero
*production* callers". Classification starts from configurable path patterns:

```toml
[caller_classification]
test_path_patterns = ["tests/", "/test/", "*_test.*", "test_*.py", "*.spec.*", "__tests__/"]
```

**Rust `#[cfg(test)]` modules are detected.** Path patterns cannot see an
inline `#[cfg(test)] mod tests` inside a production file, which on a Rust
codebase inverted the feature: a symbol called only by its own unit tests looked
load-bearing. For Rust callers where the path heuristic says `production`,
codanna parses the caller's current source with tree-sitter and re-classifies callers
inside a `#[cfg(test)]` span as `test`. Details:

- Computed at query time from the source on disk — no reindex, nothing
  persisted. Rust only.
- Staleness-guarded: if the file's hash no longer matches the index, or it is
  unreadable or fails to parse, classification falls back to the path heuristic
  for every caller in that file. Every failure path degrades toward
  `production`, so a stale file cannot turn "unsafe to delete" into "safe".
- `#[cfg(feature = "test")]`, `#[cfg(any(test, …))]`, and `#[cfg(not(test))]`
  are correctly *not* test spans (they can compile in a non-test build);
  `#[cfg(all(test, …))]` is.
- Cost is one read and one parse per distinct Rust caller file per call, run
  on a blocking thread outside the index lock; files with no `cfg` substring
  skip the parse.

## Identifying the fork

Fork builds carry a `+rcrsr.N` suffix on the upstream version:

```bash
codanna --version        # e.g. codanna <upstream-version>+rcrsr.N
```

MCP clients see the same string in the `initialize` handshake. The suffix is
semver build metadata — it does not change how the version compares, so a fork
build counts as the same release as its upstream base. `N` is a running count
of fork additions over the whole life of the fork: it only ever counts up, and
moving to a newer upstream base does not reset it. A higher `N` always means
more fork work, but says nothing about which upstream release you are on — read
the base version for that.
