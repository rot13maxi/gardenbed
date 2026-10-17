//! Integration tests for working copy's git-shaped patch renderer
//! ([`gardenbed::render::render_diff`]).
//!
//! The unit tests in `render.rs` pin each entry class alone; these pin the
//! combined output: a diff mixing text, binary, gitlink, and rename
//! entries must come out as one patch `git apply` parses entry-by-entry —
//! binary blobs render git's `Binary files … differ` line (never lossy
//! U+FFFD hunks), gitlink entries render git's `Subproject commit` hunks
//! from the SHA zero-padded in the blob id, pure renames render git's
//! `R100` form, and a payload-free entry without mode lines still carries
//! the `---`/`+++` markers git apply needs to consume it without leaking
//! its name into the next entry's parse.
//!
//! Every shape here is additionally proven against real `git apply` in
//! [`git_apply_accepts_every_rendered_shape`].
use std::process::Command;

use gardenbed::render::{DiffEntry, DiffOp, DiffSide, render_diff};
use gardenbed_model::{Id, Mode, blob_id};

/// A gitlink blob id as the import/generator store it: the upstream git
/// SHA-1 zero-padded to 32 bytes — the id *is* the padding, never a hash.
fn gitlink_id(byte: u8) -> Id {
    let mut padded = [0u8; 32];
    padded[12..].copy_from_slice(&[byte; 20]);
    Id::from_bytes(padded)
}

fn entry(path: &str, op: DiffOp, old: Option<(Id, Mode)>, new: Option<(Id, Mode)>) -> DiffEntry {
    DiffEntry {
        path: path.to_owned(),
        op,
        old: old.map(|(blob, mode)| DiffSide { blob, mode }),
        new: new.map(|(blob, mode)| DiffSide { blob, mode }),
        src_path: None,
    }
}

fn rename_entry(
    dst: &str,
    src: &str,
    old: Option<(Id, Mode)>,
    new: Option<(Id, Mode)>,
) -> DiffEntry {
    DiffEntry {
        path: dst.to_owned(),
        op: DiffOp::Rename,
        old: old.map(|(blob, mode)| DiffSide { blob, mode }),
        new: new.map(|(blob, mode)| DiffSide { blob, mode }),
        src_path: Some(src.to_owned()),
    }
}

#[test]
fn mixed_text_binary_gitlink_patch_is_git_apply_shaped() {
    // The blob store holds every *real* blob the diff touches — and none
    // of the gitlink ids: if the renderer tried to fetch a gitlink's
    // content (they are synthetic, never cached), the test panics.
    let text_old = blob_id(b"one\ntwo\n");
    let text_new = blob_id(b"one\nTWO\n");
    let script = blob_id(b"#!/bin/sh\nset -e\n");
    let icns = blob_id(b"icns\0\0\x01\x02payload");
    let png = blob_id(b"\x89PNG\r\n\x1a\n\0rest");
    let store = [
        (text_old, b"one\ntwo\n".to_vec()),
        (text_new, b"one\nTWO\n".to_vec()),
        (script, b"#!/bin/sh\nset -e\n".to_vec()),
        (icns, b"icns\0\0\x01\x02payload".to_vec()),
        (png, b"\x89PNG\r\n\x1a\n\0rest".to_vec()),
    ];
    let content_of = move |id: Id| match store.iter().find(|(k, _)| *k == id) {
        Some((_, bytes)) => Ok(bytes.clone()),
        None => panic!("renderer fetched a blob the store lacks: {id}"),
    };

    let entries = [
        // Plain text modify: header, markers, one unified hunk.
        entry(
            "src/app.c",
            DiffOp::Modify,
            Some((text_old, Mode::Regular)),
            Some((text_new, Mode::Regular)),
        ),
        // Binary delete: git's `/dev/null` for the gone side.
        entry(
            "art/logo.icns",
            DiffOp::Delete,
            Some((icns, Mode::Exec)),
            None,
        ),
        // Binary add: `/dev/null` on the old side.
        entry("art/new.png", DiffOp::Add, None, Some((png, Mode::Regular))),
        // Gitlink add: git's own gitlink-add form minus the `index` line
        // (git's abbreviation length is dynamic; the Subproject hunk
        // carries the full SHA).
        entry(
            "vendor/lib",
            DiffOp::Add,
            None,
            Some((gitlink_id(0x19), Mode::Gitlink)),
        ),
        // Gitlink pointer change: git's one-line swap hunk.
        entry(
            "vendor/other",
            DiffOp::Modify,
            Some((gitlink_id(0x2a), Mode::Gitlink)),
            Some((gitlink_id(0x5a), Mode::Gitlink)),
        ),
        // Mode-only modify (identical blob, mode flip): mode lines plus the
        // markers, no hunks — byte-identical to the pre-fix form.
        entry(
            "script/build.sh",
            DiffOp::Modify,
            Some((script, Mode::Regular)),
            Some((script, Mode::Exec)),
        ),
        // Pure rename: git's R100 form, content never fetched.
        rename_entry(
            "docs/moved.md",
            "docs/old.md",
            Some((text_old, Mode::Regular)),
            Some((text_old, Mode::Regular)),
        ),
    ];

    let out = render_diff(&entries, &content_of).unwrap();
    assert_eq!(
        out,
        "\
diff --git a/src/app.c b/src/app.c
--- a/src/app.c
+++ b/src/app.c
@@ -1,2 +1,2 @@
 one
-two
+TWO
diff --git a/art/logo.icns b/art/logo.icns
deleted file mode 100755
Binary files a/art/logo.icns and /dev/null differ
diff --git a/art/new.png b/art/new.png
new file mode 100644
Binary files /dev/null and b/art/new.png differ
diff --git a/vendor/lib b/vendor/lib
new file mode 160000
--- /dev/null
+++ b/vendor/lib
@@ -0,0 +1 @@
+Subproject commit 1919191919191919191919191919191919191919
diff --git a/vendor/other b/vendor/other
--- a/vendor/other
+++ b/vendor/other
@@ -1 +1 @@
-Subproject commit 2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a
+Subproject commit 5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a
diff --git a/script/build.sh b/script/build.sh
old mode 100644
new mode 100755
--- a/script/build.sh
+++ b/script/build.sh
diff --git a/docs/old.md b/docs/moved.md
similarity index 100%
rename from docs/old.md
rename to docs/moved.md
"
    );
}

