//! DOGFOOD #30 regressions: the direct merge-base APIs must pick from
//! the MAXIMAL common ancestors, with DAG ancestry dominating
//! timestamps. The historical `merge_base_naive` ranked the raw common
//! set by committer seconds and could surface a dominated ancestor
//! first — a same-second commit, or a skewed clock stamping an old
//! commit newer than its own descendant — mis-anchoring the 3-way
//! walk into spurious conflicts on disjoint-line edits.
//!
//! Fixtures are hand-built DAGs (no sleeps, no generator): every API
//! surface is exercised on each shape —
//! [`Engine::merge_base`] (the accelerated path `Repo::merge`,
//! `MergeBase` RPC, and rebase use), [`Engine::merge_base_naive`]
//! (the semantic reference), and [`Engine::merge_bases_all`] (the
//! recursive merge's base set) — plus the merge OUTCOME: the merge of
//! disjoint-line edits must be clean, not a conflict born of a wrong
//! base.

use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore};

fn ident(tag: &str, secs: u64) -> Identity {
    Identity {
        name: "BasePick".into(),
        email: format!("{tag}@gardenbed.example"),
        time_secs: 1_700_000_000 + secs,
        tz_min: 0,
    }
}

/// A hand-built DAG: puts changesets with EXPLICIT committer seconds
/// (the clock under test) and returns their ids in insertion order.
struct Dag {
    objects: Arc<MemoryStore>,
    metadata: Arc<MemoryStore>,
    engine: Engine,
    ids: Vec<Id>,
}

impl Dag {
    fn new() -> Self {
        let objects = Arc::new(MemoryStore::new());
        let metadata = Arc::new(MemoryStore::new());
        let engine = Engine::new(
            objects.clone() as Arc<dyn ObjectStore>,
            metadata.clone() as Arc<dyn MetadataStore>,
        );
        Dag {
            objects,
            metadata,
            engine,
            ids: Vec::new(),
        }
    }

    /// One changeset with `parents` and a snapshot of `files` at the
    /// given committer seconds. Returns its id.
    fn commit(
        &mut self,
        parents: Vec<Id>,
        tag: &str,
        secs: u64,
        files: &[(&str, &[u8])],
    ) -> Id {
        let mut changes: Vec<Change> = Vec::new();
        // Snapshot semantics vs the first parent: delete what vanished,
        // write what is there.
        if let Some(&p) = parents.first() {
            let prev = self.engine.flatten_tree(self.engine.derive_tree(p).unwrap().unwrap());
            for (ppath, _pmode, _pblob) in prev {
                if !files.iter().any(|(n, _)| *n == ppath) {
                    changes.push(Change {
                        path: ppath,
                        op: Op::Delete,
                        blob: None,
                        src_path: None,
                        mode: Mode::Regular,
                    });
                }
            }
        }
        for (path, content) in files {
            let blob = blob_id(content);
            self.objects.put_blob(content).unwrap();
            let existed = parents
                .first()
                .and_then(|&p| {
                    let tree = self.engine.derive_tree(p).unwrap();
                    self.engine
                        .flatten_tree(tree)
                        .unwrap()
                        .into_iter()
                        .find(|(fp, _m, _b)| fp == *path)
                })
                .is_some();
            changes.push(Change {
                path: (*path).to_owned(),
                op: if existed { Op::Modify } else { Op::Add },
                blob: Some(blob),
                src_path: None,
                mode: Mode::Regular,
            });
        }
        changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        let cs = Changeset {
            parents,
            changes,
            message: format!("dag {tag}"),
            author: ident(tag, secs),
            committer: ident(tag, secs),
            signature: None,
        };
        let id = self.metadata.put_changeset(&cs).unwrap();
        self.engine.on_changeset_created(id).unwrap();
        self.ids.push(id);
        id
    }
}

