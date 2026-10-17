//! Hand-crafted M6 unit tests: rename chains across merges, deleted-then-
//! readded lineage, rename onto an occupied path, empty/symlink/gitlink
//! paths, CRLF and missing trailing newlines, mode-only changes, the six
//! git-diff ambiguity shapes (captured from `git blame --first-parent`,
//! git 2.43), and the pathsummary kind (bounded list, overflow marker,
//! bloom soundness, rebuild determinism, poisoning resistance).
//!
//! Hand-computed expectations are asserted *and* cross-checked against
//! live git wherever git has defined behavior.

mod common;

use std::sync::Arc;

use common::ingest;
use gardenbed_derive::{BLOOM_BITS, DERIVED_PATHSUMMARY, Engine, MAX_SUMMARY_PATHS};
use gardenbed_harness::{
    GenOptions, GeneratedHistory, generate, git_blame_first_parent, materialize,
};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore, StoreError};

// ------------------------------------------------------------- builder

/// A tiny history builder: tracks blob contents, materializes canonical
/// changesets (changes ascending by path, blob ids content-addressed).
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

/// A submodule pointer: 12 zero bytes + a pseudo upstream sha (the
/// canonical encoding: the blob field carries the upstream git sha
/// left-padded to 32 bytes; objects never stores it as a blob).
fn gitlink_ptr(tag: u8) -> (Id, Vec<u8>) {
    let mut padded = [0u8; 32];
    padded[12] = tag;
    padded[13] = 0xAA;
    padded[14] = 0xBB;
    let id = Id::from_bytes(padded);
    (id, padded.to_vec())
}

impl Hist {
    fn new() -> Self {
        Hist {
            cs: Vec::new(),
            blobs: Vec::new(),
        }
    }

    /// Registers a submodule pointer so `materialize`/ingest can resolve it.
    fn register_gitlink(&mut self, tag: u8) -> Id {
        let (id, bytes) = gitlink_ptr(tag);
        if !self.blobs.iter().any(|(i, _)| *i == id) {
            self.blobs.push((id, bytes));
        }
        id
    }

    /// Commits `changes` (sorted canonically) with the given parents and
    /// returns the content-addressed changeset id.
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

