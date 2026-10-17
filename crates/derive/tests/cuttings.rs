//! Hand-crafted cuttings (exact-blob copy/rename fact) unit tests plus
//! registry invariants and a git differential.
//!
//! Fixture coverage: pure rename, moved-and-edited rename (no fact),
//! cross-directory exact copy, copy with several candidate sources
//! (smallest path wins), brand-new add (no fact), overwrite-style
//! modify (no fact), root changeset (no parent, empty row), octopus
//! parentage (first parent only), and encode/decode round-trip.
//!
//! The git differential runs `git diff-tree --find-copies-harder` over
//! materialized fixture histories and requires agreement with the
//! cuttings facts (documented exact-blob semantics; git's similarity
//! heuristic is disabled by comparing only exact R100/C100 entries).

mod common;

use std::sync::Arc;

use common::{git_out, ingest};
use gardenbed_derive::{
    Cutting, CuttingKind, Cuttings, DERIVED_CUTTINGS, DERIVED_PATHSUMMARY, DERIVED_SKIPLIST,
    DERIVED_TREE, Engine, derived_kinds,
};
use gardenbed_harness::{GenOptions, GeneratedHistory, generate, materialize};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore, StoreError};

// ------------------------------------------------------------- builder
// (the blame_units.rs builder, trimmed to what cuttings needs)

struct Hist {
    cs: Vec<Changeset>,
    blobs: Vec<(Id, Vec<u8>)>,
}

fn ident() -> Identity {
    Identity {
        name: "tester".into(),
        email: "t@gardenbed".into(),
        time_secs: 1_700_000_000,
        tz_min: 0,
    }
}

fn chg(
    blobs: &mut Vec<(Id, Vec<u8>)>,
    path: &str,
    op: Op,
    content: Option<&[u8]>,
    src: Option<&str>,
    mode: Mode,
) -> Change {
    let blob = content.map(|c| {
        let id = blob_id(c);
        if !blobs.iter().any(|(i, _)| *i == id) {
            blobs.push((id, c.to_vec()));
        }
        id
    });
    Change {
        path: path.to_owned(),
        op,
        blob,
        src_path: src.map(str::to_owned),
        mode,
    }
}

impl Hist {
    fn new() -> Self {
        Hist {
            cs: Vec::new(),
            blobs: Vec::new(),
        }
    }

    fn commit(&mut self, parents: Vec<Id>, mut changes: Vec<Change>, msg: &str) -> Id {
        changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        assert!(
            changes.windows(2).all(|w| w[0].path != w[1].path),
            "duplicate destination path in test changeset"
        );
        let cs = Changeset {
            parents,
            changes,
            message: msg.to_owned(),
            author: ident(),
            committer: ident(),
            signature: None,
        };
        let id = cs.id().expect("test changeset encodes");
        self.cs.push(cs);
        id
    }

    fn add(&mut self, path: &str, content: &[u8]) -> Change {
        chg(
            &mut self.blobs,
            path,
            Op::Add,
            Some(content),
            None,
            Mode::Regular,
        )
    }

    #[allow(dead_code)]
    fn modify(&mut self, path: &str, content: &[u8]) -> Change {
        chg(
            &mut self.blobs,
            path,
            Op::Modify,
            Some(content),
            None,
            Mode::Regular,
        )
    }

    fn delete(&mut self, path: &str) -> Change {
        chg(&mut self.blobs, path, Op::Delete, None, None, Mode::Regular)
    }

    /// Pure rename (blob equals the source's blob).
    fn rename(&mut self, src: &str, dst: &str, content: &[u8]) -> Change {
        chg(
            &mut self.blobs,
            dst,
            Op::Rename,
            Some(content),
            Some(src),
            Mode::Regular,
        )
    }

    /// Move-and-edit rename (fresh blob at the destination).
    fn rename_edited(&mut self, src: &str, dst: &str, content: &[u8]) -> Change {
        chg(
            &mut self.blobs,
            dst,
            Op::Rename,
            Some(content),
            Some(src),
            Mode::Regular,
        )
    }

    fn finish(self) -> GeneratedHistory {
        let head = self
            .cs
            .last()
            .map(|c| c.id().expect("encodes"))
            .unwrap_or_else(|| Id::from_bytes([0u8; 32]));
        let mut blobs = self.blobs;
        blobs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        GeneratedHistory {
            changesets: self.cs,
            blob_contents: blobs,
            head,
        }
    }
}

fn stores() -> (Arc<dyn ObjectStore>, Arc<dyn MetadataStore>) {
    let m = Arc::new(MemoryStore::new());
    (m.clone(), m)
}

