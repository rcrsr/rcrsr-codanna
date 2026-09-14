//! Index command - index source code files and directories.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::commands::directories::{SkipReason, add_paths_to_settings};
use crate::config::Settings;
use crate::indexing::DryRunOutput;
use crate::indexing::facade::IndexFacade;
use crate::storage::generation::{self, GenerationId, GenerationState, IndexLayout};
use crate::types::SymbolKind;

/// Arguments for the index command.
pub struct IndexArgs {
    pub paths: Vec<PathBuf>,
    pub force: bool,
    pub progress: bool,
    pub dry_run: bool,
    pub max_files: Option<usize>,
    pub cli_config: Option<PathBuf>,
    /// `--dry-run` output verbosity (`--list-all` / `--json`). Ignored unless
    /// `dry_run` is set.
    pub dry_run_output: DryRunOutput,
}

/// Run the index command.
///
/// This command handles both file and directory indexing with options for
/// force re-indexing, progress display, dry-run mode, and file limits.
/// Drives `indexer` (an [`IndexFacade`], or a [`crate::storage::BuildFacade`]
/// borrowed via `&mut *build` -- `BuildFacade` derefs to `IndexFacade`) and
/// returns whether it made any changes. This function never persists
/// anything itself: the caller decides what "changes were made" means for
/// its own persistence strategy (e.g. publishing a build vs. dropping an
/// orphaned one), and issues the actual save/publish call after `run`
/// returns.
pub fn run(
    args: IndexArgs,
    config: &mut Settings,
    indexer: &mut IndexFacade,
    sync_made_changes: Option<bool>,
) -> bool {
    let IndexArgs {
        paths,
        force,
        progress,
        dry_run,
        max_files,
        cli_config,
        dry_run_output,
    } = args;

    // `--json` only has a defined meaning paired with `--dry-run` (JSON
    // path list) or `--status` (JSON generation table, handled by
    // `run_status` before this function is ever called). Reaching this
    // function with `dry_run_output` set to `Json` but `dry_run` false
    // means the caller passed bare `--json` with neither modifier;
    // `--list-all` already rejects that combination at parse time via
    // `requires = "dry_run"`, so mirror that here now that `--json` no
    // longer carries the same clap constraint (it must also pair with
    // `--status`, which clap's single-field `requires` cannot express).
    if !dry_run && matches!(dry_run_output, DryRunOutput::Json) {
        eprintln!("Error: --json requires --dry-run or --status");
        std::process::exit(2);
    }

    // Preflight: construct one parser per enabled language so configuration
    // errors (e.g. malformed parser_options) fail the command here. The
    // pipeline constructs parsers per worker thread and surfaces failures
    // as per-file parse errors, which silently skips the language.
    {
        let registry = crate::parsing::registry::get_registry();
        let registry = registry.lock().unwrap_or_else(|e| {
            eprintln!("Error: language registry lock poisoned: {e}");
            std::process::exit(1);
        });
        for definition in registry.iter_enabled(config) {
            if let Err(e) = definition.create_parser(config) {
                eprintln!("Error: cannot initialize {} parser: {e}", definition.name());
                std::process::exit(2);
            }
        }
    }

    // Determine paths to index
    let paths_to_index = if !paths.is_empty() {
        // CLI paths provided - add them to settings.toml first
        let config_path = if let Some(custom_path) = cli_config {
            custom_path
        } else {
            Settings::find_workspace_config().unwrap_or_else(|| {
                eprintln!("Error: No configuration file found. Run 'codanna init' first.");
                std::process::exit(1);
            })
        };

        match add_paths_to_settings(&paths, &config_path, false) {
            Ok((updated_settings, added_paths, skipped_paths)) => {
                if !added_paths.is_empty() {
                    eprintln!("Added {} path(s) to settings.toml", added_paths.len());
                }
                // These are informational settings-sync notices, not indexing
                // results, so they always go to stderr. This keeps `--json`
                // stdout free of contamination even when a CLI-supplied path
                // is already covered by (or present in) settings.toml.
                for skipped in &skipped_paths {
                    match &skipped.reason {
                        SkipReason::CoveredBy(parent) => eprintln!(
                            "{}: Included in indexed directory {}",
                            skipped.path.display(),
                            crate::parsing::paths::render_absolute_path(parent).display()
                        ),
                        // Registration state, not index state: the path is
                        // already in indexed_paths. Saying "already indexed"
                        // here claims the content exists, which is false
                        // right after the index dir is removed by hand.
                        SkipReason::AlreadyPresent if !force => {
                            eprintln!("{}: Already indexed", skipped.path.display())
                        }
                        SkipReason::AlreadyPresent => {}
                        SkipReason::FileNotPersisted => eprintln!(
                            "{}: Ad-hoc indexed (not in settings.toml)",
                            skipped.path.display()
                        ),
                    }
                }
                // Update config with the new settings
                *config = updated_settings;
                paths
            }
            Err(e) => {
                eprintln!("Error updating settings: {e}");
                std::process::exit(1);
            }
        }
    } else {
        // No CLI paths - use settings.toml indexed_paths
        let config_paths = config.get_indexed_paths();

        if config_paths.is_empty() {
            eprintln!("Error: No paths to index");
            eprintln!();
            eprintln!("Options:");
            eprintln!("  1. Provide paths: codanna index <path> [<path>...]");
            eprintln!("  2. Configure paths: codanna add-dir <path>");
            std::process::exit(1);
        }

        if !force {
            match sync_made_changes {
                Some(true) => {
                    // Sync added new directories, already indexed. Report
                    // changes made and let the caller decide how to persist
                    // them; this function no longer saves on its own.
                    return true;
                }
                Some(false) | None => {
                    // No directory changes - check file-level changes via incremental
                    tracing::debug!(target: "indexing", "checking {} paths for file-level changes", config_paths.len());
                }
            }
        }

        // Run incremental (force=false) or full reindex (force=true).
        // A configured root that has since been deleted is skipped (the
        // pre-dispatch seed already warned about it); aborting here would
        // abandon the build for every other root.
        let (present, missing): (Vec<PathBuf>, Vec<PathBuf>) =
            config_paths.into_iter().partition(|p| p.exists());
        for path in &missing {
            tracing::debug!(target: "indexing", "skipping missing configured path {}", path.display());
        }
        if present.is_empty() {
            eprintln!("Error: None of the configured paths exist");
            std::process::exit(1);
        }
        present
    };

    // Process each path, tracking total changes. Directories index as
    // ONE run: Phase 1 walks each root, resolution runs once after the
    // last root so cross-root imports bind regardless of registration
    // order.
    let mut total_indexed = 0usize;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for path in &paths_to_index {
        if path.is_file() {
            if dry_run {
                dry_run_single_file(path, dry_run_output);
            } else if index_single_file(indexer, path, force) {
                total_indexed += 1;
            }
        } else if path.is_dir() {
            dirs.push(path.clone());
        } else {
            eprintln!(
                "Error: Path does not exist: {}",
                crate::parsing::paths::render_absolute_path(path).display()
            );
            std::process::exit(1);
        }
    }
    if !dirs.is_empty() {
        total_indexed += index_directories(
            indexer,
            &dirs,
            progress,
            dry_run,
            force,
            max_files,
            dry_run_output,
        );
    }

    if !dry_run && total_indexed == 0 {
        tracing::debug!(target: "indexing", "no changes detected, skipping save");
    }

    // `--dry-run` never mutates the index even when `total_indexed` reports
    // a nonzero preview count, so it must never report "changes made" to a
    // caller that would otherwise save/publish.
    !dry_run && total_indexed > 0
}