    /// Builds the changes for one commit via `build` (which may register
    /// blobs), then commits them.
    fn commit_with(
        &mut self,
        parents: Vec<Id>,
        msg: &str,
        build: impl FnOnce(&mut Hist) -> Vec<Change>,
    ) -> Id {
        let changes = build(self);
        self.commit(parents, changes, msg)
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

    fn add_mode(&mut self, path: &str, content: &[u8], mode: Mode) -> Change {
        chg(&mut self.blobs, path, Op::Add, Some(content), None, mode)
    }

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

    /// Pure rename (the blob must equal the source's blob).
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

/// One store pair (objects + metadata share one MemoryStore).
fn stores() -> (Arc<dyn ObjectStore>, Arc<dyn MetadataStore>) {
    let m = Arc::new(MemoryStore::new());
    (m.clone(), m)
}

/// Ingests and returns ids plus the engine.
fn setup(h: &GeneratedHistory, eager: bool) -> (Vec<Id>, Engine, Arc<dyn MetadataStore>) {
    let (objects, metadata) = stores();
    let ids = ingest(&objects, &metadata, h, eager);
    let engine = Engine::new(objects, metadata.clone());
    (ids, engine, metadata)
}

fn blame_ok(engine: &Engine, cs: Id, path: &str) -> Vec<(u64, u64, Id)> {
    engine
        .blame(cs, path)
        .unwrap_or_else(|e| panic!("blame {path:?}: {e}"))
}

/// Maps engine ranges onto git ranges via the changeset→sha mapping.
fn to_git_norm(ranges: &[(u64, u64, Id)], mapping: &[(Id, String)]) -> Vec<(u64, u64, String)> {
    ranges
        .iter()
        .map(|(lo, hi, id)| {
            let sha = &mapping
                .iter()
                .find(|(i, _)| i == id)
                .unwrap_or_else(|| panic!("changeset {id} not in mapping"))
                .1;
            (*lo, *hi, sha.clone())
        })
        .collect()
}

fn git_norm(ranges: &[gardenbed_harness::BlameRange]) -> Vec<(u64, u64, String)> {
    ranges
        .iter()
        .map(|r| (r.lo as u64, r.hi as u64, r.commit_sha.clone()))
        .collect()
}

// ----------------------------------------------------- rename chains

/// A rename chain crossing a merge: `a.txt → b.txt → d/c.txt` (pure
/// renames), one modify before the chain and one after the merge. Blame
/// must follow the whole chain back to the content's origin.
#[test]
fn rename_chain_across_merge() {
    let mut h = Hist::new();
    let c0 = h.commit_with(vec![], "root", |h| vec![h.add("a.txt", b"l1\nl2\nl3\n")]);
    let c1 = h.commit_with(vec![c0], "append l4", |h| {
        vec![h.modify("a.txt", b"l1\nl2\nl3\nl4\n")]
    });
    let c2 = h.commit_with(vec![c1], "rename to b.txt", |h| {
        vec![h.rename("a.txt", "b.txt", b"l1\nl2\nl3\nl4\n")]
    });
    let c3 = h.commit_with(vec![c2], "rename into subdir", |h| {
        vec![h.rename("b.txt", "d/c.txt", b"l1\nl2\nl3\nl4\n")]
    });
    let c4 = h.commit_with(vec![c3, c1], "merge-time append l5", |h| {
        vec![h.modify("d/c.txt", b"l1\nl2\nl3\nl4\nl5\n")]
    });
    let c5 = h.commit_with(vec![c4], "append l6", |h| {
        vec![h.modify("d/c.txt", b"l1\nl2\nl3\nl4\nl5\nl6\n")]
    });
    let hh = h.finish();
    let (ids, engine, _) = setup(&hh, true);
    assert_eq!(ids, vec![c0, c1, c2, c3, c4, c5]);

    // At the end of the chain: every line traced to its introducer.
    let want = vec![(1, 3, c0), (4, 4, c1), (5, 5, c4), (6, 6, c5)];
    assert_eq!(blame_ok(&engine, c5, "d/c.txt"), want, "full chain blame");
    assert_eq!(
        engine.blame_no_skip(c5, "d/c.txt").unwrap(),
        want,
        "naive equal"
    );
    // Mid-chain: the merge is a head; its own append attributes to it.
    assert_eq!(
        blame_ok(&engine, c4, "d/c.txt"),
        vec![(1, 3, c0), (4, 4, c1), (5, 5, c4)]
    );
    // Pure renames carry attribution through untouched.
    assert_eq!(
        blame_ok(&engine, c3, "d/c.txt"),
        vec![(1, 3, c0), (4, 4, c1)]
    );
    assert_eq!(blame_ok(&engine, c2, "b.txt"), vec![(1, 3, c0), (4, 4, c1)]);
    assert_eq!(blame_ok(&engine, c1, "a.txt"), vec![(1, 3, c0), (4, 4, c1)]);
    assert_eq!(blame_ok(&engine, c0, "a.txt"), vec![(1, 3, c0)]);

    // Segment structure: c0 and c4 are heads; members hang below them.
    let e2 = engine.pathsummary_entry(c2).unwrap();
    assert!(!e2.is_head);
    assert_eq!(e2.head, Some(c0));
    assert_eq!(e2.head_dist, 2);
    assert_eq!(e2.first_parent, Some(c1));
    // Rename sources and destinations both count as touched.
    assert!(
        e2.bloom_maybe_contains("a.txt"),
        "rename src must be in the bloom"
    );
    assert!(
        e2.bloom_maybe_contains("b.txt"),
        "rename dst must be in the bloom"
    );
    assert!(!e2.bloom_maybe_contains("zzz/none.txt"));
    let e4 = engine.pathsummary_entry(c4).unwrap();
    assert!(e4.is_head);
    assert_eq!(e4.head, None);
    assert_eq!(e4.head_dist, 0);
    assert!(
        e4.bloom.iter().all(|&b| b == 0),
        "heads carry an all-zero segment bloom"
    );
    assert!(e4.paths.contains(&"d/c.txt".to_string()));
    assert_eq!(e4.first_parent, Some(c3));
}

/// Renaming INTO an occupied path is a content replace: the base is the
/// overwritten path's own content and the walk continues under the same
/// name (git follows no rename there). Cross-checked with git.
#[test]
fn rename_onto_occupied_path_matches_git() {
    let mut h = Hist::new();
    let c0 = h.commit_with(vec![], "root", |h| {
        vec![h.add("x.txt", b"old1\nold2\n"), h.add("y.txt", b"y1\n")]
    });
    let c1 = h.commit_with(vec![c0], "rename onto y.txt", |h| {
        vec![h.rename("x.txt", "y.txt", b"old1\nold2\n")]
    });
    let c2 = h.commit_with(vec![c1], "append", |h| {
        vec![h.modify("y.txt", b"old1\nold2\nold3\n")]
    });
    let hh = h.finish();
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&hh, dir.path()).expect("materialize");
    let (_ids, engine, _) = setup(&hh, true);

    // Overwritten y.txt: every line of the new content is new at c1.
    let want = vec![(1, 2, c1)];
    assert_eq!(blame_ok(&engine, c1, "y.txt"), want);
    assert_eq!(engine.blame_no_skip(c1, "y.txt").unwrap(), want);
    let git =
        git_blame_first_parent(&mat.repo_path, &mat.mapping[1].1, "y.txt").expect("git blames");
    assert_eq!(
        to_git_norm(&want, &mat.mapping),
        git_norm(&git),
        "git agrees"
    );

    // After the append: old lines → c1, the append → c2.
    let want2 = vec![(1, 2, c1), (3, 3, c2)];
    assert_eq!(blame_ok(&engine, c2, "y.txt"), want2);
    assert_eq!(engine.blame_no_skip(c2, "y.txt").unwrap(), want2);
    let git2 =
        git_blame_first_parent(&mat.repo_path, &mat.mapping[2].1, "y.txt").expect("git blames");
    assert_eq!(to_git_norm(&want2, &mat.mapping), git_norm(&git2));

    // The renamed-away source no longer exists: both sides fail.
    assert!(matches!(
        engine.blame(c1, "x.txt"),
        Err(StoreError::NotFound)
    ));
    assert!(git_blame_first_parent(&mat.repo_path, &mat.mapping[1].1, "x.txt").is_err());
}

/// Deleted then re-added with identical content: the current content's
/// lineage restarts at the re-add (a delete is a hard break, like git).
#[test]
fn deleted_then_readded_blames_current_lineage() {
    let mut h = Hist::new();
    let c0 = h.commit_with(vec![], "root", |h| vec![h.add("f.txt", b"a\nb\n")]);
    let c1 = h.commit_with(vec![c0], "delete", |h| vec![h.delete("f.txt")]);
    let c2 = h.commit_with(vec![c1], "re-add same content", |h| {
        vec![h.add("f.txt", b"a\nb\n")]
    });
    let hh = h.finish();
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&hh, dir.path()).expect("materialize");
    let (_ids, engine, _) = setup(&hh, true);