#[test]
fn binary_and_gitlink_output_never_transcodes_bytes() {
    // The original defect: raw bytes pushed through String::from_utf8_lossy
    // litter the patch with U+FFFD and can split into invalid hunks. Every
    // non-text payload must now ride the single ASCII differ line instead.
    let bin_old = blob_id(b"\xff\xfe\0bin OLD");
    let bin_new = blob_id(b"\xff\xfe\0bin NEW");
    let store = [
        (bin_old, b"\xff\xfe\0bin OLD".to_vec()),
        (bin_new, b"\xff\xfe\0bin NEW".to_vec()),
    ];
    let content_of = move |id: Id| match store.iter().find(|(k, _)| *k == id) {
        Some((_, bytes)) => Ok(bytes.clone()),
        None => panic!("renderer fetched a blob the store lacks: {id}"),
    };

    let out = render_diff(
        &[entry(
            "img/icon.bin",
            DiffOp::Modify,
            Some((bin_old, Mode::Regular)),
            Some((bin_new, Mode::Regular)),
        )],
        &content_of,
    )
    .unwrap();
    assert_eq!(
        out,
        "diff --git a/img/icon.bin b/img/icon.bin\n\
         --- a/img/icon.bin\n\
         +++ b/img/icon.bin\n\
         Binary files a/img/icon.bin and b/img/icon.bin differ\n"
    );
    assert!(out.is_ascii(), "binary entries must not transcode: {out:?}");
    assert!(!out.contains('\u{FFFD}'), "lossy replacement in output");
}