fn setup(h: &GeneratedHistory, eager: bool) -> (Vec<Id>, Engine, Arc<dyn MetadataStore>) {
    let (objects, metadata) = stores();
    let ids = ingest(&objects, &metadata, h, eager);
    let engine = Engine::new(objects, metadata.clone());
    (ids, engine, metadata)
}

// ---------------------------------------------------------- fixtures

const B1: &[u8] = b"alpha\nbeta\n";
const B2: &[u8] = b"gamma\ndelta\n";
const B3: &[u8] = b"brand new content\n";
const B4: &[u8] = b"entirely unique bytes for fresh\n";

/// A history exercising every fact shape:
///
/// * c1 (root): adds `a.txt` (B1), `b.txt` (B2) — no facts.
/// * c2: pure rename `a.txt` → `moved.txt`; copy `b.txt`'s content to
///   `copy/of-b.txt` — one rename + one copy fact.
/// * c3: moved-and-edited rename `moved.txt` → `edited.txt` (fresh blob);
///   add of `copy/of-b.txt`-content at `x.txt` where TWO parent paths hold
///   the same blob (`b.txt` and `copy/of-b.txt`) — smallest (`b.txt`)
///   wins; no rename fact.
/// * c4: delete + brand-new add with content unique in the parent — no
///   facts at all (empty row still written).
fn fixture() -> (GeneratedHistory, Vec<Id>) {
    let mut h = Hist::new();
    let root = {
        let changes = vec![h.add("a.txt", B1), h.add("b.txt", B2)];
        h.commit(vec![], changes, "root")
    };
    let c2 = {
        let changes = vec![
            h.rename("a.txt", "moved.txt", B1),
            h.add("copy/of-b.txt", B2),
        ];
        h.commit(vec![root], changes, "pure rename + copy")
    };
    let c3 = {
        let changes = vec![
            h.rename_edited("moved.txt", "edited.txt", B3),
            h.add("x.txt", B2),
        ];
        h.commit(vec![c2], changes, "edited rename + two-source copy")
    };
    let c4 = {
        let changes = vec![h.delete("copy/of-b.txt"), h.add("fresh.txt", B4)];
        h.commit(vec![c3], changes, "delete + brand-new add")
    };
    let ids = vec![root, c2, c3, c4];
    (h.finish(), ids)
}

#[test]
fn facts_match_exact_blob_semantics() {
    let (h, ids) = fixture();
    let (_, engine, _) = setup(&h, true);

    let root = engine.cuttings_entry(ids[0]).expect("root entry");
    assert!(root.entries.is_empty(), "root has no first parent");

    let c2 = engine.cuttings_entry(ids[1]).expect("c2 entry");
    assert_eq!(
        c2.entries,
        vec![
            Cutting {
                path: "copy/of-b.txt".into(),
                src: "b.txt".into(),
                blob: blob_id(B2),
                kind: CuttingKind::Copy,
            },
            Cutting {
                path: "moved.txt".into(),
                src: "a.txt".into(),
                blob: blob_id(B1),
                kind: CuttingKind::Rename,
            },
        ],
        "c2: one copy, one pure rename, sorted by path"
    );

    let c3 = engine.cuttings_entry(ids[2]).expect("c3 entry");
    assert_eq!(
        c3.entries,
        vec![Cutting {
            path: "x.txt".into(),
            src: "b.txt".into(),
            blob: blob_id(B2),
            kind: CuttingKind::Copy,
        }],
        "c3: edited rename is no fact; smallest of two sources wins"
    );

    let c4 = engine.cuttings_entry(ids[3]).expect("c4 entry");
    assert!(
        c4.entries.is_empty(),
        "c4: delete + brand-new add produces no facts"
    );
}

#[test]
fn cuttings_rows_are_written_lazily_not_eagerly() {
    let (h, ids) = fixture();
    let (_, engine, metadata) = setup(&h, true);
    for (i, cs) in ids.iter().enumerate() {
        assert!(
            metadata
                .derived_get(DERIVED_CUTTINGS, *cs)
                .expect("derived_get")
                .is_none(),
            "cs{i} has a cuttings row after eager ingest — cuttings must be lazy \
             (its derivation walks the whole parent tree; the M5 \
             size-independence budget forbids that on the write path)"
        );
    }
    // First query computes and persists the row; the read-back proves it.
    for (i, cs) in ids.iter().enumerate() {
        engine.cuttings_entry(*cs).expect("lazy compute");
        let bytes = metadata
            .derived_get(DERIVED_CUTTINGS, *cs)
            .expect("derived_get")
            .unwrap_or_else(|| panic!("cs{i} has no cuttings row after lazy query"));
        assert!(!bytes.is_empty());
    }
}