    let want = vec![(1, 2, c2)];
    assert_eq!(blame_ok(&engine, c2, "f.txt"), want);
    assert_eq!(engine.blame_no_skip(c2, "f.txt").unwrap(), want);
    let git =
        git_blame_first_parent(&mat.repo_path, &mat.mapping[2].1, "f.txt").expect("git blames");
    assert_eq!(to_git_norm(&want, &mat.mapping), git_norm(&git));

    // At the delete changeset the path is gone: NotFound here, git error.
    assert!(matches!(
        engine.blame(c1, "f.txt"),
        Err(StoreError::NotFound)
    ));
    assert!(git_blame_first_parent(&mat.repo_path, &mat.mapping[1].1, "f.txt").is_err());
}

// ------------------------------------------------------- special paths

#[test]
fn empty_file_yields_no_ranges_and_git_agrees() {
    let mut h = Hist::new();
    let c0 = h.commit_with(vec![], "root", |h| {
        vec![h.add("e.txt", b""), h.add("f.txt", b"x\n")]
    });
    let hh = h.finish();
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&hh, dir.path()).expect("materialize");
    let (_ids, engine, _) = setup(&hh, true);
    assert_eq!(blame_ok(&engine, c0, "e.txt"), Vec::new());
    assert_eq!(engine.blame_no_skip(c0, "e.txt").unwrap(), Vec::new());
    let git = git_blame_first_parent(&mat.repo_path, &mat.mapping[0].1, "e.txt").expect("git runs");
    assert!(git.is_empty(), "git prints nothing for an empty file");
    // Sanity: a non-empty sibling still blames.
    assert_eq!(blame_ok(&engine, c0, "f.txt"), vec![(1, 1, c0)]);
}