/// Preview a single explicit file path under `--dry-run`, mirroring the
/// directory branch's `dry_run_output` rendering so `codanna index
/// somefile.rs --dry-run --json` does not silently run the real indexing
/// routine (an explicit file path is never filtered by the walker, so
/// previewing it is always exactly the one path given).
fn dry_run_single_file(path: &Path, dry_run_output: DryRunOutput) {
    match dry_run_output {
        DryRunOutput::Json => {
            let paths = [path.display().to_string()];
            match serde_json::to_string(&paths) {
                Ok(json) => println!("{json}"),
                Err(e) => {
                    eprintln!("Error: failed to serialize dry-run file list as JSON: {e}");
                    std::process::exit(1);
                }
            }
        }
        DryRunOutput::ListAll | DryRunOutput::Summary => {
            println!("Would index 1 files:");
            println!("  {}", path.display());
        }
    }
}

/// Index a single file. Returns true if file was indexed (not cached).
fn index_single_file(indexer: &mut IndexFacade, path: &PathBuf, force: bool) -> bool {
    match indexer.index_file_with_force(path, force) {
        Ok(result) => {
            let language_name = path
                .extension()
                .and_then(|ext| ext.to_str())
                .and_then(|ext| {
                    let registry = crate::parsing::get_registry();
                    registry
                        .lock()
                        .ok()
                        .and_then(|r| r.get_by_extension(ext).map(|def| def.name().to_string()))
                })
                .unwrap_or_else(|| "unknown".to_string());

            let was_indexed = !result.is_cached();

            if result.is_cached() {
                println!(
                    "Successfully loaded from cache: {} [{}]",
                    crate::parsing::paths::render_absolute_path(path).display(),
                    language_name
                );
            } else {
                println!(
                    "Successfully indexed: {} [{}]",
                    crate::parsing::paths::render_absolute_path(path).display(),
                    language_name
                );
            }
            println!("File ID: {}", result.file_id().value());

            // Get symbols for just this file
            let file_symbols = indexer.get_symbols_by_file(result.file_id());
            println!("Found {} symbols in this file", file_symbols.len());
            println!("Total symbols in index: {}", indexer.symbol_count());

            // Show summary of what was found in this file
            let functions = file_symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Function)
                .count();
            let methods = file_symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Method)
                .count();
            let structs = file_symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Struct)
                .count();
            let traits = file_symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Trait)
                .count();

            println!("  Functions: {functions}");
            println!("  Methods: {methods}");
            println!("  Structs: {structs}");
            println!("  Traits: {traits}");

            was_indexed
        }
        Err(e) => {
            eprintln!(
                "Error indexing file {}: {e}",
                crate::parsing::paths::render_absolute_path(path).display()
            );

            let suggestions = e.recovery_suggestions();
            if !suggestions.is_empty() {
                eprintln!("\nSuggestions:");
                for suggestion in suggestions {
                    eprintln!("  - {suggestion}");
                }
            }

            std::process::exit(1);
        }
    }
}

