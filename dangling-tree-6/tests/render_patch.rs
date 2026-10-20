//! Integration tests for bed's git-shaped patch renderer
//! ([`gardenbed::render::render_diff`]).
//!
//! The unit tests in `render.rs` pin each entry class alone; these pin the
//! combined output: a diff mixing text, binary, and gitlink entries must
//! come out as one patch `git apply` parses entry-by-entry — binary blobs
//! render git's `Binary files … differ` line (never lossy U+FFFD hunks),
//! gitlink entries render mode lines only (the import's gitlink blob ids
//! are synthetic padded ids, so no honest `Subproject commit` line exists),
//! and a payload-free entry without mode lines still carries the
//! `---`/`+++` markers git apply needs to consume it without leaking its
//! name into the next entry's parse.

use gardenbed::render::{DiffEntry, DiffOp, DiffSide, render_diff};
use gardenbed_model::{Id, Mode, blob_id};

/// A gitlink blob id as the import stores it: the upstream git SHA-1
/// zero-padded to 32 bytes — a synthetic id for content bed never holds.
fn gitlink_id(byte: u8) -> Id {
    let mut padded = [0u8; 32];
    padded[12..].copy_from_slice(&[byte; 20]);
    blob_id(&padded)
}

fn entry(path: &str, op: DiffOp, old: Option<(Id, Mode)>, new: Option<(Id, Mode)>) -> DiffEntry {
    DiffEntry {
        path: path.to_owned(),
        op,
        old: old.map(|(blob, mode)| DiffSide { blob, mode }),
        new: new.map(|(blob, mode)| DiffSide { blob, mode }),
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
        // Gitlink add: the `new file mode 160000` line alone — the metadata
        // carries the header, so no markers and no content of any kind.
        entry(
            "vendor/lib",
            DiffOp::Add,
            None,
            Some((gitlink_id(0x19), Mode::Gitlink)),
        ),
        // Gitlink pointer change: no mode lines exist (both sides are
        // 160000) and no honest Subproject sha — markers plus git's
        // binary-differ line keep the entry one git apply parses.
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
diff --git a/vendor/other b/vendor/other
--- a/vendor/other
+++ b/vendor/other
Binary files a/vendor/other and b/vendor/other differ
diff --git a/script/build.sh b/script/build.sh
old mode 100644
new mode 100755
--- a/script/build.sh
+++ b/script/build.sh
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