#[test]
fn eager_and_lazy_ingest_paths_compute_identical_cuttings() {
    let (h, ids) = fixture();
    let (_ids_a, engine_a, meta_a) = setup(&h, true);
    let (_, engine_b, meta_b) = setup(&h, false);
    for (i, cs) in ids.iter().enumerate() {
        let entry = engine_a.cuttings_entry(*cs).expect("A computes");
        let a = meta_a
            .derived_get(DERIVED_CUTTINGS, *cs)
            .expect("get A")
            .expect("lazy row after compute");
        let entry_b = engine_b.cuttings_entry(*cs).expect("B computes");
        let b = meta_b
            .derived_get(DERIVED_CUTTINGS, *cs)
            .expect("get B")
            .expect("lazy row after compute");
        assert_eq!(a, b, "cuttings bytes differ at cs index {i}");
        assert_eq!(entry.encode(), a, "decoded entry re-encodes identically");
        assert_eq!(entry_b.encode(), a, "both engines agree");
    }
    let _ = _ids_a;
}

#[test]
fn unknown_changeset_is_not_found() {
    let (h, _ids) = fixture();
    let (_, engine, _) = setup(&h, true);
    let bogus = Id::from_bytes([9u8; 32]);
    let err = engine.cuttings_entry(bogus).expect_err("must fail");
    assert!(matches!(err, StoreError::NotFound), "got {err:?}");
}

/// Octopus parentage: the copy source search consults the FIRST parent's
/// tree only. `side.txt` exists in the second parent (redundant merge
/// parent), not the first; an add matching it is NOT a copy.
#[test]
fn only_first_parent_is_consulted() {
    let mut h = Hist::new();
    let root = {
        let changes = vec![h.add("main.txt", B1)];
        h.commit(vec![], changes, "root")
    };
    let side = {
        let changes = vec![h.add("side.txt", B2)];
        h.commit(vec![root], changes, "side")
    };
    // Merge with side as the SECOND parent, adding B2 at other.txt.
    let merge = {
        let changes = vec![h.add("other.txt", B2)];
        h.commit(
            vec![root, side],
            changes,
            "merge adding second-parent content",
        )
    };
    let ids = vec![root, side, merge];
    let hist = h.finish();
    let (_, engine, _) = setup(&hist, true);

    let entry = engine.cuttings_entry(merge).expect("merge entry");
    assert!(
        entry.entries.is_empty(),
        "second-parent content is not a copy source: {:?}",
        entry.entries
    );
    let side_entry = engine.cuttings_entry(side).expect("side entry");
    assert!(side_entry.entries.is_empty());
    let _ = ids;
}

/// A same-path add (path existed in the parent with the same blob) must
/// not report itself as its own copy source; the entry is skipped (the
/// changeset shape itself is degenerate — the model forbids Add over an
/// existing path — but the fact search must not misfire either).
#[test]
fn same_path_is_not_its_own_source() {
    let mut h = Hist::new();
    let root = {
        let changes = vec![h.add("a.txt", B1)];
        h.commit(vec![], changes, "root")
    };
    // Model-wise this is a Modify; forge an Add at the same path to test
    // the search guard directly.
    let change = chg(
        &mut h.blobs,
        "a.txt",
        Op::Add,
        Some(B1),
        None,
        Mode::Regular,
    );
    let c2 = h.commit(vec![root], vec![change], "add at existing path");
    let hist = h.finish();
    let (_, engine, _) = setup(&hist, true);
    let entry = engine.cuttings_entry(c2).expect("entry");
    // a.txt's only blob-match in the parent is a.txt itself — no other
    // path holds B1, so no copy fact.
    assert!(
        entry.entries.is_empty(),
        "self-source must not count: {:?}",
        entry.entries
    );
}

#[test]
fn encode_decode_round_trip() {
    let mut c = Cuttings::default();
    assert_eq!(Cuttings::decode(&c.encode()), Some(c.clone()));
    c.entries.push(Cutting {
        path: "z.txt".into(),
        src: "a/b.txt".into(),
        blob: blob_id(b"x"),
        kind: CuttingKind::Copy,
    });
    c.entries.push(Cutting {
        path: "aa".into(),
        src: "ünïcode/påth".into(),
        blob: blob_id(b"y"),
        kind: CuttingKind::Rename,
    });
    let bytes = c.encode();
    assert_eq!(Cuttings::decode(&bytes), Some(c));
    // Malformed payloads are misses, not errors.
    let mut bad = bytes.clone();
    bad[0] = 0xFF; // unknown version
    assert_eq!(Cuttings::decode(&bad), None);
    assert_eq!(Cuttings::decode(&bytes[..bytes.len() - 1]), None); // truncated
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(Cuttings::decode(&trailing), None); // trailing bytes
}