#[test]
fn symlink_blames_its_target_line_like_git() {
    let mut h = Hist::new();
    let c0 = h.commit_with(vec![], "root", |h| {
        vec![h.add_mode("lnk", b"target-v1", Mode::Symlink)]
    });
    let c1 = h.commit_with(vec![c0], "retarget", |h| {
        vec![chg(
            &mut h.blobs,
            "lnk",
            Op::Modify,
            Some(b"target-v2"),
            None,
            Mode::Symlink,
        )]
    });
    let hh = h.finish();
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&hh, dir.path()).expect("materialize");
    let (_ids, engine, _) = setup(&hh, true);

    assert_eq!(blame_ok(&engine, c0, "lnk"), vec![(1, 1, c0)]);
    assert_eq!(blame_ok(&engine, c1, "lnk"), vec![(1, 1, c1)]);
    assert_eq!(engine.blame_no_skip(c1, "lnk").unwrap(), vec![(1, 1, c1)]);
    let git = git_blame_first_parent(&mat.repo_path, &mat.mapping[1].1, "lnk")
        .expect("git blames symlink");
    assert_eq!(
        git_norm(&git),
        vec![(1, 1, mat.mapping[1].1.clone())],
        "git attributes the retargeted symlink to c1"
    );
}

#[test]
fn gitlink_path_yields_documented_empty_answer() {
    let mut h = Hist::new();
    let ptr = h.register_gitlink(7);
    let changes = vec![
        h.add("f.txt", b"x\n"),
        Change {
            path: "sub".into(),
            op: Op::Add,
            blob: Some(ptr),
            src_path: None,
            mode: Mode::Gitlink,
        },
    ];
    let c0 = h.commit(vec![], changes, "root with submodule");
    let hh = h.finish();
    let (_ids, engine, _) = setup(&hh, true);
    assert_eq!(
        blame_ok(&engine, c0, "sub"),
        Vec::new(),
        "gitlink blame: documented empty answer (git errors instead)"
    );
    assert_eq!(blame_ok(&engine, c0, "f.txt"), vec![(1, 1, c0)]);
}

#[test]
fn directory_and_missing_paths_are_not_found() {
    let mut h = Hist::new();
    let c0 = h.commit_with(vec![], "root", |h| {
        vec![h.add("d/x.txt", b"1\n"), h.add("d/y.txt", b"2\n")]
    });
    let hh = h.finish();
    let (_ids, engine, _) = setup(&hh, true);
    assert!(matches!(engine.blame(c0, "d"), Err(StoreError::NotFound)));
    assert!(engine.blame(c0, "d/x.txt").is_ok());
    assert!(matches!(
        engine.blame(c0, "nope.txt"),
        Err(StoreError::NotFound)
    ));
    assert!(matches!(engine.blame(c0, ""), Err(StoreError::Invalid(_))));
}

// -------------------------------------------------- content edge cases