/// Index directories as one run. Returns the number of files changed --
/// indexed plus removed by deleted-file cleanup -- so a cleanup-only run
/// still counts as a change the caller must publish.
fn index_directories(
    indexer: &mut IndexFacade,
    dirs: &[PathBuf],
    progress: bool,
    dry_run: bool,
    force: bool,
    max_files: Option<usize>,
    dry_run_output: DryRunOutput,
) -> usize {
    // Visual separator before directory cycles (use stderr to sync with progress bars)
    eprintln!();

    // Show pre-indexing message only if we have a file limit (implies actual work)
    if let Some(max) = max_files {
        for dir in dirs {
            eprintln!(
                "Indexing directory: {} (limited to {} files)",
                crate::parsing::paths::render_absolute_path(dir).display(),
                max
            );
        }
    }

    match indexer.index_directories_with_options(
        dirs,
        progress,
        dry_run,
        force,
        max_files,
        dry_run_output,
    ) {
        Ok(all_stats) => {
            let mut files_changed = 0;
            for (dir, stats) in dirs.iter().zip(&all_stats) {
                // Deletions leave the progress trace at zero width; report them
                // explicitly so a cleanup-only run does not read as a no-op.
                if stats.files_removed > 0 {
                    eprintln!(
                        "Removed {} deleted file(s), {} symbol(s) from index",
                        stats.files_removed, stats.symbols_removed
                    );
                }
                // Print message only when no work happened (pipeline trace handles the rest)
                if stats.files_indexed == 0 && stats.files_removed == 0 {
                    eprintln!(
                        "Index up to date: {}",
                        crate::parsing::paths::render_absolute_path(dir).display()
                    );
                }
                files_changed += stats.files_indexed + stats.files_removed;
            }
            files_changed
        }
        Err(e) => {
            eprintln!("Error indexing directories: {e}");

            let suggestions = e.recovery_suggestions();
            if !suggestions.is_empty() {
                eprintln!("\nSuggestions:");
                for suggestion in suggestions {
                    eprintln!("  - {suggestion}");
                }
            }

            std::process::exit(1);
        }
    }
}

