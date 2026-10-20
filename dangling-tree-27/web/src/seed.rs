//! A deterministic demo repository: ten changesets of a small "garden
//! planner" web app, written with fixed identities and timestamps so a
//! fresh `gardenbed-web seed` produces byte-identical changeset ids every
//! time. Used by the `seed` subcommand to give the GUI something real to
//! browse.
//!
//! The history exercises everything the UI renders: adds, modifies, one
//! pure rename (`plan/spring.md` → `plan/2026.md`), one delete
//! (`notes.txt`), an executable file (`bin/sow.sh`, [`Mode::Exec`]),
//! three rotating authors, and multi-paragraph commit messages.

use std::collections::BTreeMap;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::{Repo, ServerError};

/// The ref the demo history publishes at the end.
const DEMO_REF: &str = "main";

/// Epoch base of commit 1; commit `i` lands `i * 86_400` s later (one
/// commit per day), all at `tz_min = 0` — fixed, so ids are stable.
const T0_SECS: u64 = 1_751_500_000;

/// Author "A": commits 1, 2, 5, 6, 10.
const ROSA: (&str, &str) = ("Rosa Bloom", "rosa@example.com");
/// Author "B": commits 3, 7, 9.
const KEN: (&str, &str) = ("Ken Furrow", "ken@example.com");
/// Author "C": commits 4, 8.
const ADA: (&str, &str) = ("Ada Trellis", "ada@example.com");