#[test]
fn crlf_lines_pair_like_git() {
    let mut h = Hist::new();
    let c0 = h.commit_with(vec![], "root", |h| vec![h.add("c.txt", b"a\r\nb\r\n")]);
    let c1 = h.commit_with(vec![c0], "insert B", |h| {
        vec![h.modify("c.txt", b"a\r\nB\r\nb\r\n")]
    });
    let hh = h.finish();
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&hh, dir.path()).expect("materialize");
    let (_ids, engine, _) = setup(&hh, true);

    let want = vec![(1, 1, c0), (2, 2, c1), (3, 3, c0)];
    assert_eq!(blame_ok(&engine, c1, "c.txt"), want);
    assert_eq!(engine.blame_no_skip(c1, "c.txt").unwrap(), want);
    let git =
        git_blame_first_parent(&mat.repo_path, &mat.mapping[1].1, "c.txt").expect("git blames");
    assert_eq!(to_git_norm(&want, &mat.mapping), git_norm(&git));
}

#[test]
fn missing_trailing_newline_blames_like_git() {
    let mut h = Hist::new();
    // The old file ends without a newline; the new file appends one — the
    // unterminated last record is a DIFFERENT record from the terminated
    // one (git semantics), so line 2 re-attributes.
    let c0 = h.commit_with(vec![], "root", |h| vec![h.add("f.txt", b"a\nb")]);
    let c1 = h.commit_with(vec![c0], "append c", |h| {
        vec![h.modify("f.txt", b"a\nb\nc")]
    });
    let hh = h.finish();
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&hh, dir.path()).expect("materialize");
    let (_ids, engine, _) = setup(&hh, true);

    let got = blame_ok(&engine, c1, "f.txt");
    assert_eq!(engine.blame_no_skip(c1, "f.txt").unwrap(), got);
    let git =
        git_blame_first_parent(&mat.repo_path, &mat.mapping[1].1, "f.txt").expect("git blames");
    assert_eq!(to_git_norm(&got, &mat.mapping), git_norm(&git));
}

#[test]
fn mode_only_change_inherits_all_lines_like_git() {
    let mut h = Hist::new();
    let content: &[u8] = b"#!/rows\nline1\nline2\n";
    let c0 = h.commit_with(vec![], "root", |h| vec![h.add("s.sh", content)]);
    // Mode-only modify: same blob id, mode flips Regular → Exec.
    let c1 = h.commit_with(vec![c0], "chmod +x", |h| {
        vec![chg(
            &mut h.blobs,
            "s.sh",
            Op::Modify,
            Some(content),
            None,
            Mode::Exec,
        )]
    });
    let hh = h.finish();
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&hh, dir.path()).expect("materialize");
    let (_ids, engine, _) = setup(&hh, true);

    let want = vec![(1, 3, c0)];
    assert_eq!(
        blame_ok(&engine, c1, "s.sh"),
        want,
        "mode-only change inherits 1:1"
    );
    assert_eq!(engine.blame_no_skip(c1, "s.sh").unwrap(), want);
    let git =
        git_blame_first_parent(&mat.repo_path, &mat.mapping[1].1, "s.sh").expect("git blames");
    assert_eq!(to_git_norm(&want, &mat.mapping), git_norm(&git));
}

// --------------------------------------- diff ambiguity (git-captured shapes)

/// The six ambiguity shapes, captured from `git blame --first-parent`
/// (git 2.43). The root adds `old`, the change commit rewrites to `new`;
/// the expectation lists which lines attribute to the root (0) vs the
/// change (1).
/// One captured ambiguity shape: (name, old bytes, new bytes, expected
/// attribution — `(line, line, owner)` triples, owner 0 = root, 1 = the
/// change commit).
type AmbiguityCase<'a> = (&'a str, &'a [u8], &'a [u8], &'a [(u64, u64, u8)]);