/// The classic same-second shape: a criss-cross built in ONE
/// committer second, where the maximal common ancestors (`dom` and
/// `right`) are tied with the dominated ones (`left`, `root`) — a raw
/// newest-first order over the whole common set lets id-byte order
/// decide, and can surface a DOMINATED ancestor first.
///
/// ```text
///        root                    (t=0)
///       /    \
///    left    right              (t=1, same second)
///     |  \    /                 |
///     |   \  /                  |
///    dom   cross1   cross2      cross1 parents (dom, right);
///     |                \        cross2 parents (right, dom);
///     |                 |       all five commits share t=1.
///   ours (t=2)      theirs (t=2, child of cross2)
/// ```
///
/// Common ancestors of ours/theirs: {root, left, right, dom}
/// (cross1/cross2 are each on only one side). `root` and `left` are
/// dominated (`left` rides inside `dom`, which is on both sides);
/// the maximal set is {dom, right}. A raw (secs desc, id desc) sort
/// over the whole common set can pick `left` first — the DOGFOOD #30
/// bug: `left` still carries LEFT's top edit, so a 3-way walk anchored
/// there mis-compares both tips' top lines and spuriously conflicts.
fn same_second_dag() -> Dag {
    let mut d = Dag::new();
    let base_text: Vec<u8> = (1..=12)
        .flat_map(|i| format!("base line {i}\n").into_bytes())
        .collect::<Vec<u8>>();
    let root = d.commit(vec![], "root", 0, &[("f", &base_text)]);

    let mut left_text = base_text.clone();
    left_text.splice(0..9, b"LEFT edits the top\n".iter().copied());
    let left = d.commit(vec![root], "left", 1, &[("f", &left_text)]);

    let mut right_text = base_text.clone();
    let cut = right_text.len() - b"base line 12\n".len();
    right_text.splice(cut.., b"RIGHT edits the bottom\n".iter().copied());
    let right = d.commit(vec![root], "right", 1, &[("f", &right_text)]);

    // dom: a child of left in the SAME second as everything else.
    let mut dom_text = base_text.clone();
    dom_text.splice(0..9, b"DOM also edits the top\n".iter().copied());
    let dom = d.commit(vec![left], "dom", 1, &[("f", &dom_text)]);

    // The cross merges (t=1): both carry dom (dominated by each) and
    // right. Their content is the union of dom's top edit and right's
    // bottom edit — disjoint regions, merged cleanly by construction.
    let union = {
        let mut v = dom_text.clone();
        let cut = v.len() - b"base line 12\n".len();
        v.splice(cut.., b"RIGHT edits the bottom\n".iter().copied());
        v
    };
    let cross1 = d.commit(vec![dom, right], "cross1", 1, &[("f", &union)]);
    let cross2 = d.commit(vec![right, dom], "cross2", 1, &[("f", &union)]);

    // ours descends from cross1, theirs from cross2; both edit
    // DISJOINT regions of the union — a clean merge when the base set
    // is the maximal pair {dom, right} (the recursive fold) or either
    // maximal single base.
    let mut ours_text = union.clone();
    ours_text.splice(0..9, b"OURS re-edits the top\n".iter().copied());
    let ours = d.commit(vec![cross1], "ours", 2, &[("f", &ours_text)]);

    let mut theirs_text = union.clone();
    let cut = theirs_text.len() - b"RIGHT edits the bottom\n".len();
    theirs_text.splice(cut.., b"THEIRS bottom\n".iter().copied());
    let theirs = d.commit(vec![cross2], "theirs", 2, &[("f", &theirs_text)]);
    let _ = (root, ours);
    d
}

/// The skewed-clock shape: a DOMINATED common ancestor is stamped
/// NEWER than its own descendant (the true base), so the raw
/// newest-first order picks the dominated one; DAG ancestry must
/// dominate timestamps.
///
/// ```text
///   root (t=0)
///    |
///   mid (t=99, SKEWED clock — stamped far in the future)
///    |
///   base (t=1, clock recovered)   ← unique maximal common ancestor
///   /   \
/// ours  theirs                    (t=2)
/// ```
///
/// Common ancestors of ours/theirs: {root, mid, base}; `root` and
/// `mid` are dominated by `base`. Raw order: mid (t=99) sorts before
/// base (t=1) → picks `mid`, the DOMINATED one — the bug.
fn skewed_clock_dag() -> Dag {
    let mut d = Dag::new();
    let base_text: Vec<u8> = (1..=12)
        .flat_map(|i| format!("base line {i}\n").into_bytes())
        .collect::<Vec<u8>>();
    let root = d.commit(vec![], "root", 0, &[("f", &base_text)]);
    // mid: skewed far-future stamp (unchanged content — a pure
    // topology node whose only job is the timestamp).
    let mid = d.commit(vec![root], "mid", 99, &[("f", &base_text)]);
    // base: the clock recovers; base is mid's CHILD (descendant).
    let mut base_text2 = base_text.clone();
    base_text2.splice(0..9, b"BASE edit\n".iter().copied());
    let base = d.commit(vec![mid], "base", 1, &[("f", &base_text2)]);

    let mut ours_text = base_text2.clone();
    ours_text.splice(0..9, b"OURS top\n".iter().copied());
    let ours = d.commit(vec![base], "ours", 2, &[("f", &ours_text)]);

    let mut theirs_text = base_text2.clone();
    let cut = theirs_text.len() - b"base line 12\n".len();
    theirs_text.splice(cut.., b"THEIRS bottom\n".iter().copied());
    let theirs = d.commit(vec![base], "theirs", 2, &[("f", &theirs_text)]);
    let _ = (root, mid, ours, theirs);
    d
}

