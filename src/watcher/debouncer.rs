//! Shared debouncing logic for file change events.
//!
//! Debouncing prevents excessive re-indexing when files are saved
//! multiple times in quick succession (e.g., auto-save, IDE formatting).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Debounces file change events by path.
///
/// Records change timestamps and returns paths that have been stable
/// for the configured duration.
#[derive(Debug)]
pub struct Debouncer {
    /// Pending changes: path -> last change timestamp.
    pending: HashMap<PathBuf, Instant>,
    /// Pending removal observations: path -> last change timestamp.
    /// Removals defer like modifications: a rename arrives as
    /// remove(old) + create(new), and only a window holding both sides
    /// lets the shared discovery pair them.
    pending_removals: HashMap<PathBuf, Instant>,
    /// How long a file must be stable before processing.
    duration: Duration,
}

impl Debouncer {
    /// Create a new debouncer with the given duration in milliseconds.
    pub fn new(debounce_ms: u64) -> Self {
        Self {
            pending: HashMap::new(),
            pending_removals: HashMap::new(),
            duration: Duration::from_millis(debounce_ms),
        }
    }

    /// Record a file change event.
    ///
    /// Resets the debounce timer for this path. Cancels a pending
    /// removal of the same path: the last observation wins.
    pub fn record(&mut self, path: PathBuf) {
        self.pending_removals.remove(&path);
        self.pending.insert(path, Instant::now());
    }

    /// Record a removal observation for this path.
    ///
    /// Cancels a pending modification of the same path: the last
    /// observation wins.
    pub fn record_removal(&mut self, path: PathBuf) {
        self.pending.remove(&path);
        self.pending_removals.insert(path, Instant::now());
    }

    /// Remove a path from pending (both flavors).
    pub fn remove(&mut self, path: &PathBuf) {
        self.pending.remove(path);
        self.pending_removals.remove(path);
    }

    /// Take all paths that have been stable for the debounce duration.
    ///
    /// Returns paths ready for processing and removes them from pending.
    pub fn take_ready(&mut self) -> Vec<PathBuf> {
        Self::take_stable(&mut self.pending, self.duration)
    }

