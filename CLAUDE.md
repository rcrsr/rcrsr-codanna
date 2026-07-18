# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`rcrsr-codanna` is a fork of [`bartolli/codanna`](https://github.com/bartolli/codanna) — a Rust code-intelligence engine that indexes source into a searchable symbol graph and serves it to LLM agents over MCP (semantic search, call graphs, relationship tracing). The binary is `codanna`.

### Fork discipline (read before touching version or CI)

- Version format is `<upstream>+rcrsr.N` (e.g. `0.9.23+rcrsr.1`) — see the header comment in `Cargo.toml`. `+rcrsr.N` is **semver build metadata**: it compares *equal* to the bare upstream version and is not orderable. Bump the upstream base only when rebasing onto a new upstream release (reset `N` to `.1`); increment `N` once per private addition landed on top.
- `publish = false` — the `codanna` crate name is owned by upstream. Distribution is via GitHub Releases + `cargo binstall --git`, not crates.io. Do not `cargo publish`.
- **RCRSR-README.md** documents every fork-private change for users. When you add or change fork behavior, update it.
- Fork-specific features currently live in: `serve --proxy` mode (`src/mcp/proxy.rs`, `src/serve_discovery.rs`, `src/serve_tls.rs`), the `reindex` MCP tool (`src/mcp/tools/admin.rs`), and catch-up reindex on watch-queue overflow (`src/watcher/`).

## Common commands

Build requires `--all-features` for the full feature set (proxy/HTTPS live behind features):

```bash
cargo build --release --all-features
```

Pre-push / pre-PR checks (these mirror the GitHub Actions jobs exactly — keep the scripts and workflows in sync):

```bash
./contributing/scripts/quick-check.sh   # fmt --check + clippy -D warnings (~2-3 min)
./contributing/scripts/auto-fix.sh      # auto-fix fmt + clippy
./contributing/scripts/full-test.sh     # fmt, clippy, no-default-features check, tests, doc, MCP smoke test
```

