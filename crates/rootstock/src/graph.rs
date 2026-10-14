//! The commit graph arena: pass-1 walk and topological ordering.
//!
//! Every reachable commit gets a compact `u32` arena id (parents and
//! children refer to each other by index; git ids only appear at the edges
//! — object reads and the mapping table). The walk is a deterministic BFS
//! from the selected ref tips (tips deduplicated; the caller passes them
//! ref-name-sorted), and parents are stored in git's stored order (first
//! parent = delta base).
//!
//! Two walk modes share one shape:
//!
//! * **Full import** (no `limit`): commits that already have a git-SHA
//!   mapping are *pruned* — recorded as arena leaves (their changeset id is
//!   kept for linking) but never descended into. This is sound because a
//!   mapped commit's whole ancestry is mapped too (git ids are
//!   content-addressed: an unchanged SHA means unchanged history below), so
//!   nothing unreachable can hide behind a mapped node. One documented
//!   exception: commits re-rooted by a `limit` import are mapped while
//!   having unmapped parents below — a later *full* import therefore cannot
//!   re-ingest those parents; the interaction is documented on
//!   [`ImportOptions::limit`](crate::ImportOptions::limit).
//! * **`limit` slice**: nothing is pruned (the global topological order
//!   over the *whole* reachable graph defines the slice), so mapped
//!   commits participate too.
//!
//! The Kahn ordering covers exactly the nodes a run will touch: unmapped
//! nodes for a full import (a mapped parent is already satisfied and never
//! counts toward indegree), every node for a `limit` slice. Octopus merges
//! need no special casing — every parent is just an edge. Seeding in arena
//! (discovery) order makes the whole order deterministic for a given repo
//! state, which is what `--limit` slicing relies on.

use std::collections::{HashMap, VecDeque};

use gardenbed_model::Id;
use gardenbed_store::MetadataStore;

use crate::commit::{parse_commit, RawCommit};
use crate::error::ImportError;
use crate::oid::GitOid;

/// One commit in the arena.
#[derive(Debug)]
pub(crate) struct Node {
    /// Git commit id.
    pub oid: GitOid,
    /// The commit's tree (read during the walk; reused for the empty-diff
    /// sanity check and re-roots).
    pub tree: GitOid,
    /// Arena indices of the git parents, in stored order (first parent =
    /// delta base).
    pub parents: Vec<u32>,
    /// Arena indices of the children (reverse edges for Kahn; consumed by
    /// [`Arena::ingest_order`]).
    pub children: Vec<u32>,
    /// `true` when a git-SHA mapping already existed at walk time
    /// (incremental import) — the node is never ingested again.
    pub mapped: bool,
    /// The changeset id: the pre-existing mapping for `mapped` nodes, or
    /// the freshly created one for ingested nodes.
    pub cs: Option<Id>,
    /// Kahn indegree: the number of queued parents not yet done.
    pub indeg: usize,
}

/// All commits reachable from the selected tips.
#[derive(Debug, Default)]
pub(crate) struct Arena {
    pub nodes: Vec<Node>,
    /// git id → arena index.
    pub index: HashMap<GitOid, u32>,
}

impl Arena {
    /// Walks the graph from the given tips (unique commit ids), consulting
    /// the metadata store for existing mappings. `prune_mapped` is `true`
    /// for full imports (see the module docs) and `false` for `limit`
    /// slices.
    pub fn walk(
        repo: &gix::Repository,
        tips: &[GitOid],
        metadata: &dyn MetadataStore,
        prune_mapped: bool,
    ) -> Result<Arena, ImportError> {
        let mut index: HashMap<GitOid, u32> = HashMap::with_capacity(tips.len() * 8);
        let mut oids: Vec<GitOid> = Vec::new();
        let mut trees: Vec<GitOid> = Vec::new();
        let mut mapped: Vec<bool> = Vec::new();
        let mut cs: Vec<Option<Id>> = Vec::new();
        let mut raw_parents: Vec<Vec<GitOid>> = Vec::new();
        let mut queue: VecDeque<GitOid> = tips.iter().copied().collect();

        while let Some(oid) = queue.pop_front() {
            if index.contains_key(&oid) {
                continue;
            }
            let sha_hex = oid.to_hex();
            let existing = metadata.cs_for_git(&sha_hex)?;
            let is_mapped = existing.is_some();
            let raw = read_commit(repo, oid, &sha_hex)?;
            index.insert(oid, oids.len() as u32);
            oids.push(oid);
            trees.push(raw.tree);
            mapped.push(is_mapped);
            cs.push(existing);
            raw_parents.push(raw.parents.clone());
            // Pruned (mapped) nodes are leaves: their history is assumed
            // mapped (see module docs), so it is not walked.
            if prune_mapped && is_mapped {
                continue;
            }
            for p in &raw.parents {
                queue.push_back(*p);
            }
        }

        // Link pass: resolve git parent ids to arena indices. Every parent
        // of an unmapped node is in the arena (the walk descends through
        // all of them); a pruned node's parents may be absent — those edges
        // are simply not needed (mapped nodes are never ingested).
        let mut nodes: Vec<Node> = oids
            .into_iter()
            .zip(trees)
            .zip(mapped)
            .zip(cs)
            .map(|(((oid, tree), mapped), cs)| Node {
                oid,
                tree,
                parents: Vec::new(),
                children: Vec::new(),
                mapped,
                cs,
                indeg: 0,
            })
            .collect();
        for (i, gps) in raw_parents.iter().enumerate() {
            let mut parents = Vec::with_capacity(gps.len());
            for gp in gps {
                if let Some(pidx) = index.get(gp) {
                    let pidx = *pidx;
                    parents.push(pidx);
                    nodes[pidx as usize].children.push(i as u32);
                }
            }
            nodes[i].parents = parents;
        }
        Ok(Arena { nodes, index })
    }

