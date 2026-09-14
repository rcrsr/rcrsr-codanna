//! CLI entry point for the codebase intelligence system.
//!
//! Provides commands for indexing, querying, and serving code intelligence data.
//! Uses the cli module for argument parsing and command definitions.

use clap::Parser;
use codanna::cli::{Cli, Commands, RetrieveQuery};
use codanna::indexing::facade::{IndexFacade, format_semantic_status};
use codanna::project_resolver::{
    providers::{
        csharp::CSharpProvider, go::GoProvider, java::JavaProvider, javascript::JavaScriptProvider,
        kotlin::KotlinProvider, php::PhpProvider, python::PythonProvider, swift::SwiftProvider,
        typescript::TypeScriptProvider,
    },
    registry::SimpleProviderRegistry,
};
use codanna::storage::generation;
use codanna::storage::persistence::{BuildFacade, BuildMode};
use codanna::storage::{EMISSION_SEMANTICS_VERSION, IndexLayout};
use codanna::{IndexError, IndexPersistence, IndexResult, Settings};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Create and populate the provider registry with all language providers.
///
/// This registry manages project-specific resolution providers that handle
/// configuration files (like tsconfig.json) for enhanced import resolution.
fn create_provider_registry() -> SimpleProviderRegistry {
    let mut registry = SimpleProviderRegistry::new();

    // Add TypeScript provider for tsconfig.json resolution
    registry.add(Arc::new(TypeScriptProvider::new()));

    // Add JavaScript provider for jsconfig.json resolution
    registry.add(Arc::new(JavaScriptProvider::new()));

    // Add Java provider for pom.xml/build.gradle resolution
    registry.add(Arc::new(JavaProvider::new()));

    // Add Swift provider for Package.swift resolution
    registry.add(Arc::new(SwiftProvider::new()));

    // Add Go provider for go.mod resolution
    registry.add(Arc::new(GoProvider::new()));

    // Add Python provider for pyproject.toml resolution
    registry.add(Arc::new(PythonProvider::new()));

    // Add Kotlin provider for build.gradle.kts resolution
    registry.add(Arc::new(KotlinProvider::new()));

    // Add PHP provider for composer.json resolution
    registry.add(Arc::new(PhpProvider::new()));

    // Add C# provider for .csproj resolution
    registry.add(Arc::new(CSharpProvider::new()));

    registry
}