CI enforces (run these directly if you want the raw commands):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --no-default-features        # must still compile without default features
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
```

### Running tests

```bash
cargo test                                  # all unit + integration tests
cargo test --test force_reindex             # one integration test file (tests/*.rs)
cargo test test_name                        # a single test by name substring
cargo test --test integration_tests -- --nocapture   # see println/logging output
```

Integration tests live in `tests/*.rs` (each `*_tests.rs` file is a separate crate); shared helpers are in `tests/common/` and fixtures in `tests/fixtures/`. New tests should cover error paths, not just happy paths.

### Trying the binary against this repo

```bash
codanna init                                          # writes .codanna/settings.toml
codanna index src                                     # build the index
codanna mcp semantic_search_with_context query:"..." limit:5
codanna retrieve describe symbol_id:896               # explore a symbol
codanna serve --watch                                 # stdio MCP server that re-indexes on file change
```

## Architecture

The pipeline is: **source files → tree-sitter parse → symbol graph + vector embeddings → Tantivy index on disk → retrieval → MCP/CLI surface.**

- **`src/parsing/`** — one subdirectory per language (rust, python, typescript, go, java, kotlin, php, c, cpp, csharp, swift, lua, clojure, gdscript). Each language implements the `LanguageBehavior`/parser traits and is wired up through `factory.rs` + `registry.rs`. **Architectural boundary that matters:** universal concepts (qualified names, visibility, import resolution, scope levels) belong in the base traits (`language_behavior.rs`, `resolution.rs`); language-specific concepts (separator syntax, resolution order, unique features) belong in the per-language impls. See `contributing/development/guidelines.md` and `contributing/development/language-support.md` before adding a language. Registering a language means updating three hand-maintained touch points, or it silently never loads (no compile error) — see `conduct/policies/policy-domain-cdna.md` §CDNA.2.
- **`src/indexing/`** — the walker + pipeline that drives parsing and persistence (`facade.rs` is the `IndexFacade` entry point most commands go through).
- **`src/storage/`** + **`src/vector/`** + **`src/semantic/`** — Tantivy full-text index, the vector store, and embedding/semantic-search layer (embeddings via `fastembed`, pinned — see the `Cargo.toml` comment). The on-disk index lives under `.codanna/`.
- **`src/mcp/`** — the MCP server (built on `rmcp`). `server.rs`/`service.rs` register tools; `tools/` holds tool implementations (`search.rs`, `symbols.rs`, `admin.rs`). `http_server.rs`/`https_server.rs`/`proxy.rs` are the transport variants. A new MCP tool must join a `#[tool_router]` block and be composed into all three `server.rs` constructors (+ `KNOWN_TOOLS`/CLI match for `codanna mcp <tool>`), or it silently won't appear in `list_tools` — see `conduct/policies/policy-domain-cdna.md` §CDNA.5.
- **`src/cli/commands/`** — one file per subcommand (`index`, `retrieve`, `serve`, `mcp`, `documents`, `profile`, `plugin`, …). `main.rs` dispatches; args are defined in `src/cli/args.rs`.
- **`src/watcher/`** — file-watch + hot-reload for `serve --watch`, including the fork's overflow catch-up logic.
- **`src/retrieve.rs`** — the retrieval API (`describe`, `callers`, `calls`, `symbol`) shared by CLI and MCP.
- **`src/profiles/`** + **`src/plugins/`** — packageable hooks/commands/agents and the plugin system.

### Serve modes (`src/cli/commands/serve.rs`)

`codanna serve` resolves one of four modes via `resolve_server_mode(https, http, proxy, config_mode)` — precedence: `--https` > `--http` > `--proxy`/`config.mode == "proxy"` > default stdio. Proxy mode holds **no index** (its `IndexFacade` is `None`); it is a stdio↔HTTP delegate that discovers or auto-spawns a single backing server per workspace on a random loopback port. HTTPS uses a cert-pinned reqwest client (`serve_tls::pinned_client`).

**rustls crypto-provider gotcha:** `reqwest` 0.13 (aliased as `rmcp_reqwest`) and the pinned `rustls` (ring-only) must not both install a default `CryptoProvider`. The `https-server` feature deliberately uses `rmcp_reqwest/rustls-no-provider`, and `main.rs` installs `ring` as the sole provider once before command dispatch. Read the long comments in `Cargo.toml`'s `[features]` before changing any TLS/reqwest feature — getting it wrong is a runtime panic ("No rustls crypto provider is configured"), not a compile error.

## Code guidelines (project-specific, enforced by clippy + CI)

Full detail in `contributing/development/guidelines.md`. Highlights that shape reviews here:

- **Zero-cost signatures:** borrow (`&str`, `&[T]`, `impl Trait`) over owned/`Box<dyn>` on hot paths; return `Vec<T>` when callers always collect.
- **Newtypes over primitives** for IDs, validated in constructors. ID newtypes wrap `u32` with a `new() -> Option<Self>` that rejects 0 (not `NonZeroU32`). See `conduct/policies/policy-artifact-rust.md` §RS.4.1.
- **Errors:** `thiserror` with actionable messages; `anyhow` only at the binary level. Avoid `panic!`/`unwrap()` in non-test code; `expect()` only for provably-impossible states. All error types live centrally in `src/error.rs` as per-subsystem enums (`IndexError`, `ParseError`, `StorageError`, `McpError`) — see `conduct/policies/policy-artifact-rust.md` §RS.3.
- Performance targets are real: indexing 10k+ files/s, semantic search <10ms, ~100 bytes/symbol. Justify allocations on hot paths with measurement.
- `clippy.toml` raises some thresholds (too-many-arguments = 12, cognitive-complexity = 30) and allows `unwrap`/`expect`/`dbg` in tests only.

## Notes

- Embedding model (~150MB) downloads on first semantic-search use; `.fastembed_cache` here is symlinked to `~/.codanna/models`.
- `CLAUDE.md.example` is not project config — it's a user-facing template documenting the codanna search workflow for end users' own repos. Don't confuse it with this file.