/// `git apply --check` must accept every patch shape bed renders, and the
/// applied results must match what git's own diff of the same change
/// produces — the strongest acceptance oracle available outside the
/// fixture harness. Runs in a throwaway git repo under the cargo target
/// dir; skips (with a panic) when no `git` is on PATH.
#[test]
fn git_apply_accepts_every_rendered_shape() {
    use std::fs;
    use std::path::PathBuf;

    let git = env_var_git();
    let root = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("gardenbed-render-patch"));
    let dir = root.join("git-apply");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    run(git, &dir, &["init", "-q"]);
    run(git, &dir, &["config", "user.name", "T"]);
    run(git, &dir, &["config", "user.email", "t@t"]);
    run(git, &dir, &["config", "core.quotePath", "false"]);

    // ---- regular text add + rename + mode-flip rename + gitlink shapes.
    let moved_content = b"line one\nline two\n".to_vec();
    let script_content = b"#!/bin/sh\nset -e\n".to_vec();

    fs::write(dir.join("old.txt"), &moved_content).unwrap();
    fs::write(dir.join("other.sh"), &script_content).unwrap();
    run(git, &dir, &["add", "."]);
    run(git, &dir, &["commit", "-qm", "base"]);

    // Build the bed-rendered patch for: rename old.txt→new.txt (pure),
    // rename+mode-flip other.sh→run.sh, gitlink add vendor/sub.
    let text = blob_id(&moved_content);
    let script = blob_id(&script_content);
    let sub_add = gitlink_id(0x33);
    let content_of = move |id: Id| {
        if id == text {
            Ok(moved_content.clone())
        } else if id == script {
            Ok(script_content.clone())
        } else {
            panic!("renderer fetched a blob the store lacks: {id}")
        }
    };
    let entries = [
        rename_entry(
            "new.txt",
            "old.txt",
            Some((text, Mode::Regular)),
            Some((text, Mode::Regular)),
        ),
        rename_entry(
            "run.sh",
            "other.sh",
            Some((script, Mode::Regular)),
            Some((script, Mode::Exec)),
        ),
        entry(
            "vendor/sub",
            DiffOp::Add,
            None,
            Some((sub_add, Mode::Gitlink)),
        ),
    ];
    let patch = render_diff(&entries, &content_of).unwrap();
    fs::write(dir.join("bed.patch"), &patch).unwrap();

    // git apply must accept the patch; then the working tree must carry
    // the move (old names gone, new names present, right modes).
    run(git, &dir, &["apply", "--check", "bed.patch"]);
    run(git, &dir, &["apply", "bed.patch"]);
    assert!(dir.join("new.txt").is_file(), "rename applied: dst exists");
    assert!(!dir.join("old.txt").exists(), "rename applied: src gone");
    assert!(dir.join("run.sh").is_file());
    let mode = fs::symlink_metadata(dir.join("run.sh"))
        .unwrap()
        .permissions();
    use std::os::unix::fs::PermissionsExt as _;
    assert!(
        mode.mode() & 0o111 != 0,
        "rename+mode applied: {:o}",
        mode.mode() & 0o777
    );

    // The gitlink add: a Subproject hunk applies onto the index as a
    // 160000 entry (git apply refuses submodule adds on the worktree
    // path; --cached is git's own form's acceptance surface).
    run(git, &dir, &["apply", "--cached", "bed.patch"]);
    let ls = out_of(git, &dir, &["ls-files", "-s", "vendor/sub"]);
    assert_eq!(
        ls.trim(),
        format!("160000 {} 0\tvendor/sub", hex40(0x33)),
        "gitlink add applied to the index"
    );

    // ---- gitlink pointer change + delete (on a fresh base).
    fs::write(dir.join("base2.txt"), b"base\n").unwrap();
    run(git, &dir, &["add", "base2.txt"]);
    run(git, &dir, &["commit", "-qm", "base2"]);
    run(
        git,
        &dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendor/ptr", hex40(0x44)),
        ],
    );
    run(git, &dir, &["commit", "-qm", "pointer"]);

    let old_ptr = gitlink_id(0x44);
    let new_ptr = gitlink_id(0x55);
    let entries = [entry(
        "vendor/ptr",
        DiffOp::Modify,
        Some((old_ptr, Mode::Gitlink)),
        Some((new_ptr, Mode::Gitlink)),
    )];
    let patch = render_diff(&entries, &|_| Ok(vec![])).unwrap();
    fs::write(dir.join("ptr.patch"), &patch).unwrap();
    run(git, &dir, &["apply", "--check", "--cached", "ptr.patch"]);
    run(git, &dir, &["apply", "--cached", "ptr.patch"]);
    let ls = out_of(git, &dir, &["ls-files", "-s", "vendor/ptr"]);
    assert_eq!(
        ls.trim(),
        format!("160000 {} 0\tvendor/ptr", hex40(0x55)),
        "pointer change applied to the index"
    );

    let entries = [entry(
        "vendor/ptr",
        DiffOp::Delete,
        Some((new_ptr, Mode::Gitlink)),
        None,
    )];
    let patch = render_diff(&entries, &|_| Ok(vec![])).unwrap();
    fs::write(dir.join("del.patch"), &patch).unwrap();
    run(git, &dir, &["apply", "--check", "--cached", "del.patch"]);
    run(git, &dir, &["apply", "--cached", "del.patch"]);
    let ls = out_of(git, &dir, &["ls-files", "-s", "vendor/ptr"]);
    assert_eq!(ls.trim(), "", "gitlink delete applied to the index");
}