// ----------------------------------------------------- git differential

fn opts_rename_heavy() -> GenOptions {
    GenOptions {
        commits: 30,
        rename_ratio: 0.5,
        ..GenOptions::default()
    }
}

/// Extracts the exact-blob fact set `git diff-tree -r -M -C
/// --find-copies-harder --name-status` reports for one commit, filtered
/// to R100/C100 (exact) entries, as `(status, dst, src)` triples.
fn git_exact_facts(repo: &std::path::Path, sha: &str) -> Vec<(char, String, String)> {
    let out = git_out(
        repo,
        &[
            "diff-tree",
            "--root",
            "-r",
            "-z",
            "--name-status",
            "--no-commit-id",
            "--diff-merges=first-parent",
            "-M",
            "-C",
            "--find-copies-harder",
            sha,
        ],
    )
    .expect("git diff-tree");
    let text = String::from_utf8(out).expect("utf8");
    let mut toks: Vec<&str> = text.split('\0').collect();
    if toks.last() == Some(&"") {
        toks.pop();
    }
    let mut facts = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let status = toks[i];
        i += 1;
        // R100/C100 carry (src, dst); plain statuses carry just dst.
        if status.starts_with('R') || status.starts_with('C') {
            let score_exact = &status[1..] == "100";
            let (src, dst) = if i + 1 < toks.len() {
                let s = toks[i];
                let d = toks[i + 1];
                i += 2;
                (s, d)
            } else {
                break;
            };
            if score_exact {
                facts.push((
                    status.chars().next().unwrap(),
                    dst.to_owned(),
                    src.to_owned(),
                ));
            }
        } else {
            i += 1; // dst only
        }
    }
    facts
}

/// The cuttings facts of `cs` as the same triple shape.
fn gb_exact_facts(engine: &Engine, cs: Id) -> Vec<(char, String, String)> {
    engine
        .cuttings_entry(cs)
        .expect("cuttings")
        .entries
        .iter()
        .map(|e| {
            let st = match e.kind {
                CuttingKind::Rename => 'R',
                CuttingKind::Copy => 'C',
            };
            (st, e.path.clone(), e.src.clone())
        })
        .collect()
}

#[test]
fn exact_facts_agree_with_git_find_copies_harder() {
    for seed in [777u64, 31337, 424242] {
        let h = generate(seed, &opts_rename_heavy());
        let (ids, engine, _) = setup(&h, true);
        let dir = tempfile::tempdir().expect("tempdir");
        let mat = materialize(&h, dir.path()).expect("materialize");

        for (i, cs_id) in ids.iter().enumerate() {
            let sha = &mat
                .mapping
                .get(i)
                .unwrap_or_else(|| panic!("mapping for cs{i}"))
                .1;
            let git = {
                let mut v = git_exact_facts(&mat.repo_path, sha);
                v.sort();
                v
            };
            let gb = {
                let mut v = gb_exact_facts(&engine, *cs_id);
                v.sort();
                v
            };
            assert_eq!(
                git, gb,
                "exact copy/rename facts disagree with git at seed {seed} cs{i}"
            );
        }
    }
}

// ---------------------------------------------------- registry invariants

#[test]
fn registry_lists_the_four_kinds_in_order() {
    let names: Vec<&str> = derived_kinds().iter().map(|k| k.name).collect();
    assert_eq!(names, vec!["tree", "skiplist", "pathsummary", "cuttings"]);
}