#[test]
fn merge_base_picks_maximal_on_same_second_dag() {
    let d = same_second_dag();
    // ids: [root, left, right, dom, cross1, cross2, ours, theirs]
    let (root, ours, theirs) = (d.ids[0], d.ids[6], d.ids[7]);

    // Both direct APIs must pick a MAXIMAL common ancestor — `dom` or
    // `right` — never the dominated `left` (or `root`), even though all
    // four share the same committer second and id-byte order could
    // surface `left` first.
    let (dom, left) = (d.ids[3], d.ids[1]);
    for (name, base) in [
        ("merge_base", d.engine.merge_base(ours, theirs).unwrap()),
        (
            "merge_base_naive",
            d.engine.merge_base_naive(ours, theirs).unwrap(),
        ),
    ] {
        let b = base.unwrap_or_else(|| panic!("{name} finds no base"));
        assert!(
            b == dom || b == d.ids[2],
            "{name} picked {b}, a non-maximal base (left={left} is \
             dominated despite the same second; root={root}; maximal = \
             dom={dom} / right={})",
            d.ids[2]
        );
    }
    let all = d.engine.merge_bases_all(ours, theirs).unwrap();
    assert_eq!(
        all.len(),
        2,
        "the criss-cross has exactly two maximal bases: {all:?}"
    );
    assert!(all.contains(&dom) && all.contains(&d.ids[2]));
}

#[test]
fn merge_base_picks_maximal_on_skewed_clock_dag() {
    let d = skewed_clock_dag();
    // ids: [root, mid, base, ours, theirs]
    let (root, mid, base, ours, theirs) =
        (d.ids[0], d.ids[1], d.ids[2], d.ids[3], d.ids[4]);

    // The unique maximal common ancestor is `base`; the skewed `mid`
    // (t=99) is dominated and MUST NOT be picked despite its stamp.
    for (name, got) in [
        ("merge_base", d.engine.merge_base(ours, theirs).unwrap()),
        (
            "merge_base_naive",
            d.engine.merge_base_naive(ours, theirs).unwrap(),
        ),
    ] {
        assert_eq!(
            got,
            Some(base),
            "{name} picked {got:?} — DAG ancestry must dominate the \
             skewed clock (mid={mid} is stamped t=99 but dominated; \
             root={root})"
        );
    }
    assert_eq!(
        d.engine.merge_bases_all(ours, theirs).unwrap(),
        vec![base],
        "unique maximal base"
    );
}

#[test]
fn same_second_merge_outcome_is_clean_recursive_and_single() {
    // The merge-level consequence: disjoint-line edits over the
    // criss-cross must merge CLEANLY through the recursive path (which
    // folds both cross merges) — and a single-base merge over EITHER
    // maximal cross tip is also clean. A non-maximal base (`dom`,
    // which still carries LEFT's top edit) would mis-anchor the walk
    // and can spuriously conflict.
    let d = same_second_dag();
    let (ours, theirs) = (d.ids[6], d.ids[7]);

    let recursive = d.engine.merge_tree_changes_recursive(ours, theirs).unwrap();
    assert!(
        recursive.is_ok(),
        "disjoint-line edits merge cleanly through the recursive path, got {recursive:?}"
    );

    // And the single-base merge over EITHER maximal base (dom, right)
    // is clean — this is what rebase/MergeBase consumers see when they
    // ask for one base.
    for maximal in [d.ids[3], d.ids[2]] {
        let single = d
            .engine
            .merge_tree_changes(Some(maximal), ours, theirs)
            .unwrap();
        assert!(
            single.is_ok(),
            "single-base merge over maximal {maximal} is clean, got {single:?}"
        );
    }
    // The dominated `left` mis-anchors the walk — the raw-order pick
    // would conflict here (the observable DOGFOOD #30 consequence):
    // left carries LEFT's top edit; ours re-edited the top; theirs
    // changed the bottom — a base that is not maximal makes BOTH
    // sides look changed at the top line.
    let left = d.ids[1];
    let over_dominated = d
        .engine
        .merge_tree_changes(Some(left), ours, theirs)
        .unwrap();
    assert!(
        over_dominated.is_err(),
        "merging over the dominated same-second base conflicts (the \
         raw-order pick's mis-merge), got {over_dominated:?}"
    );
}