/// Initialize project resolution providers before indexing.
///
/// This validates configuration files and builds resolution caches for
/// languages that have config_files specified in settings.toml.
fn initialize_providers(
    registry: &SimpleProviderRegistry,
    settings: &Settings,
) -> Result<(), codanna::IndexError> {
    use codanna::IndexError;

    let mut validation_errors = Vec::new();

    for provider in registry.active_providers(settings) {
        let lang_id = provider.language_id();
        let config_paths = provider.config_paths(settings);

        if config_paths.is_empty() {
            continue; // Skip if no config files specified
        }

        tracing::debug!(target: "cli", "initializing {lang_id} project resolver...");

        // Validate config paths
        let mut invalid_paths = Vec::new();
        for path in &config_paths {
            if !path.exists() {
                invalid_paths.push(path.clone());
            }
        }

        if !invalid_paths.is_empty() {
            // Collect all invalid paths for error reporting
            for path in &invalid_paths {
                eprintln!(
                    "  - {} config file not found: {}",
                    lang_id,
                    codanna::parsing::paths::render_absolute_path(path).display()
                );
            }
            validation_errors.push((lang_id.to_string(), invalid_paths));
            continue;
        }

        // Build cache
        tracing::debug!(
            target: "cli",
            "building resolution cache from {} config file(s)...",
            config_paths.len()
        );
        if let Err(e) = provider.rebuild_cache(settings) {
            // Warning only - continue without failing
            tracing::warn!(target: "cli", "failed to build {lang_id} resolution cache: {e}");
            tracing::warn!(target: "cli", "continuing without alias resolution for {lang_id}");
        } else {
            tracing::debug!(target: "cli", "{lang_id} resolution cache built successfully");
        }
    }

    if !validation_errors.is_empty() {
        // Build detailed error message
        let mut error_details = String::from("Invalid project configuration files:\n");
        for (lang, paths) in &validation_errors {
            error_details.push_str(&format!("\n{lang} configuration:\n"));
            for path in paths {
                error_details.push_str(&format!(
                    "  - {} not found\n",
                    codanna::parsing::paths::render_absolute_path(path).display()
                ));
            }
        }
        error_details.push_str("\nSuggestion: Check paths in .codanna/settings.toml");
        error_details.push_str("\nExample for TypeScript:\n");
        error_details.push_str("  [languages.typescript]\n");
        error_details
            .push_str("  config_files = [\"tsconfig.json\", \"packages/web/tsconfig.json\"]");

        Err(IndexError::ConfigError {
            reason: error_details,
        })
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct SeedReport {
    newly_seeded: Vec<PathBuf>,
    missing_paths: Vec<PathBuf>,
}

fn seed_indexer_with_config_paths(
    indexer: &mut IndexFacade,
    config_paths: &[PathBuf],
) -> SeedReport {
    let mut report = SeedReport::default();

    if config_paths.is_empty() {
        return report;
    }

    // Collect existing tracked paths once to avoid repeated borrow issues
    let mut existing: std::collections::HashSet<PathBuf> =
        indexer.get_indexed_paths().iter().cloned().collect();

    for path in config_paths {
        if !path.exists() {
            report.missing_paths.push(path.clone());
            continue;
        }

        if !path.is_dir() {
            tracing::debug!(
                target: "cli",
                "skipping configured path (not a directory): {}",
                path.display()
            );
            continue;
        }

        if existing.contains(path) {
            continue;
        }

        let len_before = existing.len();
        indexer.add_indexed_path(path);
        // Refresh our view of tracked paths to honor internal dedup logic
        existing = indexer.get_indexed_paths().iter().cloned().collect();
        if existing.len() > len_before {
            report.newly_seeded.push(path.clone());
        }
        tracing::debug!(
            target: "cli",
            "seeded configured directory into tracked paths: {}",
            path.display()
        );
    }

    report
}

/// Resolve whether a `Commands::Serve` invocation selects proxy mode.
///
/// Proxy mode never loads an `IndexFacade` in-process (§4.5): it discovers or
/// spawns a backing `codanna serve --http` and relays stdio traffic to it. This
/// mirrors the CLI-flag-then-config precedence in
/// `cli::commands::serve::run` so the pre-dispatch resource predicates
/// (`needs_indexer`, `needs_trait_resolver`, `needs_semantic_search`) agree
/// with the mode `serve::run` will actually select.
fn is_proxy_serve(command: &Commands, config: &Settings) -> bool {
    match command {
        Commands::Serve {
            proxy: true,
            http: false,
            https: false,
            ..
        } => true,
        Commands::Serve {
            proxy: false,
            http: false,
            https: false,
            ..
        } => config.server.mode == "proxy",
        _ => false,
    }
}

/// Resolve whether a `Commands::Serve` invocation is a registry lifecycle
/// operation (`--list`/`--stop`/`--reap`/`--kill-all`) rather than a request
/// to start a server.
///
/// These operations only ever read/write the per-user server registry
/// (`src/serve_registry.rs`); they never load an `IndexFacade`, exactly like
/// proxy mode (`is_proxy_serve`). Every pre-dispatch resource predicate that
/// excludes proxy mode must also exclude these, or `serve --list` would
/// needlessly load a full index before printing a table.
fn is_serve_management_op(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Serve { list: true, .. }
            | Commands::Serve { stop: Some(_), .. }
            | Commands::Serve { reap: true, .. }
            | Commands::Serve { kill_all: true, .. }
    )
}

/// Print `e`'s message (prefixed by `context`) and recovery suggestions to
/// stderr, then exit with `e`'s mapped exit code. Centralizes the
/// print-suggestions-then-exit shape shared by every hard failure to open,
/// build, or load a generation.
fn exit_with_index_error(e: &IndexError, context: &str) -> ! {
    eprintln!("Error: {context}: {e}");
    let suggestions = e.recovery_suggestions();
    if !suggestions.is_empty() {
        eprintln!("\nSuggestions:");
        for suggestion in suggestions {
            eprintln!("  - {suggestion}");
        }
    }
    std::process::exit(codanna::io::ExitCode::from_error(e) as i32);
}

/// A resource held across the pre-dispatch resource-loading section and
/// command dispatch: either a plain loaded [`IndexFacade`] (every command
/// except `codanna index` itself) or an in-progress [`BuildFacade`] (the
/// `index` build-target commands). Both are driven identically by the
/// shared seed/semantic-enable/sync sequence via [`Self::facade_mut`],
/// keeping that sequence single-sourced regardless of which variant is
/// live (§BASIC.2).
enum FacadeHandle {
    Loaded(IndexFacade),
    Building(BuildFacade),
}

impl FacadeHandle {
    fn facade_mut(&mut self) -> &mut IndexFacade {
        match self {
            Self::Loaded(facade) => facade,
            Self::Building(build) => build,
        }
    }

    /// Unwraps a [`Self::Loaded`] handle. Every command except `codanna
    /// index` itself only ever populates `handle` via [`load_current`] /
    /// [`load_current_or_recover`], never [`Self::Building`]; reaching the
    /// other arm here means a command was wired to the wrong branch of the
    /// pre-dispatch load/build section.
    fn into_loaded(self) -> IndexFacade {
        match self {
            Self::Loaded(facade) => facade,
            Self::Building(_) => {
                unreachable!("only `codanna index` opens a build; every other command loads")
            }
        }
    }
}

/// Load the current generation, choosing full or lite loading based on
/// whether the caller's command needs semantic search.
fn load_current(
    persistence: &IndexPersistence,
    settings: &Arc<Settings>,
    needs_semantic_search: bool,
) -> IndexResult<IndexFacade> {
    if needs_semantic_search {
        persistence.load_facade(settings.clone())
    } else {
        persistence.load_facade_lite(settings.clone())
    }
}

/// Load the current generation for a command that only ever reads it
/// (`serve`, `retrieve`, `dump`, `mcp` without `--watch`) -- never builds.
///
/// On a load failure, [`generation::list_generations`] distinguishes a
/// genuinely empty root from damaged remnants left behind on disk:
/// - Genuinely empty **and** `is_serve`: bootstrap a fresh empty generation
///   via [`IndexFacade::new`], matching prior behavior for a brand-new
///   workspace (§BASIC.12.1: this is the one case where bootstrapping is
///   still safe, since there is demonstrably nothing to lose).
/// - Genuinely empty and any other (read-only) command: print the error and
///   exit non-zero -- a read command cannot itself populate the index.
/// - Damaged remnants (`list_generations` is non-empty): never bootstrap
///   over them, for ANY command including `serve` -- print the error and
///   exit non-zero rather than silently discarding forensic evidence
///   (§BASIC.12.1).
fn load_current_or_recover(
    persistence: &IndexPersistence,
    settings: &Arc<Settings>,
    index_path: &Path,
    needs_semantic_search: bool,
    info: bool,
    is_serve: bool,
) -> IndexFacade {
    match load_current(persistence, settings, needs_semantic_search) {
        Ok(loaded) => {
            if info {
                eprintln!(
                    "Loaded existing index (total: {} symbols)",
                    loaded.symbol_count()
                );
            }
            loaded
        }
        Err(e) => {
            let layout = IndexLayout::new(index_path.to_path_buf());
            // A listing failure is treated the same as "remnants exist":
            // never bootstrap over an on-disk state this process could not
            // even enumerate.
            let root_has_remnants = generation::list_generations(&layout)
                .map(|rows| !rows.is_empty())
                .unwrap_or(true);

            if is_serve && !root_has_remnants {
                IndexFacade::new(settings.clone())
                    .unwrap_or_else(|e2| exit_with_index_error(&e2, "Failed to create index"))
            } else {
                exit_with_index_error(&e, "Could not load index")
            }
        }
    }
}

/// Entry point with tokio async runtime.
///
/// Handles config initialization, index loading/creation, and command dispatch.
/// Auto-initializes config for index command. Persists index after modifications.
#[tokio::main]
async fn main() {
    // reqwest's `rustls-no-provider` backend (enabled transitively by the
    // `https-server` feature, see Cargo.toml) requires a default rustls
    // crypto provider installed before the FIRST `reqwest::Client` is built
    // anywhere in this process, or that build panics with "No rustls crypto
    // provider is configured" -- including the plain `--http` proxy path's
    // client (src/mcp/proxy.rs), which never touches `serve_tls` at all.
    // Installing it once, here, before any command dispatch, covers every
    // `reqwest::Client` construction site regardless of which one runs first.
    #[cfg(feature = "https-server")]
    {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    let cli = Cli::parse();

    // For index command, auto-initialize if needed (but not when using --config)
    if matches!(cli.command, Commands::Index { .. }) && cli.config.is_none() {
        if Settings::check_init().is_err() {
            // Auto-initialize for index command
            eprintln!("Initializing project configuration...");
            match Settings::init_config_file(false) {
                Ok(path) => {
                    eprintln!("Created configuration file at: {}", path.display());
                }
                Err(e) => {
                    eprintln!("Warning: Could not create config file: {e}");
                    eprintln!("Using default configuration.");
                }
            }
        }
    } else if !matches!(cli.command, Commands::Init { .. }) && cli.config.is_none() {
        // For other commands without --config flag, just warn
        if let Err(warning) = Settings::check_init() {
            eprintln!("Warning: {warning}");
            eprintln!("Using default configuration for now.");
        }
    }

    // Load configuration
    let mut config = if let Some(config_path) = &cli.config {
        Settings::load_from(config_path).unwrap_or_else(|e| {
            eprintln!(
                "Configuration error loading from {}: {}",
                config_path.display(),
                e
            );
            std::process::exit(1);
        })
    } else {
        Settings::load().unwrap_or_else(|e| {
            eprintln!("Configuration error: {e}");
            Settings::default()
        })
    };

    // Initialize logging with config (supports RUST_LOG env var override)
    // All logging goes to stderr to avoid polluting stdout (JSON output, piping)
    codanna::logging::init_with_config(&config.logging);

    // Determine resource requirements based on command type
    // Commands are categorized by what infrastructure they need:
    // - Thin: No index, no providers (Parse, McpTest, Benchmark)
    // - Config-only: Settings but no index (Init, Config, AddDir, RemoveDir, ListDirs, Plugin, Profile, Documents)
    // - Full: Index + providers (Retrieve, Mcp, Serve, Index)
    let needs_providers = !matches!(
        &cli.command,
        Commands::Parse { .. } | Commands::McpTest { .. } | Commands::Benchmark { .. }
    ) && !is_serve_management_op(&cli.command);

    let needs_indexer = !matches!(
        &cli.command,
        Commands::Init { .. }
            | Commands::Config
            | Commands::Parse { .. }
            | Commands::McpTest { .. }
            | Commands::Benchmark { .. }
            | Commands::AddDir { .. }
            | Commands::RemoveDir { .. }
            | Commands::ListDirs
            | Commands::Plugin { .. }
            | Commands::Documents { .. }
            | Commands::Profile { .. }
            | Commands::Ls
            // `--status`, `--gc`, and `--rollback` are read-only/generation-
            // level inspection or maintenance operations (see `run_status`,
            // `run_gc`, `run_rollback`); none of them must trigger
            // `IndexFacade::new`'s bootstrap-on-open, which writes a fresh
            // `current` generation to disk when nothing resolves.
            | Commands::Index { status: true, .. }
            | Commands::Index { gc: true, .. }
            | Commands::Index {
                rollback: Some(_),
                ..
            }
    ) && !is_proxy_serve(&cli.command, &config)
        && !is_serve_management_op(&cli.command);

    // Initialize project resolution providers (only if needed)
    // This ensures caches are built before indexing starts
    if needs_providers {
        let provider_registry = create_provider_registry();
        if let Err(e) = initialize_providers(&provider_registry, &config) {
            // Only fatal for commands that need providers (like index)
            if matches!(cli.command, Commands::Index { .. }) {
                eprintln!("\n{e}");
                let suggestions = e.recovery_suggestions();
                if !suggestions.is_empty() {
                    eprintln!("\nSuggestions:");
                    for suggestion in suggestions {
                        eprintln!("  - {suggestion}");
                    }
                }
                std::process::exit(1);
            } else {
                // For other commands, just warn
                eprintln!("Warning: Provider initialization failed: {e}");
            }
        }
    }

    // Apply config overrides from CLI args
    if let Commands::Index {
        threads: Some(t), ..
    } = &cli.command
    {
        config.indexing.parallelism = *t;
    }

    // Set up persistence based on config
    // Use global path resolution that handles --config properly
    let index_path = codanna::init::resolve_index_path(&config, cli.config.as_deref());

    // Update the config with the resolved index_path so SimpleIndexer uses the correct path
    config.index_path = index_path.clone();

    let persistence = IndexPersistence::new(index_path.clone());

    // Determine if we need full trait resolver initialization
    // Only needed for trait-related commands: implementations, trait analysis, etc.
    let needs_trait_resolver = matches!(
        cli.command,
        Commands::Retrieve {
            query: RetrieveQuery::Implementations { .. },
            ..
        } | Commands::Index { .. }
            | Commands::Serve { .. }
    ) && !is_proxy_serve(&cli.command, &config)
        && !is_serve_management_op(&cli.command);

    // Determine if we need semantic search (ML model loading)
    // Retrieve commands use Tantivy text search only - no ML model needed
    let needs_semantic_search = match &cli.command {
        Commands::Mcp { tool, .. } => {
            // Only these MCP tools need semantic search
            ["semantic_search_docs", "semantic_search_with_context"].contains(&tool.as_str())
        }
        Commands::Serve { .. } if is_proxy_serve(&cli.command, &config) => false,
        Commands::Serve { .. } if is_serve_management_op(&cli.command) => false,
        Commands::Index { .. } | Commands::Serve { .. } => true,
        _ => false,
    };

    // Emission-semantics gate: an index stamped with a different (or no)
    // emission version must not be read or incrementally extended -- a
    // partial rewrite leaves a silent hybrid mixing row semantics.
    // `codanna index` heals by full rebuild; everything else (including
    // dry-run, whose pre-dispatch path sync can write) refuses with the
    // heal command. `--force` clears unconditionally and needs no gate.
    // Every published generation carries `index.meta`: serve's startup
    // bootstrap stamps the current emission version into an otherwise
    // empty one, so a manufactured skeleton passes the gate, while
    // pre-stamping indexes carry `index.meta` without the version field
    // and keep gating.
    let mut emission_heal = false;
    if needs_indexer
        && persistence.exists()
        && !matches!(cli.command, Commands::Index { force: true, .. })
    {
        let stored = persistence
            .current_metadata()
            .and_then(|m| m.emission_version);
        if stored != Some(EMISSION_SEMANTICS_VERSION) {
            let stored_txt = stored.map_or_else(|| "none".to_string(), |v| format!("v{v}"));
            let current = EMISSION_SEMANTICS_VERSION;
            if matches!(cli.command, Commands::Index { dry_run: false, .. }) {
                eprintln!(
                    "Index emission semantics changed (index: {stored_txt}, binary: v{current}). Rebuilding from scratch."
                );
                emission_heal = true;
            } else {
                eprintln!(
                    "Error: index emission semantics changed (index: {stored_txt}, binary: v{current})."
                );
                eprintln!(
                    "Reading it would mix stale and current rows. Run 'codanna index' to rebuild."
                );
                // Client-spawned stdio servers lose stderr: a pre-handshake
                // exit surfaces as an opaque connection failure. Serve a
                // degraded handshake (zero tools, heal command in the
                // instructions field), then exit with the gate code when the
                // session ends. HTTP/HTTPS serve is terminal-launched, where
                // stderr is already visible.
                if matches!(
                    cli.command,
                    Commands::Serve {
                        http: false,
                        https: false,
                        ..
                    }
                ) && config.server.mode != "http"
                {
                    codanna::cli::commands::serve::run_stale_stdio(stored, current).await;
                }
                std::process::exit(codanna::io::ExitCode::IndexCorrupted as i32);
            }
        }
    }

    // Load existing index or create new one (only if command needs it)
    let settings = Arc::new(config.clone());
    // Captured before facade creation: creating a facade manufactures the
    // index directory, so persistence.exists() afterwards cannot tell a
    // real index from one this process just created.
    // Guarded by `needs_indexer`: `exists()` migrates a legacy flat layout,
    // and read-only commands (`index --status`) must never do that.
    let index_preexisted = needs_indexer && persistence.exists();

    // The force and emission-heal lanes clear the persisted index during
    // facade creation, before the rebuild sources are validated; a
    // mistyped CLI path — or a configured root set with no surviving
    // entry on the bare lane — would destroy the index and rebuild
    // nothing. Existence is checked here, ahead of any destructive clear.
    if let Commands::Index { paths, force, .. } = &cli.command {
        if *force || emission_heal {
            if !paths.is_empty() {
                let mut missing = false;
                for path in paths.iter().filter(|p| !p.exists()) {
                    missing = true;
                    eprintln!("Error: Path does not exist: {}", path.display());
                }
                if missing {
                    std::process::exit(1);
                }
            } else if index_preexisted && !config.indexing.indexed_paths.iter().any(|p| p.exists())
            {
                for path in &config.indexing.indexed_paths {
                    eprintln!(
                        "Error: Configured path does not exist: {}",
                        codanna::parsing::paths::render_absolute_path(path).display()
                    );
                }
                eprintln!("Error: --force would clear the index with nothing to rebuild");
                std::process::exit(1);
            }
        }
    }
    // `codanna index` (outside `--status`/`--gc`/`--rollback`, already
    // excluded from `needs_indexer`) is the one command that builds: it
    // opens a staged generation via `open_build` and publishes it once its
    // work (including any pre-dispatch sync below) is done. Every other
    // command that needs an indexer only ever reads the current generation.
    let is_index_build_command = needs_indexer && matches!(cli.command, Commands::Index { .. });
    let is_serve_command = matches!(cli.command, Commands::Serve { .. });
    // Force flag always means a from-scratch generation, regardless of path
    // source (CLI or settings.toml); an emission-semantics change heals the
    // same way.
    let is_force_index =
        matches!(cli.command, Commands::Index { force: true, .. }) || emission_heal;

    let mut handle: Option<FacadeHandle> = if !needs_indexer {
        None
    } else if is_index_build_command {
        let mode = if is_force_index {
            BuildMode::Fresh
        } else {
            BuildMode::CloneCurrent
        };
        match persistence.open_build(settings.clone(), mode) {
            Ok(build) => Some(FacadeHandle::Building(build)),
            Err(e) => exit_with_index_error(&e, "Failed to open index build"),
        }
    } else {
        let skip_trait_resolver = !needs_trait_resolver;
        if skip_trait_resolver {
            tracing::debug!(target: "cli", "using lazy initialization (skipping trait resolver)");
        }
        Some(FacadeHandle::Loaded(load_current_or_recover(
            &persistence,
            &settings,
            &index_path,
            needs_semantic_search,
            cli.info,
            is_serve_command,
        )))
    };

    // Enable semantic search if configured
    let seed_report = if let Some(ref mut h) = handle {
        Some(seed_indexer_with_config_paths(
            h.facade_mut(),
            &config.indexing.indexed_paths,
        ))
    } else {
        None
    };

    if let Some(ref mut h) = handle {
        let idx = h.facade_mut();
        // Only enable semantic search for commands that need it
        if needs_semantic_search
            && config.semantic_search.enabled
            && !idx.has_semantic_search()
            && !idx.is_semantic_incompatible()
        {
            if let Err(e) = idx.enable_semantic_search() {
                eprintln!("Warning: Failed to enable semantic search: {e}");
            } else {
                let status = format_semantic_status(&config.semantic_search);
                eprintln!("{status}");
            }
        }
    }

    // Sync indexed paths with config - auto-index new directories
    // This handles changes made while the index was not in use (e.g., add-dir command)
    // Skip sync if force flag is present (force means fresh start, not incremental)

    // Progress is enabled by default from settings, can be disabled with --no-progress
    let no_progress_flag = matches!(
        cli.command,
        Commands::Index {
            no_progress: true,
            ..
        }
    );
    let show_progress = config.indexing.show_progress && !no_progress_flag;
    // Extract CLI-provided paths for accurate --force messaging
    let cli_index_paths: Vec<PathBuf> = if let Commands::Index { ref paths, .. } = cli.command {
        paths.clone()
    } else {
        Vec::new()
    };

    if let Some(report) = &seed_report {
        if is_force_index {
            if !cli_index_paths.is_empty() {
                // CLI paths provided -- only those will be rebuilt
                let cli_roots: Vec<String> = cli_index_paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                println!("Rebuilding index for: {}", cli_roots.join(", "));

                // Warn about configured paths that won't be rebuilt
                let cli_canonical: Vec<PathBuf> = cli_index_paths
                    .iter()
                    .filter_map(|p| p.canonicalize().ok())
                    .collect();
                let not_rebuilt: Vec<String> = config
                    .indexing
                    .indexed_paths
                    .iter()
                    .filter(|p| {
                        let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                        !cli_canonical
                            .iter()
                            .any(|c| canon.starts_with(c) || c.starts_with(&canon))
                    })
                    .map(|p| {
                        codanna::parsing::paths::render_absolute_path(p)
                            .display()
                            .to_string()
                    })
                    .collect();
                if !not_rebuilt.is_empty() {
                    eprintln!(
                        "Warning: --force clears the entire index. These configured paths will not be rebuilt: {}",
                        not_rebuilt.join(", ")
                    );
                    eprintln!("Run 'codanna index --force' without paths to rebuild everything.");
                }
            } else if !report.newly_seeded.is_empty() {
                let roots: Vec<String> = report
                    .newly_seeded
                    .iter()
                    .map(|p| {
                        codanna::parsing::paths::render_absolute_path(p)
                            .display()
                            .to_string()
                    })
                    .collect();
                println!(
                    "Rebuilding index for configured roots: {}",
                    roots.join(", ")
                );
            } else if !config.indexing.indexed_paths.is_empty() {
                let roots: Vec<String> = config
                    .indexing
                    .indexed_paths
                    .iter()
                    .map(|p| {
                        codanna::parsing::paths::render_absolute_path(p)
                            .display()
                            .to_string()
                    })
                    .collect();
                println!(
                    "Rebuilding index for configured roots: {}",
                    roots.join(", ")
                );
            } else {
                println!("Rebuilding index with provided paths only (no configured roots).");
            }
        }

        if !report.missing_paths.is_empty() {
            if report.missing_paths.len() == 1 {
                eprintln!(
                    "Warning: Skipping configured path (not found): {}",
                    codanna::parsing::paths::render_absolute_path(&report.missing_paths[0])
                        .display()
                );
            } else {
                let listed: Vec<String> = report
                    .missing_paths
                    .iter()
                    .map(|p| {
                        codanna::parsing::paths::render_absolute_path(p)
                            .display()
                            .to_string()
                    })
                    .collect();
                eprintln!(
                    "Warning: Skipping {} configured paths (not found): {}",
                    report.missing_paths.len(),
                    listed.join(", ")
                );
            }
        }
    }
    // Track whether sync made changes (for later check); None means sync did not run
    let mut sync_made_changes: Option<bool> = None;

    // The index command owns indexing work. On a freshly created index
    // (e.g. after `rm -rf .codanna/index`), stored_paths defaults empty
    // and sync would read every configured root as a config change,
    // running the full pass pre-dispatch and leaving the command phase
    // to no-op ("Index up to date"). Removal sync after `remove-dir`
    // needs a pre-existing index by definition, so this gate never
    // skips it.
    let index_command_fresh_index = is_index_build_command && !index_preexisted;

    // Sync a single set of indexed-path directories against `config`, using
    // whichever `IndexFacade` `on` derefs to. Shared by both branches below
    // so the log lines and error handling stay single-sourced regardless of
    // whether the caller is driving its own build (the `index` command) or a
    // throwaway sync-only build (every other command, below).
    fn run_sync(
        on: &mut IndexFacade,
        stored_paths: Option<Vec<PathBuf>>,
        config: &Settings,
        show_progress: bool,
    ) -> codanna::indexing::SyncStats {
        match on.sync_with_config(stored_paths, &config.indexing.indexed_paths, show_progress) {
            Ok(stats) => {
                if stats.added_dirs > 0 {
                    tracing::info!(
                        target: "sync",
                        "indexed {} directories ({} files, {} symbols)",
                        stats.added_dirs, stats.files_indexed, stats.symbols_found
                    );
                }
                if stats.removed_dirs > 0 {
                    tracing::info!(
                        target: "sync",
                        "removed {} directories from index",
                        stats.removed_dirs
                    );
                }
                if stats.files_modified > 0 || stats.files_added > 0 {
                    tracing::info!(
                        target: "sync",
                        "synced {} modified, {} new files",
                        stats.files_modified, stats.files_added
                    );
                }
                stats
            }
            Err(e) => {
                eprintln!("\nFailed to sync indexed paths: {e}");
                let suggestions = e.recovery_suggestions();
                if !suggestions.is_empty() {
                    eprintln!("\nRecovery steps:");
                    for suggestion in suggestions {
                        eprintln!("  - {suggestion}");
                    }
                }
                use codanna::io::ExitCode;
                std::process::exit(ExitCode::from_error(&e) as i32);
            }
        }
    }

    if is_index_build_command {
        // The `index` command drives its own build: sync runs directly on
        // it via `&mut *build` (`FacadeHandle::facade_mut`), and any
        // changes it makes are published together with the command's own
        // indexing work at the dispatch arm below -- never saved here.
        if persistence.exists() && !is_force_index && !index_command_fresh_index {
            match persistence.current_metadata() {
                Some(metadata) => {
                    let idx = handle
                        .as_mut()
                        .expect("index build always populates handle")
                        .facade_mut();
                    let stats =
                        run_sync(idx, metadata.indexed_paths.clone(), &config, show_progress);
                    sync_made_changes = Some(stats.has_changes());
                }
                None => {
                    eprintln!("\nWarning: Could not load index metadata; skipping sync");
                    tracing::debug!(
                        target: "cli",
                        "index root: {}",
                        codanna::parsing::paths::render_absolute_path(&config.index_path)
                            .display()
                    );
                    eprintln!("\nRecovery steps:");
                    eprintln!("  - Run 'codanna index' to rebuild metadata");
                    eprintln!("  - Or use 'codanna index --force' for a full rebuild");
                    sync_made_changes = None;
                }
            }
        }
    } else if needs_indexer && persistence.exists() {
        // Every other command that needs an indexer (`serve`, `retrieve`,
        // `dump`, `mcp`) only reads the current generation; auto-indexing a
        // config change here means opening a throwaway `CloneCurrent` build,
        // syncing on it, and publishing it only if it actually changed
        // anything. A no-op build is simply dropped -- it becomes an orphan
        // generation reclaimed by a later `gc` pass, never manually cleaned
        // up at this call site.
        if let Some(metadata) = persistence.current_metadata() {
            let stored_set: std::collections::HashSet<PathBuf> = metadata
                .indexed_paths
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect();
            let config_set: std::collections::HashSet<PathBuf> =
                config.indexing.indexed_paths.iter().cloned().collect();

            if stored_set != config_set {
                match persistence.open_build(settings.clone(), BuildMode::CloneCurrent) {
                    Ok(mut build) => {
                        let stats =
                            run_sync(&mut build, metadata.indexed_paths, &config, show_progress);
                        if stats.has_changes() {
                            match persistence.publish(build) {
                                Ok(_) => {
                                    handle =
                                        Some(FacadeHandle::Loaded(
                                            load_current(
                                                &persistence,
                                                &settings,
                                                needs_semantic_search,
                                            )
                                            .unwrap_or_else(|e| {
                                                exit_with_index_error(
                                                    &e,
                                                    "Could not load freshly synced generation",
                                                )
                                            }),
                                        ));
                                }
                                Err(e) => {
                                    tracing::warn!(target: "sync", "failed to publish synced generation: {e}");
                                }
                            }
                        }
                        // else: no changes; `build` is dropped here.
                    }
                    Err(e) => {
                        tracing::warn!(target: "sync", "failed to open sync build: {e}");
                    }
                }
            }
        } else {
            eprintln!("\nWarning: Could not load index metadata; skipping sync");
            eprintln!("\nRecovery steps:");
            eprintln!("  - Run 'codanna index' to rebuild metadata");
            eprintln!("  - Or use 'codanna index --force' for a full rebuild");
        }
    }

    let serve_is_proxy = is_proxy_serve(&cli.command, &config);
    let serve_is_management = is_serve_management_op(&cli.command);

    match cli.command {
        Commands::Init { force } => {
            codanna::cli::commands::init::run_init(force);
        }

        Commands::Config => {
            codanna::cli::commands::init::run_config(&config);
        }

        Commands::Parse {
            file,
            output,
            max_depth,
            all_nodes,
        } => {
            codanna::cli::commands::parse::run(&file, output, max_depth, all_nodes);
        }

        Commands::McpTest {
            server_binary,
            tool,
            args,
            delay,
        } => {
            use codanna::mcp::client::CodeIntelligenceClient;

            let server_path = server_binary.unwrap_or_else(|| {
                std::env::current_exe().expect("Failed to get current executable path")
            });

            if let Err(e) = CodeIntelligenceClient::test_server(
                server_path,
                cli.config.clone(),
                tool,
                args,
                delay,
            )
            .await
            {
                eprintln!("MCP test failed: {e}");
                std::process::exit(1);
            }
        }

        Commands::Serve {
            watch,
            watch_interval,
            http,
            https,
            proxy,
            bind,
            list,
            stop,
            reap,
            kill_all,
            include_proxies,
            force,
            include_rogue,
        } => {
            use codanna::cli::commands::serve::{ServeArgs, run as run_serve};
            // Proxy mode and registry-management ops (--list/--stop/--reap)
            // never load an IndexFacade in-process (§4.5): the predicates
            // above (needs_indexer/needs_trait_resolver/needs_semantic_search)
            // already exclude both, so `handle` is `None` here and must not
            // be unwrapped in either case.
            let facade = if serve_is_proxy || serve_is_management {
                None
            } else {
                Some(
                    handle
                        .expect("non-proxy, non-management serve requires indexer")
                        .into_loaded(),
                )
            };
            run_serve(
                ServeArgs {
                    watch,
                    watch_interval,
                    http,
                    https,
                    proxy,
                    bind,
                    list,
                    stop,
                    reap,
                    kill_all,
                    include_proxies,
                    force,
                    include_rogue,
                },
                config,
                settings,
                facade,
                index_path,
                cli.config.clone(),
            )
            .await;
        }

        Commands::Index {
            paths,
            threads: _,
            force,
            no_progress,
            dry_run,
            list_all,
            json,
            max_files,
            status,
            gc,
            rollback,
        } => {
            use codanna::cli::commands::index::{
                IndexArgs, run as run_index, run_gc, run_rollback, run_status,
            };
            use codanna::indexing::DryRunOutput;

            if status {
                // Read-only: never builds or writes, so it does not touch
                // `handle` at all.
                run_status(&config, json);
            } else if gc {
                // Generation-level maintenance: no facade involved, same as
                // `--status`.
                run_gc(&config);
            } else if let Some(rollback_id) = rollback {
                // Generation-level maintenance: no facade involved, same as
                // `--status`.
                run_rollback(&config, rollback_id);
            } else {
                // Progress enabled by default from settings, --no-progress overrides
                let progress = config.indexing.show_progress && !no_progress;
                // `--json` wins over `--list-all` under `--dry-run`.
                let dry_run_output = if json {
                    DryRunOutput::Json
                } else if list_all {
                    DryRunOutput::ListAll
                } else {
                    DryRunOutput::Summary
                };

                let mut build = match handle {
                    Some(FacadeHandle::Building(build)) => build,
                    _ => unreachable!(
                        "this arm always opens a BuildFacade via is_index_build_command"
                    ),
                };

                let made_changes = run_index(
                    IndexArgs {
                        paths,
                        force,
                        progress,
                        dry_run,
                        max_files,
                        cli_config: cli.config.clone(),
                        dry_run_output,
                    },
                    &mut config,
                    &mut build,
                    sync_made_changes,
                );

                // `run` never persists anything itself (see its doc
                // comment); this call site owns the publish decision.
                if made_changes {
                    eprintln!(
                        "\nSaving index with {} total symbols, {} total relationships...",
                        build.symbol_count(),
                        build.relationship_count()
                    );
                    match persistence.publish(build) {
                        Ok(_) => {
                            println!(
                                "Index saved to: {}",
                                codanna::parsing::paths::render_absolute_path(&config.index_path)
                                    .display()
                            );
                        }
                        Err(e) => {
                            eprintln!("Error: Could not save index: {e}");
                            std::process::exit(1);
                        }
                    }
                } else if index_command_fresh_index {
                    // No changes were made (e.g. `--dry-run` against a
                    // never-before-indexed workspace, or real paths with
                    // nothing indexable), but this root has no prior
                    // generation to fall back to: publish this (possibly
                    // empty) build anyway so `current` resolves to a
                    // well-formed generation, exactly as
                    // `IndexFacade::new`'s bootstrap always did before
                    // generations existed; otherwise the next read command
                    // would find no index at all.
                    if let Err(e) = persistence.publish(build) {
                        eprintln!("Error: Could not save index: {e}");
                        std::process::exit(1);
                    }
                }
                // else: no changes against an already-existing root; the
                // build is simply dropped here, becoming an orphan
                // generation reclaimed by a later `gc` pass -- never
                // manually cleaned up at this call site.
            }
        }

        Commands::AddDir { path } => {
            codanna::cli::commands::directories::run_add_dir(path, cli.config.as_deref());
        }

        Commands::RemoveDir { path } => {
            codanna::cli::commands::directories::run_remove_dir(path, cli.config.as_deref());
        }

        Commands::ListDirs => {
            codanna::cli::commands::directories::run_list_dirs(&config);
        }

        Commands::Ls => {
            codanna::cli::commands::ls::run();
        }

        Commands::Retrieve { query } => {
            let facade = handle.expect("retrieve requires indexer").into_loaded();
            let exit_code = codanna::cli::commands::retrieve::run(query, &facade);
            std::process::exit(exit_code as i32);
        }

        Commands::Dump {
            symbols,
            edges,
            relation,
            kind,
        } => {
            let filter = codanna::dump::DumpFilter {
                rows: match (symbols, edges) {
                    (true, _) => codanna::dump::Rows::Symbols,
                    (_, true) => codanna::dump::Rows::Relationships,
                    _ => codanna::dump::Rows::All,
                },
                relation,
                kind,
            };
            let facade = handle.expect("dump requires indexer").into_loaded();
            let exit_code = codanna::cli::commands::dump::run(&facade, &config, &filter);
            std::process::exit(exit_code as i32);
        }

        Commands::Mcp {
            tool,
            positional,
            args,
            json,
            fields,
            watch,
        } => {
            let mut indexer = handle.expect("mcp requires indexer").into_loaded();

            // If --watch is enabled, check for file changes and reindex
            // through a throwaway `CloneCurrent` build: publish it (and
            // reload `indexer` onto the freshly published generation) only
            // if it actually indexed something; otherwise drop it -- a
            // no-op build becomes an orphan generation reclaimed by a later
            // `gc` pass, never manually cleaned up at this call site.
            if watch {
                let paths = config.get_indexed_paths();
                if !paths.is_empty() {
                    match persistence.open_build(settings.clone(), BuildMode::CloneCurrent) {
                        Ok(mut build) => {
                            let mut total_indexed = 0usize;
                            for path in &paths {
                                if path.is_dir() {
                                    // Run incremental indexing (force=false)
                                    match build.index_directory_with_options(
                                        path,
                                        false, // no progress bars for watch mode
                                        false, // not dry run
                                        false, // not force (incremental)
                                        None,  // no max_files limit
                                        codanna::indexing::DryRunOutput::default(),
                                    ) {
                                        Ok(stats) => total_indexed += stats.files_indexed,
                                        Err(e) => {
                                            tracing::warn!(target: "mcp", "watch reindex failed for {}: {e}", codanna::parsing::paths::render_absolute_path(path).display());
                                        }
                                    }
                                }
                            }
                            if total_indexed > 0 {
                                match persistence.publish(build) {
                                    Ok(_) => {
                                        indexer = load_current(
                                            &persistence,
                                            &settings,
                                            needs_semantic_search,
                                        )
                                        .unwrap_or_else(|e| {
                                            exit_with_index_error(
                                                &e,
                                                "Could not load freshly indexed generation",
                                            )
                                        });
                                    }
                                    Err(e) => {
                                        tracing::warn!(target: "mcp", "failed to publish watch reindex: {e}");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(target: "mcp", "failed to open watch reindex build: {e}");
                        }
                    }
                }
            }

            codanna::cli::commands::mcp::run(
                tool, positional, args, json, fields, indexer, &config,
            )
            .await;
        }

        Commands::Benchmark { language, file } => {
            codanna::cli::commands::benchmark::run(&language, file);
        }

        Commands::Plugin { action } => {
            codanna::cli::commands::plugin::run(action, &config);
        }

        Commands::Documents { action } => {
            codanna::cli::commands::documents::run(action, &config, cli.config.as_ref());
        }

        Commands::Profile { action } => {
            codanna::cli::commands::profile::run(action);
        }
    }
}

#[cfg(test)]
mod seed_indexer_tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn test_seed_indexer_with_config_paths_tracks_configured_roots() {
        let temp_dir = TempDir::new().unwrap();
        let parent = temp_dir.path().join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();

        let settings = Settings {
            index_path: temp_dir.path().join("index"),
            ..Settings::default()
        };
        let mut indexer =
            IndexFacade::new(Arc::new(settings)).expect("Failed to create IndexFacade");
        assert!(indexer.get_indexed_paths().is_empty());

        let canonical_parent = parent.canonicalize().unwrap();
        let report =
            seed_indexer_with_config_paths(&mut indexer, std::slice::from_ref(&canonical_parent));
        assert_eq!(report.newly_seeded.len(), 1);
        assert_eq!(report.newly_seeded[0], canonical_parent);
        assert!(report.missing_paths.is_empty());

        let tracked: Vec<_> = indexer.get_indexed_paths().iter().cloned().collect();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0], canonical_parent);

        // Adding a child after the parent should be a no-op
        let canonical_child = child.canonicalize().unwrap();
        let child_report =
            seed_indexer_with_config_paths(&mut indexer, std::slice::from_ref(&canonical_child));
        assert!(
            child_report.newly_seeded.is_empty(),
            "child seeding should not add new directories"
        );
        let tracked_after_child: Vec<_> = indexer.get_indexed_paths().iter().cloned().collect();
        assert_eq!(tracked_after_child.len(), 1, "child should not be tracked");
        assert_eq!(tracked_after_child[0], canonical_parent);
    }

    #[test]
    fn test_seed_indexer_with_config_paths_reports_missing() {
        let temp_dir = TempDir::new().unwrap();
        let missing = temp_dir.path().join("missing_dir");

        let settings = Arc::new(Settings {
            index_path: temp_dir.path().join("index"),
            ..Settings::default()
        });
        let mut indexer = IndexFacade::new(settings).expect("Failed to create IndexFacade");

        let report = seed_indexer_with_config_paths(&mut indexer, std::slice::from_ref(&missing));
        assert!(
            report.newly_seeded.is_empty(),
            "missing directory should not be seeded"
        );
        assert_eq!(report.missing_paths.len(), 1);
        assert_eq!(report.missing_paths[0], missing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Verifies CLI structure is valid at compile time.
    ///
    /// Uses clap's debug_assert to catch configuration errors.
    #[test]
    fn verify_cli() {
        // This test ensures the CLI structure is valid
        Cli::command().debug_assert();
    }
}

#[cfg(test)]
mod is_proxy_serve_tests {
    use super::*;

    // `is_proxy_serve` is pure (Commands + Settings in, bool out) so precedence
    // between the CLI `--proxy` flag and `config.server.mode` can be asserted
    // hermetically here, mirroring `resolve_server_mode`'s precedence in
    // `cli::commands::serve`.

    fn serve_command(http: bool, https: bool, proxy: bool) -> Commands {
        Commands::Serve {
            watch: false,
            watch_interval: 5,
            http,
            https,
            proxy,
            bind: "127.0.0.1:8080".to_string(),
            list: false,
            stop: None,
            reap: false,
            kill_all: false,
            include_proxies: false,
            force: false,
            include_rogue: false,
        }
    }

    #[test]
    fn cli_proxy_flag_selects_proxy() {
        let config = Settings::default();
        assert!(is_proxy_serve(&serve_command(false, false, true), &config));
    }

    #[test]
    fn config_server_mode_proxy_selects_proxy_for_bare_serve() {
        let mut config = Settings::default();
        config.server.mode = "proxy".to_string();
        assert!(is_proxy_serve(&serve_command(false, false, false), &config));
    }

    #[test]
    fn cli_http_flag_still_selects_http_over_config_proxy() {
        let mut config = Settings::default();
        config.server.mode = "proxy".to_string();
        assert!(!is_proxy_serve(&serve_command(true, false, false), &config));
    }

    #[test]
    fn non_serve_command_is_never_proxy() {
        let config = Settings::default();
        assert!(!is_proxy_serve(&Commands::Config, &config));
    }
}