#[test]
fn diff_ambiguity_cases_match_git_and_captured_shapes() {
    let cases: &[AmbiguityCase] = &[
        // swap: old2↔new1 pair; the new "a" is the added line.
        ("swap", b"a\nb\n", b"b\na\n", &[(1, 1, 0), (2, 2, 1)]),
        // move-around: x slides up (old2↔new1, old3↔new3); "a" re-adds.
        (
            "move",
            b"a\nx\nb\n",
            b"x\na\nb\n",
            &[(1, 1, 0), (2, 2, 1), (3, 3, 0)],
        ),
        // shift: keeping a,b (edit script of 1 add + 1 del) is strictly
        // smaller than keeping c, so git adds the leading c and a,b stay
        // with the root. (git 2.43 live capture.)
        ("shift", b"a\nb\nc\n", b"c\na\nb\n", &[(1, 1, 1), (2, 3, 0)]),
        // dup: old1↔new1 (a), old3↔new2 (b); the second a is added.
        ("dup", b"a\na\nb\n", b"a\nb\na\n", &[(1, 2, 0), (3, 3, 1)]),
        (
            "insmid",
            b"a\nb\nc\nd\n",
            b"a\nX\nb\nc\nd\n",
            &[(1, 1, 0), (2, 2, 1), (3, 5, 0)],
        ),
        (
            "dupins",
            b"a\nb\n",
            b"a\na\nb\n",
            &[(1, 1, 0), (2, 2, 1), (3, 3, 0)],
        ),
    ];
    for (name, old, new, expect) in cases {
        let mut h = Hist::new();
        let c0 = h.commit_with(vec![], "root", |h| vec![h.add("f.txt", old)]);
        let c1 = h.commit_with(vec![c0], name, |h| vec![h.modify("f.txt", new)]);
        let hh = h.finish();
        let dir = tempfile::tempdir().expect("tempdir");
        let mat = materialize(&hh, dir.path()).expect("materialize");
        let (_ids, engine, _) = setup(&hh, true);

        let want: Vec<(u64, u64, Id)> = expect
            .iter()
            .map(|&(lo, hi, side)| (lo, hi, if side == 0 { c0 } else { c1 }))
            .collect();
        assert_eq!(
            blame_ok(&engine, c1, "f.txt"),
            want,
            "{name}: captured shape"
        );
        assert_eq!(
            engine.blame_no_skip(c1, "f.txt").unwrap(),
            want,
            "{name}: naive equal"
        );
        let git = git_blame_first_parent(&mat.repo_path, &mat.mapping[1].1, "f.txt")
            .unwrap_or_else(|e| panic!("{name}: git oracle: {e}"));
        assert_eq!(
            to_git_norm(&want, &mat.mapping),
            git_norm(&git),
            "{name}: git agrees"
        );
    }
}

// ------------------------------------------------------------- skipping

/// A long linear chain where the blamed path is touched only at the two
/// ends: skip-enabled and naive blame agree, and the answer is the hand-
/// computed one. The segment bloom of a mid-chain member proves the whole
/// untouched run in one entry read.
#[test]
fn long_sparse_chain_skip_equals_naive() {
    let mut h = Hist::new();
    let c0 = h.commit_with(vec![], "root", |h| vec![h.add("log.txt", b"seed line\n")]);
    let mut prev = c0;
    let mut ids = vec![c0];
    for i in 0..400u32 {
        let path = format!("filler{:03}.txt", i % 40);
        let content = format!("filler content {i}\n");
        let ch = h.modify(&path, content.as_bytes());
        prev = h.commit(vec![prev], vec![ch], &format!("fill {i}"));
        ids.push(prev);
    }
    let append = h.modify("log.txt", b"seed line\nappended\n");
    let last = h.commit(vec![prev], vec![append], "touch log");
    ids.push(last);
    let hh = h.finish();
    let (ids2, engine, _) = setup(&hh, true);
    assert_eq!(ids, ids2, "ingest order matches construction");

    let want = vec![(1, 1, c0), (2, 2, last)];
    assert_eq!(
        blame_ok(&engine, last, "log.txt"),
        want,
        "skip walk attributes correctly"
    );
    assert_eq!(
        engine.blame_no_skip(last, "log.txt").unwrap(),
        want,
        "naive agrees"
    );

    // The whole chain is one segment below the root head; the tip's bloom
    // names its own touch (log.txt), but the member just below the tip
    // covers (root, member] — no log.txt touch — and proves the skip.
    let tip = engine.pathsummary_entry(last).unwrap();
    assert!(
        tip.bloom_maybe_contains("log.txt"),
        "tip touched log.txt itself"
    );
    let below = engine.pathsummary_entry(ids[ids.len() - 2]).unwrap();
    assert!(!below.is_head);
    assert_eq!(below.head, Some(c0));
    assert!(
        below.head_dist > 100,
        "chain must be deep, got {}",
        below.head_dist
    );
    assert!(
        !below.bloom_maybe_contains("log.txt"),
        "log.txt must be provably absent from the mid-chain segment bloom"
    );
}