#[test]
fn skewed_clock_merge_outcome_is_clean() {
    let d = skewed_clock_dag();
    let (base, ours, theirs) = (d.ids[2], d.ids[3], d.ids[4]);
    let merged = d.engine.merge_tree_changes(Some(base), ours, theirs).unwrap();
    assert!(
        merged.is_ok(),
        "disjoint-line edits over the true maximal base merge cleanly, got {merged:?}"
    );
    // Through merge_base itself (the accelerated path rebase uses).
    let b = d.engine.merge_base(ours, theirs).unwrap().unwrap();
    let via_api = d.engine.merge_tree_changes(Some(b), ours, theirs).unwrap();
    assert!(via_api.is_ok(), "merge over Engine::merge_base's pick is clean");
}

#[test]
fn merge_base_skewed_dag_matches_git() {
    // Oracle parity for the skewed shape: git merge-base on the same
    // DAG names the maximal ancestor, not the skewed dominated one.
    let d = skewed_clock_dag();
    let (base, ours, theirs) = (d.ids[2], d.ids[3], d.ids[4]);
    // Build the same DAG in a real git repo and ask git.
    let dir = tempfile::tempdir().expect("git tempdir");
    let g = |args: &[&str]| {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("LANG", "C");
        let out = cmd.args(args).output().expect("git runs");
        assert!(
            out.status.success(),
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        out
    };
    g(&["init", "-q", "-b", "main", "."]);
    g(&["config", "user.email", "t@g.example"]);
    g(&["config", "user.name", "T"]);
    let base_content: Vec<u8> = (1..=12)
        .flat_map(|i| format!("base line {i}\n").into_bytes())
        .collect();
    let commit = |msg: &str, env_secs: &str, content: &[u8]| -> String {
        std::fs::write(dir.path().join("f"), content).unwrap();
        let out = std::process::Command::new("git")
            .current_dir(dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@g.example")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@g.example")
            .env("GIT_COMMITTER_DATE", &format!("{env_secs} +0000"))
            .env("GIT_AUTHOR_DATE", &format!("{env_secs} +0000"))
            .args(["commit", "-q", "--allow-empty", "-am", msg])
            .output()
            .expect("commit runs");
        assert!(out.status.success(), "git commit failed");
        String::from_utf8(
            std::process::Command::new("git")
                .current_dir(dir.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse runs")
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned()
    };
    let _root_sha = commit("root", "1700000000", &base_content);
    // mid: skewed far-future stamp (a child of root).
    let mid_content = base_content.clone();
    let _mid_sha = commit("mid", "1700000099", &mid_content);
    // base: clock recovered, mid's CHILD.
    let mut base_content2 = base_content.clone();
    base_content2.splice(0..9, b"BASE edit\n".iter().copied());
    let base_sha = commit("base", "1700000001", &base_content2);
    // ours / theirs: disjoint edits off base.
    let mut ours_text = base_content2.clone();
    ours_text.splice(0..9, b"OURS top\n".iter().copied());
    let ours_sha = commit("ours", "1700000002", &ours_text);
    let mut theirs_text = base_content2.clone();
    let cut = theirs_text.len() - b"base line 12\n".len();
    theirs_text.splice(cut.., b"THEIRS bottom\n".iter().copied());
    let theirs_sha = commit("theirs", "1700000002", &theirs_text);

    let git_base = String::from_utf8(
        g(&["merge-base", &ours_sha, &theirs_sha]).stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    assert_eq!(
        git_base, base_sha,
        "git itself picks the maximal base, not the skewed mid"
    );
    // And Gardenbed agrees with git on the same DAG.
    assert_eq!(
        d.engine.merge_base(ours, theirs).unwrap(),
        Some(base),
        "Gardenbed's pick equals git's on the skewed DAG"
    );
}
