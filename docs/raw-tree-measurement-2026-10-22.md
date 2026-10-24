# Raw-tree fidelity measurement — 2026-10-22

The final raw-tree candidate `b994428af4ef4c72` passes the matched workload
regression threshold: mean throughput is 97.25% of the control, above 80%.
Code review R3 passed separately. These are fresh limited imports, not a full
kernel-history import or full-kernel SHA roundtrip.

Control: `9be22f2f1c6e03dc`. Treatment: `b994428af4ef4c72`.
Both release binaries imported an independent bare copy of the pinned Linux
mirror at `0fcb05b72344f40806cbe810de8872f1698bf053`, with renames on,
`GARDENBED_FSYNC=off`, `refs/heads/master`, fresh metadata/object stores,
and `--limit 1000` or `5000`. One build/test/import process held the shared
memory-bounded gate throughout. Each endpoint's one-minute load was below 4
and varied by less than a factor of two within each run. All child exits were
zero, every run imported exactly its limit of new changesets and Git mappings,
and no commit was skipped. Times below are ImportReport elapsed seconds;
external process wall times, load samples, hashes and peak RSS are retained
in the adjacent JSON evidence.

| New commits | Pair | Control seconds | Treatment seconds |
| --- | --- | ---: | ---: |
| 1000 | final1000-1 | 43.27 | 42.99 |
| 1000 | final1000-2 | 40.07 | 40.32 |
| 1000 | final1000-3 | 39.92 | 43.39 |
| 5000 | final5000-1 | 58.18 | 60.00 |

The arithmetic mean of per-run throughput for the three 1,000-commit runs
is 24.3724 commits/s for control and 23.7032 for treatment (ratio 0.972544).
The 5,000-commit pair is 85.94 versus 83.33 commits/s (ratio 0.9697).
Treatment peak RSS was about 2.25 GiB at 1,000 commits and 2.44 GiB at 5,000.

Both sides captured 245 raw commits at 1,000 and 953 at 5,000. Treatment
captured **zero raw trees** in these segments. This measures traversal and
classification overhead on real kernel data; it does not measure compression
cost for divergent trees. Noncanonical modes, non-UTF-8 paths and empty
directory closure are exercised by the hermetic SHA-exact fidelity tests.
The full ordered Git mapping and canonical changeset sets of the 5,000-commit
control and treatment runs were identical.

## Historical baseline and phase diagnostic

The historical 278 commits/s was a 619,487-commit resumed import in 2,230 s
(see corpora.md). The fresh limited runs above are outside its absolute ±20%
interval. Matching that figure to these totals would be a workload mismatch;
it is not evidence of host noise. `graph.rs` walks and sorts all 1,485,163
reachable nodes before limiting a fresh import. Each truncated ancestry root
also requires a full snapshot; a resumed import already has those parents.

Separate diagnostic-only native commits add phase timers to the control
(`0bf6b4a1069fe505`) and treatment (`59a59f2330358627`). They are never part
of the feature or main. Their 5,000-commit outputs must match the corresponding
uninstrumented primary changeset and Git mapping sets exactly. The diagnostic
results below separate graph setup, root snapshots, and nonroot ingestion
(including diff subprocess time). These component rates explain the differing
workload; they are not substituted for the primary end-to-end measurements
or presented as a new full resumed-import benchmark.

| Diagnostic phase | Control | Treatment |
| --- | ---: | ---: |
| Graph setup (seconds) | 15.323 | 15.259 |
| Root snapshots (seconds; 15 roots) | 28.127 | 27.837 |
| Nonroot ingestion + diff (seconds; 4,985 commits) | 16.788 | 15.683 |
| Nonroot component rate (commits/s) | 296.944 | 317.853 |

Both diagnostics exited zero, met the same quiet criteria, and produced exactly
the same canonical changesets and Git mappings as their uninstrumented primary
runs. The component rates (297 and 318 commits/s) are within ±20% of 278;
this supports the workload explanation, without claiming that subtracting
phases constitutes an independently measured full resumed import.

The campaign used roughly 2.3–2.5 GiB RSS, within its 6 GiB memory scope. It
makes no claim about the separate full-kernel GC RSS or wall-clock budgets.
Earlier exploratory pairs and rejected noisy attempts remain in the local
recovery campaign directory; none were included in the final three-pair mean.
The adjacent JSON retains all eight final primary observations and both
separate diagnostics, including binary digests and raw phase text. Logs and
SQLite outputs remain under `.gardenbed-recovery/2026-10-21/throughput-campaign/`;
that local recovery directory is deliberately excluded from exported source.