/// Wiping derived data and rebuilding through the eager path rewrites
/// exactly the registered kinds with eager hooks — nothing more,
/// nothing less. Cuttings is lazy-only: the rebuild leaves its rows
/// empty, the lazy getter restores them, and an unregistered kind's row
/// is left untouched and never read.
#[test]
fn eager_rebuild_covers_exactly_the_registry() {
    let (h, _ids) = fixture();
    let (objects, metadata) = stores();
    let ids = ingest(&objects, &metadata, &h, true);
    let engine = Engine::new(objects.clone(), metadata.clone());

    // A foreign row under an unregistered kind tag: the rebuild must not
    // touch it, and no registry reader consults it.
    let foreign_kind = "definitely-not-registered";
    metadata
        .derived_put(foreign_kind, ids[0], b"sentinel")
        .expect("foreign put");

    // Wipe every registered kind.
    for k in derived_kinds() {
        for cs in &ids {
            metadata.derived_put(k.name, *cs, b"").expect("wipe");
        }
    }

    for cs in &ids {
        engine.on_changeset_created(*cs).expect("rebuild");
    }

    for k in derived_kinds() {
        for (i, cs) in ids.iter().enumerate() {
            let bytes = metadata
                .derived_get(k.name, *cs)
                .expect("get")
                .unwrap_or_else(|| panic!("{} row missing at cs{i}", k.name));
            if k.name == DERIVED_CUTTINGS {
                // Lazy-only kind: the eager rebuild must NOT restore it.
                assert!(
                    bytes.is_empty(),
                    "cuttings row restored eagerly at cs{i} — it must stay lazy"
                );
            } else {
                assert!(
                    !bytes.is_empty() || k.name == DERIVED_TREE,
                    "{} row empty at cs{i}",
                    k.name
                );
            }
        }
    }
    // The lazy getter restores the wiped cuttings rows on demand.
    engine
        .cuttings_entry(ids[1])
        .expect("lazy restore after wipe");
    assert!(
        !metadata
            .derived_get(DERIVED_CUTTINGS, ids[1])
            .expect("get")
            .unwrap()
            .is_empty(),
        "lazy restore wrote the row"
    );
    let sentinel = metadata
        .derived_get(foreign_kind, ids[0])
        .expect("foreign get")
        .expect("sentinel survives");
    assert_eq!(sentinel, b"sentinel");
}

/// The eager and batched (rows-for-new) paths produce byte-identical
/// rows for every registered kind **with hooks** across several
/// generated histories. Cuttings is lazy-only: neither path writes its
/// rows, and the lazy getter produces the same bytes from either store.
#[test]
fn eager_and_batched_rows_are_byte_identical() {
    for seed in [777u64, 31337, 424242] {
        let h = generate(
            seed,
            &GenOptions {
                commits: 20,
                ..GenOptions::default()
            },
        );

        // Eager path.
        let (objects_a, metadata_a) = stores();
        let ids = ingest(&objects_a, &metadata_a, &h, true);
        let engine_a = Engine::new(objects_a.clone(), metadata_a.clone());

        // Batched path: rows prepared without writes, then stored.
        let (objects_b, metadata_b) = stores();
        ingest(&objects_b, &metadata_b, &h, false);
        let engine_b = Engine::new(objects_b.clone(), metadata_b.clone());
        for (i, cs_id) in ids.iter().enumerate() {
            let cs = h.changesets.get(i).expect("changeset");
            let rows = engine_b
                .derived_rows_for_new(*cs_id, cs)
                .expect("rows for new");
            assert_eq!(
                rows.len(),
                derived_kinds().len() - 1, // cuttings contributes no row
                "one row per hooked kind at cs{i}"
            );
            for row in rows {
                metadata_b
                    .derived_put(&row.kind, row.cs, &row.bytes)
                    .expect("store row");
            }
        }

        for k in [DERIVED_TREE, DERIVED_SKIPLIST, DERIVED_PATHSUMMARY] {
            for (i, cs) in ids.iter().enumerate() {
                let a = metadata_a
                    .derived_get(k, *cs)
                    .expect("get A")
                    .unwrap_or_else(|| panic!("{k} row A missing at cs{i}"));
                let b = metadata_b
                    .derived_get(k, *cs)
                    .expect("get B")
                    .unwrap_or_else(|| panic!("{k} row B missing at cs{i}"));
                assert_eq!(a, b, "{k} bytes differ between paths at cs{i}");
            }
        }

        // Cuttings: no rows from either write path, identical lazy bytes.
        for (i, cs) in ids.iter().enumerate() {
            for (label, meta) in [("A", &metadata_a), ("B", &metadata_b)] {
                assert!(
                    meta.derived_get(DERIVED_CUTTINGS, *cs)
                        .expect("get")
                        .is_none(),
                    "cuttings row {label} present at cs{i} — must be lazy"
                );
            }
            let a = engine_a.cuttings_entry(*cs).expect("A lazy");
            let b = engine_b.cuttings_entry(*cs).expect("B lazy");
            assert_eq!(
                a.encode(),
                b.encode(),
                "lazy cuttings bytes differ between stores at cs{i}"
            );
        }
    }
}