/// One row of `codanna index --status` output: a generation's identity,
/// classified state, on-disk size, age, and (only for `Damaged`
/// generations) the recorded validation-failure reason.
#[derive(Debug, Serialize)]
struct GenerationStatus {
    id: String,
    state: &'static str,
    age_seconds: u64,
    size_bytes: u64,
    error: Option<String>,
}

impl GenerationStatus {
    fn from_generation(
        layout: &IndexLayout,
        id: generation::GenerationId,
        state: GenerationState,
        size_bytes: u64,
        age: std::time::Duration,
    ) -> Self {
        // The recorded reason is diagnostic-only forensic text (see
        // `write_damaged_marker` in `storage::generation::layout`), so a
        // missing or unreadable `DAMAGED` file degrades to `None` rather
        // than failing the whole status report.
        let error = matches!(state, GenerationState::Damaged)
            .then(|| std::fs::read_to_string(layout.damaged_marker(&id)).ok())
            .flatten();
        Self {
            id: id.to_string(),
            state: state_label(state),
            age_seconds: age.as_secs(),
            size_bytes,
            error,
        }
    }
}

fn state_label(state: GenerationState) -> &'static str {
    match state {
        GenerationState::Current => "current",
        GenerationState::Previous => "previous",
        GenerationState::Building => "building",
        GenerationState::Orphan => "orphan",
        GenerationState::Damaged => "damaged",
        GenerationState::Incompatible => "incompatible",
    }
}

