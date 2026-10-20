#!/usr/bin/env python3
"""Cross-language verification of the canonical encoding spec (M0 #4).

Reads every fixture under crates/model/tests/fixtures/, decodes it with the
independent Python implementation (gbcodec.py, written from the spec doc
alone), re-encodes it, and recomputes the SHA-256 Id. Any disagreement with
the Rust-blessed fixtures is a spec or implementation bug.

Usage: python3 tools/py-canonical/verify_fixtures.py [fixtures-dir]
"""

import hashlib
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import gbcodec  # noqa: E402


def main(fixtures_dir: str) -> int:
    fixtures = pathlib.Path(fixtures_dir)
    if not fixtures.is_dir():
        print(f"fixtures dir not found: {fixtures} (run `cargo test -p gardenbed-model` first)")
        return 2

    checked = 0
    failed = 0
    for hexfile in sorted(fixtures.glob("*.hex")):
        name = hexfile.stem
        raw = bytes.fromhex(hexfile.read_text().strip())
        is_tree = "tree" in name

        if name.startswith("invalid-"):
            try:
                if is_tree:
                    gbcodec.decode_tree(raw)
                else:
                    gbcodec.decode_changeset(raw)
                print(f"FAIL {name}: Python decoder accepted bytes it must reject")
                failed += 1
            except gbcodec.CodecError:
                checked += 1
            continue

        try:
            obj = gbcodec.decode_tree(raw) if is_tree else gbcodec.decode_changeset(raw)
        except gbcodec.CodecError as e:
            print(f"FAIL {name}: Python decoder rejected valid bytes: {e}")
            failed += 1
            continue

        re = gbcodec.encode(obj)
        if re != raw:
            print(f"FAIL {name}: Python re-encode differs from fixture bytes")
            failed += 1
            continue

        idfile = hexfile.with_suffix(".id")
        want = idfile.read_text().strip()
        got = hashlib.sha256(raw).hexdigest()
        if got != want:
            print(f"FAIL {name}: id mismatch (python {got} vs rust {want})")
            failed += 1
            continue

        checked += 1

    print(f"python cross-check: {checked} fixtures verified, {failed} failures")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "crates/model/tests/fixtures"))
