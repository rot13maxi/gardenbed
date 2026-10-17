//! A deterministic demo repository: sixteen changesets of a small "garden
//! planner" web app, written with fixed identities and timestamps so a
//! fresh `gardenbed-web seed` produces byte-identical changeset ids every
//! time. Used by the `seed` subcommand to give the GUI something real to
//! browse.
//!
//! Shape: **twelve published commits on `main`** (a mainline with one
//! side branch and merge) plus **four unpublished ones** (two draft heads
//! and a rebase pair) that are stored but ref-less — the shape agent
//! working copies produce and the activity view exists to surface.
//!
//! The history exercises everything the UI renders: adds, modifies, one
//! pure rename (`plan/spring.md` → `plan/2026.md`), one delete
//! (`notes.txt`), an executable file (`bin/sow.sh`, [`Mode::Exec`]),
//! three rotating authors on mainline (two more on the drafts), and
//! multi-paragraph commit messages.

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
/// Unpublished-only authors: the draft commits 13–16.
const PIKE: (&str, &str) = ("Pike Rowan", "pike@example.com");
const MOSS: (&str, &str) = ("Moss Hale", "moss@example.com");

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

    /// One changeset on an explicit parent list (the merge + side-branch
    /// commits use this directly; `commit!` below is the linear chain).
    macro_rules! commit_at {
        ($n:expr, $author:expr, $parents:expr, $message:expr, [$($edit:expr),* $(,)?]) => {{
            let parents: Vec<Id> = $parents;
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

    /// Linear-chain commit: one parent — the running tip.
    macro_rules! commit {
        ($n:expr, $author:expr, $message:expr, [$($edit:expr),* $(,)?]) => {
            commit_at!($n, $author, tip.into_iter().collect::<Vec<Id>>(), $message, [$($edit),*])
        };
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
    // Branch point for the side line that gets merged after c8.
    let c3 = tip;

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

    // Side branch off c3: Ken adds watering data while mainline moved on
    // to the sow script, README, rename, prune, and the spacing tune.
    // The merge below gives the seeded history a two-parent changeset so
    // the commit graph renders its merge nodes and second-parent lane
    // against real data.
    let mainline = tip;
    commit_at!(
        9,
        KEN,
        vec![c3.expect("commit 3 ran")],
        "Add watering schedule data\n\nPer-crop intervals plus the next-watering math the grid will\nneed; no code changes, data only.\n",
        [Edit::Write {
            op: Op::Add,
            path: "data/watering.json",
            content: WATERING_JSON,
            mode: Mode::Regular,
        }]
    );
    let side = tip;

    // Merge: the mainline tip stays c8 (the side commit did not move
    // it); the merged tree gains exactly the side branch's file
    // (disjoint from everything mainline touched).
    let mainline_c8 = mainline;
    commit_at!(
        10,
        ROSA,
        vec![
            mainline_c8.expect("commit 8 ran"),
            side.expect("side commit ran"),
        ],
        "Merge watering schedule into the planner\n\nKen's watering data lands alongside the sow script; the grid\nkeeps its newer palette. \n",
        [Edit::Write {
            op: Op::Add,
            path: "data/watering.json",
            content: WATERING_JSON,
            mode: Mode::Regular,
        }]
    );

    commit!(
        11,
        KEN,
        "Add rotation notes for 2027",
        [Edit::Write {
            op: Op::Add,
            path: "plan/rotation.md",
            content: ROTATION,
            mode: Mode::Regular,
        }]
    );
    let c11 = tip;

    commit!(
        12,
        ROSA,
        "Document the sow script",
        [Edit::Write {
            op: Op::Modify,
            path: "README.md",
            content: README_V3,
            mode: Mode::Regular,
        }]
    );

    // Publish the twelve-commit mainline; everything after this point is
    // stored but unreachable from `main` — agent working copies commit
    // straight to the server, and the GUI's activity view must see them.
    let published = tip.expect("seed always creates twelve changesets");
    repo.cas_ref(DEMO_REF, None, published)?;

    // Draft A: an unfinished drip-irrigation sketch on top of the tip.
    commit!(
        13,
        PIKE,
        "Draft: drip irrigation sketch\n\nNot ready: zones still hardcoded.\n",
        [Edit::Write {
            op: Op::Add,
            path: "plan/drip.md",
            content: DRIP_MD,
            mode: Mode::Regular,
        }]
    );
    let _draft_a = tip; // unpublished: never pointed at by a ref

    // Draft B: a calmer resting-bed palette, branched from the same tip —
    // two unpublished heads that never merge into mainline.
    commit_at!(
        14,
        MOSS,
        vec![published],
        "draft: calmer empty-bed colors",
        [Edit::Write {
            op: Op::Modify,
            path: "styles.css",
            content: STYLES_V3,
            mode: Mode::Regular,
        }]
    );
    let _draft_b = tip; // unpublished

    // A rebase pair — same author, same message: v1 sat on the rotation
    // notes commit, v2 was re-based onto the published tip (its file edit
    // differs by one trailing line, so the ids differ even before
    // parenting). Both stay unpublished.
    commit_at!(
        15,
        PIKE,
        vec![c11.expect("commit 11 ran")],
        "Add evening mulch reminder",
        [Edit::Write {
            op: Op::Add,
            path: "plan/mulch.md",
            content: MULCH_V1,
            mode: Mode::Regular,
        }]
    );
    let _rebase_v1 = tip; // unpublished
    commit_at!(
        16,
        PIKE,
        vec![published],
        "Add evening mulch reminder",
        [Edit::Write {
            op: Op::Add,
            path: "plan/mulch.md",
            content: MULCH_V2,
            mode: Mode::Regular,
        }]
    );
    let _rebase_v2 = tip; // unpublished

    Ok(Some(published))
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

// --- Unpublished drafts (commits 13–16): stored, never ref'd ----------

const DRIP_MD: &str = "\
# drip irrigation sketch

Zones are hardcoded for now; sizing comes later.

- zone 1 (north bed): 4 L/h dripline, 30 min mornings
- zone 2 (middle bed): 2 L/h, lettuce hates wet crowns
- zone 3 (south bed): 4 L/h, sandy side drains fast

Total budget: ~120 L per watering day; tank holds 400 L.
";

/// STYLES_V2 plus the one rule draft B experiments with: muted resting
/// beds (never published — commit 14 stays ref-less).
const STYLES_V3: &str = "\
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

/* Pike suggested keeping only the tint; Moss wants borders gone too. */
.bed-cell.resting {
  border-color: transparent;
  opacity: 0.85;
}
";

const MULCH_V1: &str = "\
# evening mulch reminder

- check the brassica bed for drying
- top up straw on the carrot rows before dusk
";

/// Same reminder, one trailing line added — the re-based v2 (commit 16).
const MULCH_V2: &str = "\
# evening mulch reminder

- check the brassica bed for drying
- top up straw on the carrot rows before dusk
- record overnight low for the frost log
";

const WATERING_JSON: &str = "\
{
  \"intervals_days\": {
    \"carrot\": 2,
    \"lettuce\": 1,
    \"tomato\": 3
  },
  \"note\": \"deep water, less often, once seedlings establish\"
}
";