/// The pairing guards: a delete must not pair with a *rewrite of a
/// pre-existing destination* or with a *cross-kind write*, even when the
/// blob ids are identical — both render `R100` blocks git apply refuses
/// ("already exists in working directory" / "new mode does not match").
/// The guarded shapes stay delete + write — git's own `D`+`M` / `A`+`D`
/// shape — and apply cleanly. This is the retroactive-review regression
/// pair for the M19 pairing bug.
#[test]
fn guarded_corners_render_delete_plus_write_and_apply() {
    use std::fs;
    use std::path::PathBuf;

    let git = env_var_git();
    let root = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("gardenbed-render-patch"));
    let dir = root.join("git-apply-guarded");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    run(git, &dir, &["init", "-q"]);
    run(git, &dir, &["config", "user.name", "T"]);
    run(git, &dir, &["config", "user.email", "t@t"]);
    run(git, &dir, &["config", "core.quotePath", "false"]);

    // Base: src.txt and dst.txt both carry "shared bytes\n"; pre.txt is a
    // regular file whose content equals the link target of the corner
    // below.
    let shared = b"shared bytes\n".to_vec();
    let target = b"target-of-link\n".to_vec();
    fs::write(dir.join("src.txt"), &shared).unwrap();
    fs::write(dir.join("dst.txt"), b"dst original\n").unwrap();
    fs::write(dir.join("pre.txt"), &target).unwrap();
    run(git, &dir, &["add", "."]);
    run(git, &dir, &["commit", "-qm", "base"]);

    // The change under test, as diff entries *before* pairing: src.txt
    // deleted; dst.txt rewritten to the same bytes as src.txt had (an
    // existing path, so no pairing); pre.txt deleted; link created as a
    // symlink whose target bytes equal pre.txt's content (cross-kind, so
    // no pairing).
    let shared_id = blob_id(&shared);
    let target_id = blob_id(&target);
    let mut entries = vec![
        entry(
            "src.txt",
            DiffOp::Delete,
            Some((shared_id, Mode::Regular)),
            None,
        ),
        entry(
            "dst.txt",
            DiffOp::Modify,
            Some((blob_id(b"dst original\n"), Mode::Regular)),
            Some((shared_id, Mode::Regular)),
        ),
        entry(
            "pre.txt",
            DiffOp::Delete,
            Some((target_id, Mode::Regular)),
            None,
        ),
        entry("link", DiffOp::Add, None, Some((target_id, Mode::Symlink))),
    ];
    gardenbed::render::pair_renames(&mut entries);

    // The guards held: no rename was invented for either corner.
    assert!(
        entries.iter().all(|e| e.op != DiffOp::Rename),
        "guarded corners must not pair"
    );
    assert_eq!(entries.len(), 4, "no delete was consumed");

    let dst_original_id = blob_id(b"dst original\n");
    let store = [
        (shared_id, shared.clone()),
        (target_id, target.clone()),
        (dst_original_id, b"dst original\n".to_vec()),
    ];
    let content_of = move |id: Id| {
        store
            .iter()
            .find(|(k, _)| *k == id)
            .map(|(_, v)| v.clone())
            .ok_or(gardenbed::error::ClientError::BlobHashMismatch(id))
    };
    let patch = render_diff(&entries, &content_of).unwrap();
    assert!(
        !patch.contains("rename from"),
        "no R100 may appear:\n{patch}"
    );
    fs::write(root.join("guarded.patch"), &patch).unwrap();

    // git's own shape for the same change, from a real worktree edit —
    // the acceptance oracle: bed's patch must apply where git's does.
    // (dst.txt rewritten in place; src.txt and pre.txt removed; link
    // created.)
    fs::write(dir.join("dst.txt"), &shared).unwrap();
    fs::remove_file(dir.join("src.txt")).unwrap();
    fs::remove_file(dir.join("pre.txt")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("target-of-link\n", dir.join("link")).unwrap();
    let git_out = out_of(
        git,
        &dir,
        &["diff", "--src-prefix=a/", "--dst-prefix=b/", "HEAD"],
    );
    // Reset the worktree to the base so bed's patch applies to the same
    // starting state git's diff was computed against.
    run(git, &dir, &["checkout", "--", "."]);
    run(git, &dir, &["clean", "-fdq"]);
    run(git, &dir, &["apply", "--check", "../guarded.patch"]);
    run(git, &dir, &["apply", "../guarded.patch"]);
    assert_eq!(
        fs::read(dir.join("dst.txt")).unwrap(),
        shared,
        "the rewrite applied"
    );
    assert!(!dir.join("src.txt").exists(), "the delete applied");
    assert!(!dir.join("pre.txt").exists(), "the delete applied");

    // git itself prints D+M / D+A for this change — the letter shape the
    // guards preserve.
    assert!(
        git_out.contains("deleted file mode"),
        "git prints D: {git_out}"
    );
    assert!(
        !git_out.contains("rename from"),
        "git prints no R: {git_out}"
    );
}

fn hex40(byte: u8) -> String {
    let hex = format!("{byte:02x}");
    hex.repeat(20)
}

fn env_var_git() -> &'static str {
    // The fixture harness elsewhere pins `git` via its own discovery; a
    // plain `git` on PATH covers every environment these tests run in.
    "git"
}

fn run(git: &str, dir: &std::path::Path, args: &[&str]) {
    let out = Command::new(git)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn out_of(git: &str, dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(git)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