    /// Kahn topological order over the nodes this run touches:
    /// unmapped-only when `limit` is `None`, the whole graph otherwise
    /// (sliced to the last `limit` entries — the "newest" suffix).
    ///
    /// Returns the arena indices in ingestion order. Errors on a cycle
    /// (git graphs are acyclic; anything else is corruption).
    pub fn ingest_order(&mut self, limit: Option<usize>) -> Result<Vec<u32>, ImportError> {
        let full_graph = limit.is_some();
        let n = self.nodes.len();
        // Indegree pass (read-only): a limit slice counts every parent
        // (the whole graph is walked); a full import counts only parents
        // that will themselves be ingested (mapped ones are satisfied).
        let indegs: Vec<usize> = self
            .nodes
            .iter()
            .map(|node| {
                if full_graph {
                    node.parents.len()
                } else {
                    node.parents
                        .iter()
                        .filter(|p| !self.nodes[**p as usize].mapped)
                        .count()
                }
            })
            .collect();
        for (i, indeg) in indegs.into_iter().enumerate() {
            self.nodes[i].indeg = indeg;
        }
        let mut queue: VecDeque<u32> = (0..n)
            .filter(|&i| {
                let node = &self.nodes[i];
                (full_graph || !node.mapped) && node.indeg == 0
            })
            .map(|i| i as u32)
            .collect();
        let mut order = Vec::with_capacity(n);
        while let Some(idx) = queue.pop_front() {
            order.push(idx);
            let children = std::mem::take(&mut self.nodes[idx as usize].children);
            for c in children {
                if full_graph || !self.nodes[c as usize].mapped {
                    self.nodes[c as usize].indeg -= 1;
                    if self.nodes[c as usize].indeg == 0 {
                        queue.push_back(c);
                    }
                }
            }
        }
        let expected = if full_graph {
            n
        } else {
            self.nodes.iter().filter(|n| !n.mapped).count()
        };
        if order.len() != expected {
            return Err(ImportError::Other(format!(
                "commit graph is not a DAG (Kahn covered {} of {} nodes)",
                order.len(),
                expected
            )));
        }
        if full_graph {
            let limit = limit.unwrap_or(usize::MAX);
            let start = order.len().saturating_sub(limit);
            Ok(order.split_off(start))
        } else {
            Ok(order)
        }
    }
}

/// Reads a commit object and parses its header fields.
pub(crate) fn read_commit(
    repo: &gix::Repository,
    oid: GitOid,
    sha_hex: &str,
) -> Result<RawCommit, ImportError> {
    let obj = repo
        .find_object(gix_id(oid))
        .map_err(|e| ImportError::Object {
            oid: sha_hex.to_owned(),
            detail: format!("cannot read commit: {e}"),
        })?;
    if obj.kind != gix::object::Kind::Commit {
        return Err(ImportError::Object {
            oid: sha_hex.to_owned(),
            detail: format!("expected a commit object, found {:?}", obj.kind),
        });
    }
    parse_commit(&obj.data).map_err(|detail| ImportError::Commit {
        sha: sha_hex.to_owned(),
        detail,
    })
}

/// Converts a raw git id into gix's `ObjectId`.
pub(crate) fn gix_id(oid: GitOid) -> gix::ObjectId {
    gix::ObjectId::try_from(oid.as_bytes().as_slice())
        .expect("20 raw bytes always form a gix ObjectId")
}