// ------------------------------------------------------- path summaries

#[test]
fn pathsummary_overflow_keeps_bloom_sound() {
    // A root changeset touching 300 distinct paths: the bounded list caps
    // at 256 (byte-ascending), the truncated flag is the "unbounded"
    // marker, and the bloom layer must still cover ALL 300 paths (the
    // soundness rule: absence from the bloom proves the segment clean).
    let mut h = Hist::new();
    let mut adds: Vec<Change> = Vec::new();
    for i in 0..300u32 {
        let path = format!("p{i:03}.txt");
        adds.push(h.add(&path, format!("content {i}\n").as_bytes()));
    }
    let c0 = h.commit(vec![], adds, "root with 300 files");
    // Second changeset modifies one file; its segment bloom covers just it.
    let c1 = h.commit_with(vec![c0], "touch one", |h| {
        vec![h.modify("p123.txt", b"changed\n")]
    });
    let hh = h.finish();
    let (_ids, engine, _) = setup(&hh, true);

    let e0 = engine.pathsummary_entry(c0).unwrap();
    assert!(e0.truncated, "300 touched paths must mark truncation");
    assert_eq!(e0.paths.len(), MAX_SUMMARY_PATHS);
    // Byte-ascending prefix of the full sorted touched set.
    let mut all: Vec<String> = (0..300).map(|i| format!("p{i:03}.txt")).collect();
    all.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(&e0.paths[..], &all[..MAX_SUMMARY_PATHS]);
    assert!(e0.is_head, "root is a head");
    assert!(
        e0.bloom.iter().all(|&b| b == 0),
        "heads carry an all-zero bloom"
    );
    // A truncated head's list cannot prove absence, so blame must (and
    // does) read the changeset instead — exercised by blaming a path
    // beyond the cap:
    assert_eq!(blame_ok(&engine, c0, "p299.txt"), vec![(1, 1, c0)]);
    assert_eq!(blame_ok(&engine, c0, "p000.txt"), vec![(1, 1, c0)]);

    let e1 = engine.pathsummary_entry(c1).unwrap();
    assert!(!e1.truncated);
    assert_eq!(e1.paths, vec!["p123.txt".to_string()]);
    assert_eq!(e1.head, Some(c0));
    assert_eq!(e1.head_dist, 1);
    assert!(!e1.bloom_maybe_contains("p999.txt"));
    assert!(e1.bloom_maybe_contains("p123.txt"));
}

