//! Index generation primitives.
//!
//! Layout contract: generations live under `gen/<id>/` inside the index
//! root, each identified by a [`GenerationId`]. A `current` file at the
//! index root points at the generation directory currently considered
//! active; readers and writers resolve it via [`IndexLayout`] rather than
//! assuming a fixed path. [`markers`] records generation lifecycle state
//! (building/complete) on disk, [`layout`] classifies a generation's
//! [`GenerationState`] and enumerates/validates generation directories, and
//! [`mod@gc`] reclaims generations that are no longer useful.
//!
//! This module has zero production callers as of this phase -- it is
//! introduced as self-contained plumbing for a later work item to wire
//! into the indexing and CLI/MCP surfaces.

pub mod gc;
pub mod id;
pub mod layout;
pub mod markers;

pub use gc::{GcSummary, gc, gc_logged};
pub use id::GenerationId;
pub use layout::{
    GenerationState, IndexLayout, ResolvedGeneration, classify, clone_generation,
    free_space_preflight, list_generations, migrate_flat_layout, resolve_current,
    resolve_current_with_recovery, validate_generation,
};
pub use markers::{Building, Complete, CompleteFileEntry};