/// Print current-plus-per-generation index status and exit. Read-only:
/// never builds, migrates, or writes anything -- pure inspection of what
/// [`generation::list_generations`] already reports on disk. Callers must
/// invoke this without first constructing an [`IndexFacade`]: `IndexFacade::new`
/// migrates a flat-layout index and bootstraps an empty `current` generation
/// when nothing resolves, both of which are writes that `--status` must
/// never trigger. On a never-indexed workspace (or one still in the
/// pre-generation flat layout) `list_generations` reports no rows, and
/// this prints "No index found." rather than fabricating a generation.
pub fn run_status(config: &Settings, json: bool) {
    let layout = IndexLayout::new(config.index_path.clone());

    let rows: Vec<GenerationStatus> = match generation::list_generations(&layout) {
        Ok(entries) => entries
            .into_iter()
            .map(|(id, state, size, age)| {
                GenerationStatus::from_generation(&layout, id, state, size, age)
            })
            .collect(),
        Err(e) => {
            eprintln!("Error reading index generation status: {e}");
            std::process::exit(1);
        }
    };

    if json {
        match serde_json::to_string(&rows) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("Error: failed to serialize index status as JSON: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if rows.is_empty() {
        if layout.root().join("tantivy").join("meta.json").is_file() {
            println!(
                "Legacy flat index layout, not yet migrated. Any command that opens the index (e.g. `codanna index`) migrates it into a generation."
            );
        } else {
            println!("No index found.");
        }
        return;
    }
    for row in &rows {
        print!(
            "{}  state={}  age={}s  size={}B",
            row.id, row.state, row.age_seconds, row.size_bytes
        );
        if let Some(error) = &row.error {
            print!("  error={error}");
        }
        println!();
    }
}

/// Run garbage collection over on-disk generations and print a summary,
/// then exit. Constructs the [`IndexLayout`] directly rather than opening
/// an [`IndexFacade`] -- mirroring [`run_status`]'s read-first-no-facade
/// pattern -- so this never triggers `IndexFacade::new`'s
/// bootstrap-on-open, which would write a fresh `current` generation to a
/// never-indexed workspace.
pub fn run_gc(config: &Settings) {
    let layout = IndexLayout::new(config.index_path.clone());

    match generation::gc(&layout, true) {
        Ok(summary) => {
            println!(
                "gc: removed={} retried_later={} skipped_locked={}",
                summary.removed.len(),
                summary.retried_later.len(),
                summary.skipped_locked
            );
        }
        Err(e) => {
            eprintln!("Error running garbage collection: {e}");
            std::process::exit(1);
        }
    }
}

/// Roll `current` back to `id` (or, when `id` is `None`, the newest
/// [`GenerationState::Previous`] generation), then exit. Same
/// no-facade pattern as [`run_status`]/[`run_gc`]. Refuses -- loudly,
/// leaving `current` untouched -- to roll back onto a generation that
/// fails [`generation::validate_generation`], surfacing the recorded
/// damage reason rather than silently flipping to a broken generation.
pub fn run_rollback(config: &Settings, id: Option<String>) {
    let layout = IndexLayout::new(config.index_path.clone());

    let target = match id {
        Some(raw) => match GenerationId::new(&raw) {
            Some(gid) => gid,
            None => {
                eprintln!("Error: invalid generation id: {raw}");
                std::process::exit(2);
            }
        },
        None => {
            let rows = match generation::list_generations(&layout) {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("Error reading index generation status: {e}");
                    std::process::exit(1);
                }
            };
            match rows
                .into_iter()
                .filter(|(_, state, ..)| matches!(state, GenerationState::Previous))
                .max_by(|a, b| a.0.cmp(&b.0))
            {
                Some((gid, ..)) => gid,
                None => {
                    eprintln!("Error: no previous generation available to roll back to");
                    std::process::exit(1);
                }
            }
        }
    };

    if let Err(e) = generation::validate_generation(&layout, &target) {
        eprintln!("Error: refusing to roll back to a damaged generation: {e}");
        std::process::exit(1);
    }

    // Hold the same `publish_lock` `IndexPersistence::publish` uses around
    // its own read-current/write-current sequence: `write_current`'s
    // compare-and-swap is only atomic with respect to other holders of this
    // lock, not an unlocked reader-then-writer racing a concurrent publish.
    let lock_path = layout.publish_lock();
    let lock_file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "Error: failed to open publish lock {}: {e}",
                lock_path.display()
            );
            std::process::exit(1);
        }
    };
    #[allow(clippy::incompatible_msrv)]
    if let Err(e) = lock_file.lock() {
        eprintln!(
            "Error: failed to acquire publish lock {}: {e}",
            lock_path.display()
        );
        std::process::exit(1);
    }

    let old_current = match layout.read_current() {
        Ok(current) => current,
        Err(e) => {
            eprintln!("Error reading current generation pointer: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = layout.write_current(&target) {
        eprintln!("Error: failed to roll back current generation pointer: {e}");
        std::process::exit(1);
    }

    #[allow(clippy::incompatible_msrv)]
    let _ = lock_file.unlock();

    let old_txt = old_current
        .as_ref()
        .map(GenerationId::to_string)
        .unwrap_or_else(|| "none".to_string());
    println!("Rolled back current generation: {old_txt} -> {target}");
}
