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
    /// Refs whose new tip is NOT a descendant of the ref's previous value
    /// (the ref moved backward, or sideways onto rewritten history):
    /// `(full ref name, old tip, new tip, fell_off)` where `fell_off` is
    /// the number of changesets reachable from the old tip but not the
    /// new one (`None` when the count was not computed), i.e. the work
    /// that fell off the ref. Sorted by name. Empty unless the previous
    /// value existed — moving a ref that had no prior value is a fast
    /// forward by definition.
    pub refs_rewound: Vec<RefRewind>,
    /// Raw tree rows captured this run: git tree objects whose canonical
    /// render diverges from the original bytes (pre-canonical modes such
    /// as `100664`, non-UTF-8 names), stored verbatim keyed by the
    /// original git tree SHA so export can emit them SHA-exact. Shared
    /// subtrees are decided once per run, so this counts distinct rows,
    /// and idempotent re-imports report zero (the walk's memo plus the
    /// store's immutability check).
    pub raw_trees_captured: u64,
    /// Wall-clock duration of the whole import (graph walk + ingestion).
    pub elapsed: Duration,
}

/// One backward (or sideways) ref move found by an import.
#[derive(Clone, Debug)]
pub struct RefRewind {
    /// The full ref name, e.g. `refs/heads/main`.
    pub name: String,
    /// The ref's value before this import moved it.
    pub old_tip: Id,
    /// The ref's value after this import moved it.
    pub new_tip: Id,
    /// Changesets reachable from the old tip but not the new one — the
    /// work that fell off the ref — or `None` when counting was skipped
    /// (limit imports, where slice re-rooting breaks real ancestry).
    pub fell_off: Option<u64>,
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
        )?;
        if self.raw_trees_captured > 0 {
            write!(f, ", {} raw tree(s) captured", self.raw_trees_captured)?;
        }
        if !self.refs_rewound.is_empty() {
            write!(f, ", {} ref(s) rewound", self.refs_rewound.len())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_rewound_refs_only_when_nonzero() {
        let mut report = ImportReport::default();
        assert!(!report.to_string().contains("rewound"));
        report.refs_rewound.push(RefRewind {
            name: "refs/heads/main".into(),
            old_tip: Id::from_bytes([1; 32]),
            new_tip: Id::from_bytes([2; 32]),
            fell_off: Some(3),
        });
        assert_eq!(
            report.to_string(),
            "0 commits (0 new, 0 skipped), 0 objects, 0 ref(s) moved, 0.00s (0 commits/s), 1 ref(s) rewound"
        );
    }
}