#[test]
fn pathsummary_rebuild_bytes_identical_and_poison_resistant() {
    let opts = GenOptions {
        commits: 30,
        merge_ratio: 0.15,
        octopus_ratio: 0.05,
        rename_ratio: 0.4,
        delete_ratio: 0.1,
        max_files: 12,
        max_dirs: 3,
        big_blob_bytes: None,
        exec_ratio: 0.05,
        symlink_ratio: 0.05,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
    };
    let h = generate(4242, &opts);
    let (objects_a, metadata_a) = stores();
    let ids = ingest(&objects_a, &metadata_a, &h, true);
    let engine_a = Engine::new(objects_a.clone(), metadata_a.clone());
    let (objects_b, metadata_b) = stores();
    ingest(&objects_b, &metadata_b, &h, false);
    let engine_b = Engine::new(objects_b.clone(), metadata_b.clone());

    for (i, cs) in ids.iter().enumerate() {
        let bytes_a = metadata_a
            .derived_get(DERIVED_PATHSUMMARY, *cs)
            .expect("read A")
            .expect("eagerly written");
        let summary = engine_b
            .pathsummary_entry(*cs)
            .expect("B computes on demand");
        let bytes_b = metadata_b
            .derived_get(DERIVED_PATHSUMMARY, *cs)
            .expect("read B")
            .expect("persisted after compute");
        assert_eq!(
            bytes_a, bytes_b,
            "recomputed entry must be byte-identical at cs {i}"
        );
        // Structural invariants of the decoded summary.
        assert_eq!(summary.bloom.len(), BLOOM_BITS / 8);
        if summary.is_head {
            assert_eq!(summary.head, None);
            assert_eq!(summary.head_dist, 0);
            assert!(summary.bloom.iter().all(|&b| b == 0));
        } else {
            assert!(summary.head.is_some());
            assert!(summary.head_dist >= 1);
            assert_eq!(
                summary.first_parent,
                h.changesets[i].parents.first().copied()
            );
        }
        // Paths deduped and ascending.
        let mut sorted = summary.paths.clone();
        sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        sorted.dedup();
        assert_eq!(summary.paths, sorted);
        assert!(summary.paths.len() <= MAX_SUMMARY_PATHS);
    }

    // Poisoning: a corrupt cache payload is a miss, never trusted — the
    // entry recomputes to the correct value.
    let victim = ids[3];
    metadata_b
        .derived_put(
            DERIVED_PATHSUMMARY,
            victim,
            b"\xff\xfe garbage not a payload",
        )
        .expect("poison");
    let rep = engine_b
        .pathsummary_entry(victim)
        .expect("recompute after poisoning");
    let bytes_a = metadata_a
        .derived_get(DERIVED_PATHSUMMARY, victim)
        .expect("read A")
        .expect("present");
    let bytes_c = metadata_b
        .derived_get(DERIVED_PATHSUMMARY, victim)
        .expect("read B")
        .expect("rewritten");
    assert_eq!(
        bytes_a, bytes_c,
        "poisoned entry must be replaced by the true bytes"
    );
    assert_eq!(rep, engine_a.pathsummary_entry(victim).expect("A entry"));
}

/// A rename source colliding with another change's destination path must
/// appear exactly once in the path summary (the summary's `paths` list is
/// the deduplicated dst ∪ src set, ascending by bytes). Regression for the
/// O(n²) containment scan, which this shape exercised on every insert.
#[test]
fn pathsummary_dedups_rename_src_against_dest() {
    let mut h = Hist::new();
    // Root: a.txt and b.txt.
    let c0 = h.commit_with(Vec::new(), "root", |h| {
        vec![h.add("a.txt", b"aaa\n"), h.add("b.txt", b"bbb\n")]
    });
    // Rename a.txt→b.txt's neighbor plus modify b.txt: `b.txt` is both a
    // rename destination and a plain change path; `a.txt` is a source.
    // Also rename b.txt→c.txt so `b.txt` is simultaneously a source for
    // one change and a destination-free path for another.
    let c1 = h.commit_with(vec![c0], "collide", |h| {
        vec![
            h.rename("a.txt", "c.txt", b"aaa\n"),
            h.modify("a.txt", b"aaa2\n"),
        ]
    });
    let h: GeneratedHistory = {
        // Reuse the ingest path: build a GeneratedHistory from the Hist.
        let cs = h.cs.clone();
        let blobs = h.blobs.clone();
        let head = c1;

        GeneratedHistory {
            changesets: cs,
            blob_contents: {
                let mut v = blobs;
                v.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                v
            },
            head,
        }
    };
    let (ids, engine, _meta) = setup(&h, true);
    let e1 = engine.pathsummary_entry(ids[1]).unwrap();
    // a.txt (src + modified dest), c.txt (rename dest): each exactly once.
    assert_eq!(e1.paths, vec!["a.txt".to_string(), "c.txt".to_string()]);
    assert!(!e1.truncated, "two paths cannot overflow the cap");
}
