# Terminology

Gardenbed is the product name; `bed` is its CLI. Use standard source-control
and systems terms for everything else. A reader should not need a glossary
of project-specific metaphors to understand an operation or error.

## Core concepts

| Term | Meaning |
|---|---|
| Repository | Server-side history, objects, and refs |
| Server | The `gardenbed-server` process serving a repository |
| Working copy | Local files and their `.bed/` state directory |
| Changeset | An immutable record of file changes, parents, and metadata |
| Commit | The operation that creates a changeset on the server |
| Sparse paths | Repository-relative prefixes selected for local materialization |
| Object store | Content-addressed blobs and trees in `objects/` |
| Metadata database | Changesets, refs, Git mappings, and validation records in `metadata.sqlite3` |
| Derived data | Rebuildable trees, blame, ancestry, and path summaries |
| Git bridge | Import/export support in the `gardenbed-git` crate |
| Local daemon | A planned filesystem watcher and shared cache |

A commit and publication are separate operations. `bed commit` records work
on the server; `bed publish` fast-forwards `main` to the current changeset.
An unpublished changeset is already stored on the server. Publication does
not perform a content merge.

## Commands

| Command | Meaning |
|---|---|
| `bed clone` | Create a working copy without materializing files |
| `bed sparse add/list/remove` | Manage selected paths |
| `bed switch` | Switch the working copy to a changeset or ref |
| `bed commit` | Create a changeset from local changes |
| `bed rebase` | Replay the current changeset onto another parent |
| `bed publish` | Fast-forward `main` to the current changeset |
| `bed status/log/diff/file/blame` | Inspect local changes or repository history |

Use `--state-dir` for the local state directory and `clone --sparse` to record
initial path selections. `clone --sparse` does not fetch working files;
`bed sparse add` does. `sparse remove` changes the selection and keeps files.
`rebase` currently replays one changeset, not an entire stack.

## Writing and naming rules

- Describe the result directly: “created changeset,” “current changeset,”
  “working copy is clean,” and “server refused.”
- Explain an unfamiliar implementation term when it matters. User-facing
  errors should name the operation, affected path, and next step.
- Prefer ordinary component names in documentation, APIs, and code. Avoid
  adding alternate names for servers, databases, caches, stacks, and sandboxes.
- Keep storage paths and examples consistent with the running software.

The original design notes, release records, and recorded corpus reports use
historical names. They describe earlier versions; the current CLI, API, and
layout use the terms above. Old command names and flags are not aliases.