    /// Drain the whole burst once every entry of BOTH flavors is stable,
    /// or once the oldest entry of either flavor has been held for
    /// `max_hold`.
    ///
    /// Returns `(removed, modified)`, or `None` while any entry is still
    /// inside its window: the matching create of a rename may trail the
    /// remove, and a wave missing one side deletes what actually moved.
    /// `None` when no removal is pending -- modification-only bursts take
    /// the per-path `take_ready` lane. A steady stream of fresh
    /// modifications can otherwise keep `stable(&self.pending)` false
    /// forever, starving a removal that has been ready to drain since its
    /// own window closed; `max_hold` bounds how long that removal waits.
    pub fn take_settled_burst(
        &mut self,
        max_hold: Duration,
    ) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
        let now = Instant::now();
        let stable = |pending: &HashMap<PathBuf, Instant>| {
            pending
                .values()
                .all(|last| now.duration_since(*last) >= self.duration)
        };
        if self.pending_removals.is_empty() {
            return None;
        }
        let stable_both = stable(&self.pending) && stable(&self.pending_removals);
        if !stable_both {
            let oldest = self
                .pending
                .values()
                .chain(self.pending_removals.values())
                .min();
            let forced = match oldest {
                Some(oldest) => now.duration_since(*oldest) >= max_hold,
                None => false,
            };
            if !forced {
                return None;
            }
        }
        let removed = self
            .pending_removals
            .drain()
            .map(|(path, _)| path)
            .collect();
        let modified = self.pending.drain().map(|(path, _)| path).collect();
        Some((removed, modified))
    }

    fn take_stable(pending: &mut HashMap<PathBuf, Instant>, duration: Duration) -> Vec<PathBuf> {
        let now = Instant::now();
        let mut ready = Vec::new();

        pending.retain(|path, last_change| {
            if now.duration_since(*last_change) >= duration {
                ready.push(path.clone());
                false // Remove from pending
            } else {
                true // Keep in pending
            }
        });

        ready
    }

    /// Check if there are any pending changes of either flavor.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty() || !self.pending_removals.is_empty()
    }

    /// Check if any removal observation is pending.
    pub fn has_pending_removals(&self) -> bool {
        !self.pending_removals.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_debouncer_basic() {
        let mut debouncer = Debouncer::new(50); // 50ms debounce

        let path = PathBuf::from("/test/file.rs");
        debouncer.record(path.clone());

        // Immediately after, nothing should be ready
        assert!(debouncer.take_ready().is_empty());
        assert!(debouncer.has_pending());

        // Wait for debounce period
        sleep(Duration::from_millis(60));

        // Now it should be ready
        let ready = debouncer.take_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0], path);
        assert!(!debouncer.has_pending());
    }

    #[test]
    fn test_debouncer_resets_on_new_change() {
        let mut debouncer = Debouncer::new(50);

        let path = PathBuf::from("/test/file.rs");
        debouncer.record(path.clone());

        // Wait half the debounce period
        sleep(Duration::from_millis(30));

        // Record again - should reset the timer
        debouncer.record(path.clone());

        // Wait another 30ms (total 60ms from first, but only 30ms from second)
        sleep(Duration::from_millis(30));

        // Should not be ready yet (need 50ms from last change)
        assert!(debouncer.take_ready().is_empty());

        // Wait for the remaining time
        sleep(Duration::from_millis(30));

        // Now it should be ready
        let ready = debouncer.take_ready();
        assert_eq!(ready.len(), 1);
    }

    #[test]
    fn test_debouncer_multiple_files() {
        let mut debouncer = Debouncer::new(50);

        let path1 = PathBuf::from("/test/file1.rs");
        let path2 = PathBuf::from("/test/file2.rs");

        debouncer.record(path1.clone());
        sleep(Duration::from_millis(30));
        debouncer.record(path2.clone());

        // Wait for path1 to be ready (50ms total)
        sleep(Duration::from_millis(25));

        let ready = debouncer.take_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0], path1);

        // path2 should still be pending
        assert!(debouncer.has_pending());

        // Wait for path2
        sleep(Duration::from_millis(30));

        let ready = debouncer.take_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0], path2);
    }

    // Removals defer like modifications: a rename arrives as remove(old)
    // + create(new), and only a window holding both sides lets the shared
    // discovery pair them.
    #[test]
    fn removals_defer_for_the_debounce_window() {
        let mut debouncer = Debouncer::new(50);

        let path = PathBuf::from("/test/old.rs");
        debouncer.record_removal(path.clone());

        let large_max_hold = Duration::from_secs(3600);
        assert!(debouncer.take_settled_burst(large_max_hold).is_none());
        assert!(debouncer.has_pending());
        assert!(debouncer.has_pending_removals());

        sleep(Duration::from_millis(60));

        let (removed, modified) = debouncer.take_settled_burst(large_max_hold).unwrap();
        assert_eq!(removed, vec![path]);
        assert!(modified.is_empty());
        assert!(!debouncer.has_pending());
    }

    // The burst drains only when EVERY side is stable: a create trailing
    // the remove must not be split off into its own per-file wave, or the
    // pairing sees a deleted file with no new side.
    #[test]
    fn burst_holds_until_every_side_is_stable() {
        let mut debouncer = Debouncer::new(50);
        let old = PathBuf::from("/test/old.rs");
        let new = PathBuf::from("/test/new.rs");
        let large_max_hold = Duration::from_secs(3600);

        debouncer.record_removal(old.clone());
        sleep(Duration::from_millis(60));
        // The removal alone is stable, but the trailing create is not.
        debouncer.record(new.clone());
        assert!(debouncer.take_settled_burst(large_max_hold).is_none());

        sleep(Duration::from_millis(60));
        let (removed, modified) = debouncer.take_settled_burst(large_max_hold).unwrap();
        assert_eq!(removed, vec![old]);
        assert_eq!(modified, vec![new]);

        // Second phase: a max_hold already exceeded by the oldest entry
        // force-drains the burst even though the trailing create is not
        // yet stable, pinning the bound so the old unbounded-hold
        // semantics cannot silently return.
        let old2 = PathBuf::from("/test/old2.rs");
        let new2 = PathBuf::from("/test/new2.rs");
        debouncer.record_removal(old2.clone());
        sleep(Duration::from_millis(60));
        // The removal alone is stable, but the trailing create is not --
        // and the oldest entry (the removal) has already been held well
        // past this small max_hold.
        debouncer.record(new2.clone());
        let small_max_hold = Duration::from_millis(10);
        let (removed, modified) = debouncer.take_settled_burst(small_max_hold).unwrap();
        assert_eq!(removed, vec![old2]);
        assert_eq!(modified, vec![new2]);
    }

    #[test]
    fn opposite_observations_for_one_path_cancel() {
        let mut debouncer = Debouncer::new(50);
        let path = PathBuf::from("/test/file.rs");
        let large_max_hold = Duration::from_secs(3600);

        // Remove then create: the file is alive; only a modification fires.
        debouncer.record_removal(path.clone());
        debouncer.record(path.clone());
        sleep(Duration::from_millis(60));
        assert!(
            debouncer.take_settled_burst(large_max_hold).is_none(),
            "no removal pending after the create cancelled it"
        );
        assert_eq!(debouncer.take_ready(), vec![path.clone()]);

        // Create then remove: the file is gone; only a removal fires.
        debouncer.record(path.clone());
        debouncer.record_removal(path.clone());
        sleep(Duration::from_millis(60));
        let (removed, modified) = debouncer.take_settled_burst(large_max_hold).unwrap();
        assert_eq!(removed, vec![path]);
        assert!(modified.is_empty());
    }

    // A steady stream of fresh modifications must not starve a removal
    // that has been stable and ready to drain since its own window
    // closed: `max_hold` bounds how long the removal waits behind the
    // churn. On the pre-fix code (no bound) this returns `None` forever.
    #[test]
    fn starved_removal_burst_drains_after_max_hold() {
        let mut debouncer = Debouncer::new(50);
        let removed_path = PathBuf::from("/test/removed.rs");
        let churn_path = PathBuf::from("/test/churn.rs");
        let max_hold = Duration::from_millis(120);

        debouncer.record_removal(removed_path.clone());

        let mut drained = None;
        for _ in 0..10 {
            debouncer.record(churn_path.clone());
            sleep(Duration::from_millis(30));
            if let Some(result) = debouncer.take_settled_burst(max_hold) {
                drained = Some(result);
                break;
            }
        }

        let (removed, _modified) = drained.expect(
            "burst must force-drain once the oldest entry exceeds max_hold, \
             even while the churner keeps `pending` unstable",
        );
        assert!(removed.contains(&removed_path));
    }

    #[test]
    fn test_debouncer_remove() {
        let mut debouncer = Debouncer::new(50);

        let path = PathBuf::from("/test/file.rs");
        debouncer.record(path.clone());
        assert!(debouncer.has_pending());

        debouncer.remove(&path);
        assert!(!debouncer.has_pending());
    }
}
