//! The import report returned by every git_bridge import.

use std::time::Duration;

use gardenbed_model::Id;

/// What an import did.
///
/// Counters are exact: `commits_total = commits_new + commits_skipped`
/// always, and `objects_written` counts the objects objects (blobs *and*
/// derived trees) actually created during this run — idempotent re-puts
/// are not writes, so re-importing an unchanged repository reports zero.
#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    /// Commits this run touched: newly ingested plus mapped commits that
    /// were skipped. A full import of a fresh store reports the whole
    /// reachable graph; an incremental run reports only what the walk
    /// reached, because mapped ancestry is pruned (see the crate docs).
    pub commits_total: u64,
    /// Commits turned into new changesets (and their objects written).
    pub commits_new: u64,
    /// Commits whose pre-existing git-SHA mapping made ingestion (and, for
    /// a full import, the entire walk below them) unnecessary: for a full
    /// import the mapped frontier at which the walk stopped, for a
    /// `--limit` import the mapped commits inside the slice. Their objects
    /// are never touched.
    pub commits_skipped: u64,
    /// New objects entries created by this import (blobs + derived trees),
    /// counted as actual object-store writes (`has()` was false before the
    /// put — equivalent to counting `<root>/objects` directory entries
    /// before and after for the filesystem backend).
    pub objects_written: u64,
    /// Branch refs created or moved by this import, as `(full ref name,
    /// changeset id)`, sorted by name. Refs that already pointed at the
    /// same changeset are absent (an unchanged re-import reports none).
    pub refs_updated: Vec<(String, Id)>,
    /// Wall-clock duration of the whole import (graph walk + ingestion).
    pub elapsed: Duration,
}

impl ImportReport {
    /// Newly ingested commits per second of wall-clock time (0 when the
    /// import was instant).
    pub fn commits_per_second(&self) -> f64 {
        if self.elapsed.as_secs_f64() <= 0.0 {
            return 0.0;
        }
        self.commits_new as f64 / self.elapsed.as_secs_f64()
    }
}

impl std::fmt::Display for ImportReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} commits ({} new, {} skipped), {} objects, {} ref(s) moved, {:.2}s ({:.0} commits/s)",
            self.commits_total,
            self.commits_new,
            self.commits_skipped,
            self.objects_written,
            self.refs_updated.len(),
            self.elapsed.as_secs_f64(),
            self.commits_per_second(),
        )
    }
}