/// One file-level edit in a seeded changeset. Content is static so the
/// whole history is compile-time data.
enum Edit {
    /// `path` gains `content` with `mode` (Add or Modify chosen by the
    /// caller — the op is part of the pinned history shape).
    Write {
        op: Op,
        path: &'static str,
        content: &'static str,
        mode: Mode,
    },
    Delete(&'static str),
    /// Pure rename: `content` is the source's bytes, whose id the rename
    /// change must carry (a pure rename reuses the old blob id); it is
    /// already in objects, so nothing rides inline.
    Rename {
        from: &'static str,
        to: &'static str,
        content: &'static str,
    },
}

/// Seeds the demo history unless the repository already carries refs.
///
/// * `Ok(Some(tip))` — history was written and `main` now points at
///   `tip`.
/// * `Ok(None)` — refs already existed; nothing was touched
///   (idempotent re-run).
pub fn seed(repo: &Repo) -> Result<Option<Id>, ServerError> {
    if !repo.list_refs()?.is_empty() {
        return Ok(None);
    }

    let mut tip: Option<Id> = None;

    macro_rules! commit {
        ($n:expr, $author:expr, $message:expr, [$($edit:expr),* $(,)?]) => {{
            let parents: Vec<Id> = tip.into_iter().collect();
            let (name, email) = $author;
            let identity = Identity {
                name: name.to_owned(),
                email: email.to_owned(),
                time_secs: T0_SECS + $n * 86_400,
                tz_min: 0,
            };
            let mut inline: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            let mut changes: Vec<Change> = Vec::new();
            $(match $edit {
                Edit::Write { op, path, content, mode } => {
                    let id = blob_id(content.as_bytes());
                    inline.insert(id.to_hex(), content.as_bytes().to_vec());
                    changes.push(Change {
                        path: path.to_owned(),
                        op,
                        blob: Some(id),
                        src_path: None,
                        mode,
                    });
                }
                Edit::Delete(path) => changes.push(Change {
                    path: path.to_owned(),
                    op: Op::Delete,
                    blob: None,
                    src_path: None,
                    mode: Mode::Regular,
                }),
                Edit::Rename { from, to, content } => changes.push(Change {
                    path: to.to_owned(),
                    op: Op::Rename,
                    blob: Some(blob_id(content.as_bytes())),
                    src_path: Some(from.to_owned()),
                    mode: Mode::Regular,
                }),
            })*
            // Canonical order: strictly ascending by path bytes.
            changes.sort_by(|a, b| a.path.cmp(&b.path));
            let cs = Changeset {
                parents,
                changes,
                message: $message.to_owned(),
                author: identity.clone(),
                committer: identity,
                signature: None,
            };
            let id = repo.create_changeset(&cs, &inline)?;
            tip = Some(id);
        }};
    }

    commit!(
        1,
        ROSA,
        "Initial commit: README and garden plan outline",
        [
            Edit::Write {
                op: Op::Add,
                path: "README.md",
                content: README_V1,
                mode: Mode::Regular,
            },
            Edit::Write {
                op: Op::Add,
                path: "plan/spring.md",
                content: SPRING_PLAN,
                mode: Mode::Regular,
            },
            Edit::Write {
                op: Op::Add,
                path: "notes.txt",
                content: "scratch notes\n",
                mode: Mode::Regular,
            },
        ]
    );

    commit!(
        2,
        ROSA,
        "Add bed data and styling",
        [
            Edit::Write {
                op: Op::Add,
                path: "data/beds.json",
                content: BEDS_JSON,
                mode: Mode::Regular,
            },
            Edit::Write {
                op: Op::Add,
                path: "styles.css",
                content: STYLES_V1,
                mode: Mode::Regular,
            },
        ]
    );

    commit!(
        3,
        KEN,
        "Build interactive bed grid\n\nrenderBedGrid() turns data/beds.json into a table of clickable\ncells; each cell remembers its crop and last watering date.\n",
        [Edit::Write {
            op: Op::Add,
            path: "garden.js",
            content: GARDEN_JS_V1,
            mode: Mode::Regular,
        }]
    );

    commit!(
        4,
        ADA,
        "Add sow script",
        [Edit::Write {
            op: Op::Add,
            path: "bin/sow.sh",
            content: SOW_SH,
            mode: Mode::Exec,
        }]
    );

    commit!(
        5,
        ROSA,
        "Expand README with usage instructions",
        [Edit::Write {
            op: Op::Modify,
            path: "README.md",
            content: README_V2,
            mode: Mode::Regular,
        }]
    );

    commit!(
        6,
        ROSA,
        "Rename spring plan to yearly plan\n\nThe outline grew past spring: it now tracks the whole season.\nContents unchanged; only the path moves to plan/2026.md.\n",
        [Edit::Rename {
            from: "plan/spring.md",
            to: "plan/2026.md",
            content: SPRING_PLAN,
        }]
    );

    commit!(7, KEN, "Prune outdated notes", [Edit::Delete("notes.txt")]);

    commit!(
        8,
        ADA,
        "Tune grid spacing and colors\n\nLooser rows for deep-root crops and a calmer palette for beds\nthat are resting between sowings.\n",
        [
            Edit::Write {
                op: Op::Modify,
                path: "garden.js",
                content: GARDEN_JS_V2,
                mode: Mode::Regular,
            },
            Edit::Write {
                op: Op::Modify,
                path: "styles.css",
                content: STYLES_V2,
                mode: Mode::Regular,
            },
        ]
    );

    commit!(
        9,
        KEN,
        "Add rotation notes for 2027",
        [Edit::Write {
            op: Op::Add,
            path: "plan/rotation.md",
            content: ROTATION,
            mode: Mode::Regular,
        }]
    );

    commit!(
        10,
        ROSA,
        "Document the sow script",
        [Edit::Write {
            op: Op::Modify,
            path: "README.md",
            content: README_V3,
            mode: Mode::Regular,
        }]
    );

    let tip = tip.expect("seed always creates ten changesets");
    repo.cas_ref(DEMO_REF, None, tip)?;
    Ok(Some(tip))
}

// ------------------------------------------------------ pinned content

const README_V1: &str = "\
# garden planner

A tiny in-browser planner for backyard vegetable beds.

## what it does

- lays out beds on a simple grid
- records which crop lives in which bed
- keeps short plans as markdown under `plan/`
- stores bed data as plain JSON under `data/`

## layout

- `index.html` opens the planner in any browser
- `garden.js` renders the bed grid and handles clicks
- `styles.css` keeps the grid readable outdoors on a phone
- `data/beds.json` is the single source of truth for beds

## philosophy

The planner is deliberately static: no build step, no server, no
framework. Open the page, look at the beds, close it again. Notes and
longer thoughts live in markdown files, versioned alongside the code
they describe.

Contributions welcome; keep the no-build rule.
";

const README_V2: &str = "\
# garden planner

A tiny in-browser planner for backyard vegetable beds.

## what it does

- lays out beds on a simple grid
- records which crop lives in which bed
- keeps short plans as markdown under `plan/`
- stores bed data as plain JSON under `data/`

## layout

- `index.html` opens the planner in any browser
- `garden.js` renders the bed grid and handles clicks
- `styles.css` keeps the grid readable outdoors on a phone
- `data/beds.json` is the single source of truth for beds

## philosophy

The planner is deliberately static: no build step, no server, no
framework. Open the page, look at the beds, close it again. Notes and
longer thoughts live in markdown files, versioned alongside the code
they describe.

Contributions welcome; keep the no-build rule.

## usage

1. edit `data/beds.json` to match your real beds
2. open `index.html` in a browser
3. click a bed cell to cycle the crop
4. check `plan/` before each sowing weekend
";

const README_V3: &str = "\
# garden planner

A tiny in-browser planner for backyard vegetable beds.

## what it does

- lays out beds on a simple grid
- records which crop lives in which bed
- keeps short plans as markdown under `plan/`
- stores bed data as plain JSON under `data/`

## layout

- `index.html` opens the planner in any browser
- `garden.js` renders the bed grid and handles clicks
- `styles.css` keeps the grid readable outdoors on a phone
- `data/beds.json` is the single source of truth for beds

## philosophy

The planner is deliberately static: no build step, no server, no
framework. Open the page, look at the beds, close it again. Notes and
longer thoughts live in markdown files, versioned alongside the code
they describe.

Contributions welcome; keep the no-build rule.

## usage

1. edit `data/beds.json` to match your real beds
2. open `index.html` in a browser
3. click a bed cell to cycle the crop
4. check `plan/` before each sowing weekend

## the sow script

[`bin/sow.sh`](bin/sow.sh) turns `data/beds.json` into a printable
sowing checklist. Run it the morning of a sowing day; it needs only
`sh` and `grep`.

```sh
bin/sow.sh --bed north-1 --crop carrot --date 2026-04-11
```

Bed contents stay in [the data file](data/beds.json); the yearly plan
lives in [plan/2026.md](plan/2026.md).
";

const SPRING_PLAN: &str = "\
# spring plan

Rough outline for the spring sowing season.

- bed north: tomatoes after the last frost
- bed middle: lettuce, two rounds
- bed south: carrots and spring onions
";

const BEDS_JSON: &str = "\
[
  { \"name\": \"north bed\", \"crop\": \"tomatoes\" },
  { \"name\": \"middle bed\", \"crop\": \"lettuce\" },
  { \"name\": \"south bed\", \"crop\": \"carrots\" }
]
";

const STYLES_V1: &str = "\
:root {
  --soil: #4a3b2a;
  --leaf: #3f7d3a;
  --paper: #f7f4ec;
}

body {
  margin: 0;
  font-family: sans-serif;
  background: var(--paper);
  color: var(--soil);
}

.bed-grid {
  display: grid;
  gap: 4px;
  padding: 8px;
}

.bed-cell {
  background: var(--leaf);
  border: 1px solid var(--soil);
  min-height: 48px;
}
";

const STYLES_V2: &str = "\
:root {
  --soil: #4a3b2a;
  --leaf: #5a8f52;
  --resting: #b7ad97;
  --paper: #f7f4ec;
}

body {
  margin: 0;
  font-family: sans-serif;
  background: var(--paper);
  color: var(--soil);
}

.bed-grid {
  display: grid;
  gap: 8px;
  padding: 12px;
}

.bed-cell {
  background: var(--leaf);
  border: 1px solid var(--soil);
  min-height: 56px;
}

.bed-cell.resting {
  background: var(--resting);
}
";

const GARDEN_JS_V1: &str = "\
// garden.js — the interactive bed grid.
//
// Loaded by index.html; reads data/beds.json with fetch() and paints one
// cell per bed. No dependencies, no build step.

const GARDEN = {
  beds: [],
  selected: null,
};

async function loadBeds() {
  const resp = await fetch('data/beds.json');
  if (!resp.ok) {
    throw new Error('cannot load data/beds.json: ' + resp.status);
  }
  GARDEN.beds = await resp.json();
}

function bedCell(bed) {
  const cell = document.createElement('button');
  cell.className = 'bed-cell';
  cell.dataset.bed = bed.name;
  cell.textContent = bed.name + ': ' + bed.crop;
  cell.addEventListener('click', () => selectBed(bed.name));
  return cell;
}

function selectBed(name) {
  GARDEN.selected = name;
  for (const cell of document.querySelectorAll('.bed-cell')) {
    cell.classList.toggle('selected', cell.dataset.bed === name);
  }
  const detail = document.getElementById('bed-detail');
  const bed = GARDEN.beds.find((b) => b.name === name);
  detail.textContent = bed ? bed.name + ' grows ' + bed.crop : '';
}

function renderBedGrid(rootId) {
  const root = document.getElementById(rootId);
  if (!root) {
    throw new Error('bed-grid root not found: ' + rootId);
  }
  root.classList.add('bed-grid');
  root.textContent = '';
  for (const bed of GARDEN.beds) {
    root.appendChild(bedCell(bed));
  }
  const detail = document.createElement('p');
  detail.id = 'bed-detail';
  root.after(detail);
}

async function main() {
  await loadBeds();
  renderBedGrid('app');
}

main().catch((err) => {
  document.getElementById('app').textContent = String(err);
});
";

const GARDEN_JS_V2: &str = "\
// garden.js — the interactive bed grid.
//
// Loaded by index.html; reads data/beds.json with fetch() and paints one
// cell per bed. No dependencies, no build step.
//
// v2: looser row spacing for deep-root crops and a 'resting' state for
// beds between sowings (see data/beds.json).

const GARDEN = {
  beds: [],
  selected: null,
};

async function loadBeds() {
  const resp = await fetch('data/beds.json');
  if (!resp.ok) {
    throw new Error('cannot load data/beds.json: ' + resp.status);
  }
  GARDEN.beds = await resp.json();
}

function bedCell(bed) {
  const cell = document.createElement('button');
  cell.className = 'bed-cell';
  if (bed.crop === 'resting') {
    cell.classList.add('resting');
  }
  cell.dataset.bed = bed.name;
  cell.textContent = bed.name + ': ' + bed.crop;
  cell.addEventListener('click', () => selectBed(bed.name));
  return cell;
}

function selectBed(name) {
  GARDEN.selected = name;
  for (const cell of document.querySelectorAll('.bed-cell')) {
    cell.classList.toggle('selected', cell.dataset.bed === name);
  }
  const detail = document.getElementById('bed-detail');
  const bed = GARDEN.beds.find((b) => b.name === name);
  detail.textContent = bed ? bed.name + ' grows ' + bed.crop : '';
}

function renderBedGrid(rootId) {
  const root = document.getElementById(rootId);
  if (!root) {
    throw new Error('bed-grid root not found: ' + rootId);
  }
  root.classList.add('bed-grid');
  root.textContent = '';
  for (const bed of GARDEN.beds) {
    root.appendChild(bedCell(bed));
  }
  const detail = document.createElement('p');
  detail.id = 'bed-detail';
  root.after(detail);
}

async function main() {
  await loadBeds();
  renderBedGrid('app');
}

main().catch((err) => {
  document.getElementById('app').textContent = String(err);
});
";

const SOW_SH: &str = "\
#!/bin/sh
set -eu

# bin/sow.sh — print a sowing checklist from data/beds.json.

beds_file=${1:-data/beds.json}

if [ ! -f \"$beds_file\" ]; then
  echo \"no bed data at $beds_file\" >&2
  exit 1
fi

echo \"sowing checklist:\"
grep -o '\"crop\": \"[^\"]*\"' \"$beds_file\" | sed 's/\"crop\": //' | tr -d '\"'
echo \"done.\"
";

const ROTATION: &str = "\
# rotation notes 2027

- north bed: legumes after this year's tomatoes
- middle bed: brassicas; lettuce moves south
- south bed: alliums, then a green manure year
";
