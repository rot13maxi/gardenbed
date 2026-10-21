# Export mapping recording (`--record-mappings`)

`gardenbed-server export --record-mappings` persists the computed
changeset↔git-SHA mapping into the metadata store's `git_map` table after
a successful export (v0.2, dogfood #26). This closes the loop that
previously required manually recording the landing tip: after any
`--record-mappings` export, the OPERATIONS §1.1b `MAIN_TWIN` query
answers for **native** main tips too, and a re-import of the exported
repository into the same stores is a pure no-op.

## Semantics

Per exported `(git sha, changeset)` pair:

| State | Outcome |
|---|---|
| Neither side bound | recorded (one transaction for the whole batch) |
| Already recorded exactly | idempotent no-op, counted `unchanged` |
| Sha bound to a different cs, or cs bound to a different sha | **refused** (never overwritten), reported as a conflict |

* **Never an overwrite.** Import-written rows are canonical; a native
  changeset whose git bytes collide with an imported twin's (or a later
  pair repeating a sha within one batch) is refused with a warning, and
  the existing mapping survives.
* **All-or-nothing.** Accepted pairs land in one store transaction per
  batch. A crash between the git side (pack/idx/refs, already complete)
  and the metadata side leaves the batch entirely absent — re-running
  the same export repairs it (every pair is idempotent). A malformed sha
  in the batch fails the whole call before any row lands.
* **Conflicts are data, not errors.** The export itself succeeded; each
  conflict is printed on stderr as
  `warning: export mapping conflict: …` naming the sha, the changeset,
  and the surviving canonical row.
* **Only successful bytes are recorded.** Recording happens after
  `write_refs`; a refused (e.g. non-bare target) or failed export
  records nothing.

## Report line

With `--record-mappings`, the export report gains a suffix:

```text
274 commits (0 signed dropped), 17482 blobs, 31 ref(s), 8.93s, mappings: 51 recorded, 223 unchanged, 0 conflict(s)
```

Without the flag the report line is byte-identical to before (scripts
parsing the line — demo/run-v0.2.sh, the runbook — are unaffected).

## Where it plugs in

* **Store**: `MetadataStore::record_export_mappings(&[(sha, cs)])`
  (required method; SQLite / Memory / Postgres each implement the
  one-transaction batch with in-batch twin classification).
* **Library**: `gardenbed_git::ExportOptions::record_mappings` (default
  `false` — every other caller keeps the read-only behavior), outcome in
  `ExportReport::mappings: Option<ExportMappingReport>`.
* **CLI**: `--record-mappings` on `export` (local and `--storage` modes).

## Limitations

* The mapping table stays a bijection: no alias support for multiple
  changesets sharing one git SHA (the one genuine collision shape — a
  native-vs-re-imported twin — is refused and reported, not aliased).
  Canonical history has no duplicate SHAs, so this is deliberately not
  over-engineered.
* `--record-mappings` writes to the metadata store: it is not safe to
  run concurrently with an import racing the same sha. The bijection
  refusal makes the outcome safe (no overwrite, no corruption) either
  way; the loser is simply reported.
