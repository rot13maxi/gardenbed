/* gardenbed web GUI — vanilla, hash-routed, no build step.
 *
 * Sections:
 *   1. state & api helpers
 *   2. formatters
 *   3. tiny syntax highlighter (fail-safe, own tokenizer)
 *   4. dom helpers & components
 *   5. views (one function per route)
 *   6. router & boot
 */

"use strict";

/* ================= 1. state & api helpers ================= */

const state = {
  refs: [],          // [{name, cs}]
  rev: null,         // selected default ref name (drives cs= params)
  refsLoaded: false,
};

const REF_KEY = "gardenbed.ref";

/** Fetch a JSON endpoint; throws Error with the server's {error} message. */
async function api(path) {
  let res;
  try {
    res = await fetch(path);
  } catch (e) {
    throw new Error("network error: " + (e && e.message ? e.message : e));
  }
  let body = null;
  try { body = await res.json(); } catch (_) { /* fall through */ }
  if (!res.ok) {
    const msg = body && body.error ? body.error : "HTTP " + res.status;
    const err = new Error(msg);
    err.status = res.status;
    throw err;
  }
  if (body === null) throw new Error("invalid JSON response");
  return body;
}

function qs(params) {
  const p = new URLSearchParams();
  for (const k in params) if (params[k] !== null && params[k] !== undefined) p.set(k, params[k]);
  return p.toString();
}

/** Default revspec: the ref chosen in the top-bar selector (loaded async). */
function currentRev() {
  return state.rev || "main";
}

/* session caches: files by (rev,path), changesets by id */
const FILE_CACHE = {};
function fetchFileCached(rev, path) {
  const k = rev + "" + path;
  if (!FILE_CACHE[k]) FILE_CACHE[k] = api("/api/file?" + qs({ cs: rev, path: path }));
  return FILE_CACHE[k];
}
const CS_CACHE = {};
function fetchChangesetCached(id) {
  if (!CS_CACHE[id]) CS_CACHE[id] = api("/api/changeset?" + qs({ id: id }));
  return CS_CACHE[id];
}

function loadRefs() {
  const sel = document.getElementById("ref-select");
  return api("/api/refs").then((data) => {
    state.refs = (data && data.refs) || [];
    state.refsLoaded = true;
    sel.textContent = "";
    if (!state.refs.length) {
      sel.appendChild(option("— no refs —", ""));
      sel.disabled = true;
      return;
    }
    const stored = safeStorage("get", REF_KEY);
    let chosen = stored && state.refs.some((r) => r.name === stored) ? stored : null;
    if (!chosen) {
      const main = state.refs.find((r) => r.name === "refs/heads/main" || r.name === "main");
      chosen = (main || state.refs[0]).name;
    }
    state.rev = chosen;
    for (const r of state.refs) sel.appendChild(option(r.name, r.name));
    sel.value = chosen;
    sel.disabled = false;
    sel.addEventListener("change", () => {
      state.rev = sel.value;
      safeStorage("set", REF_KEY, state.rev);
      route();
    });
    for (const r of state.refs) lastTips[r.name] = r.cs; // live-poll baseline
    startLivePoll();
  }).catch(() => {
    sel.textContent = "";
    sel.appendChild(option("refs unavailable", ""));
    sel.disabled = true;
  });
}

function option(label, value) {
  const o = document.createElement("option");
  o.value = value;
  o.textContent = label;
  return o;
}

function safeStorage(op, key, val) {
  try {
    if (typeof localStorage === "undefined") return null;
    return op === "get" ? localStorage.getItem(key) : localStorage.setItem(key, val);
  } catch (_) {
    return null;
  }
}

/* ================= 2. formatters ================= */

function shortId(id, n) {
  return typeof id === "string" ? id.slice(0, n || 12) : "";
}

function pad2(n) { return String(n).padStart(2, "0"); }

/**
 * Civil time in the identity's own recorded offset. We shift the epoch by
 * tz_min and then read UTC fields, so the result never depends on the
 * viewer's local timezone.
 */
function civilTime(idn) {
  if (!idn || typeof idn.time_secs !== "number") return "";
  const d = new Date((idn.time_secs + (idn.tz_min || 0) * 60) * 1000);
  const s = d.getUTCFullYear() + "-" + pad2(d.getUTCMonth() + 1) + "-" + pad2(d.getUTCDate()) +
    " " + pad2(d.getUTCHours()) + ":" + pad2(d.getUTCMinutes()) + ":" + pad2(d.getUTCSeconds());
  const tz = idn.tz_min || 0;
  const sign = tz >= 0 ? "+" : "-";
  const ab = Math.abs(tz);
  return s + " " + sign + pad2(Math.floor(ab / 60)) + pad2(ab % 60);
}

function relativeTime(secs) {
  if (typeof secs !== "number") return "";
  const diff = Math.max(0, Math.floor(Date.now() / 1000) - secs);
  const units = [[31536000, "y"], [2592000, "mo"], [604800, "w"], [86400, "d"], [3600, "h"], [60, "m"]];
  for (const [span, label] of units) {
    if (diff >= span) return Math.floor(diff / span) + label + " ago";
  }
  return diff <= 1 ? "just now" : diff + "s ago";
}

function humanSize(n) {
  if (typeof n !== "number") return "";
  if (n < 1024) return n + " B";
  if (n < 1024 * 1024) return (n / 1024).toFixed(1).replace(/\.0$/, "") + " KiB";
  return (n / (1024 * 1024)).toFixed(1).replace(/\.0$/, "") + " MiB";
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function firstLine(s) {
  const i = String(s == null ? "" : s).indexOf("\n");
  return i < 0 ? String(s == null ? "" : s) : String(s).slice(0, i);
}

function restLines(s) {
  const i = String(s == null ? "" : s).indexOf("\n");
  return i < 0 ? "" : String(s).slice(i + 1).replace(/\n+$/, "");
}

/** URL for browsing a path at a rev. Path segments are individually encoded. */
function browseHref(rev, path) {
  return pathHref("browse", rev, path);
}

function fileHref(rev, path) {
  return pathHref("file", rev, path);
}

function pathHref(kind, rev, path) {
  let h = "#/" + kind + "/" + encodeURIComponent(rev || currentRev());
  if (path) h += "/" + path.split("/").map(encodeURIComponent).join("/");
  return h;
}

/** Deterministic hue for a changeset id (blame coloring). */
function idHue(id) {
  let h = 5381;
  for (let i = 0; i < id.length; i++) h = ((h * 33) ^ id.charCodeAt(i)) >>> 0;
  return h % 360;
}

/* ================= 3. tiny syntax highlighter ================= */
/* Own tokenizer for a few languages; unknown extensions get no
 * highlighting (fail-safe beats wrong highlighting). */

const HL_LANGS = {
  rs: { com: "//", blk: ["/*", "*/"], str: ['"'], rawStr: true,
    kw: "fn let mut struct enum impl pub use mod match if else for while loop in return trait where const static type dyn move async await self Self crate super ref break continue unsafe extern as true false" },
  js: { com: "//", blk: ["/*", "*/"], str: ['"', "'", "`"],
    kw: "const let var function return if else for while do switch case default break continue new delete in of typeof instanceof this class extends super static get set import export from async await try catch finally throw yield null undefined true false" },
  ts: { com: "//", blk: ["/*", "*/"], str: ['"', "'", "`"],
    kw: "const let var function return if else for while do switch case default break continue new delete in of typeof instanceof this class extends super static get set import export from async await try catch finally throw yield null undefined true false interface type implements readonly declare enum namespace keyof" },
  py: { com: "#", str: ['"', "'"],
    kw: "def class return if elif else for while in not and or is None True False import from as with try except finally raise lambda pass global nonlocal yield async await assert del" },
  json: { str: ['"'], kw: "true false null" },
  toml: { com: "#", str: ['"', "'"], kw: "true false" },
  sh: { com: "#", str: ['"', "'"],
    kw: "if then else elif fi for while in do done case esac function return local export source echo cd set shift exit read test true false" },
  bash: { com: "#", str: ['"', "'"],
    kw: "if then else elif fi for while in do done case esac function return local export source echo cd set shift exit read test true false" },
  md: { str: ["`"], heading: true },
};

const HL_EXT = {
  rs: "rs", toml: "toml", json: "json", py: "py", md: "md", markdown: "md",
  js: "js", mjs: "js", cjs: "js", jsx: "js",
  ts: "ts", mts: "ts", cts: "ts", tsx: "ts",
  sh: "sh", bash: "bash",
  yml: null, yaml: null,
};

const HL_NUM = /0[xXbBoO][0-9a-fA-F_]+|\d[\d_]*(\.\d[\d_]*)?([eE][+-]?\d+)?/y;
const HL_IDENT = /[A-Za-z_$][\w$-]*/y;

/** Returns highlighted HTML for code, or null if the language is unknown. */
function highlight(code, path) {
  try {
    const ext = (path.match(/\.([A-Za-z0-9]+)$/) || [])[1];
    const lang = ext ? HL_EXT[ext.toLowerCase()] : null;
    const spec = lang ? HL_LANGS[lang] : null;
    if (!spec) return null;
    const kws = new Set((spec.kw || "").split(" ").filter(Boolean));
    let out = "", i = 0;
    const n = code.length;
    while (i < n) {
      // markdown headings: whole line
      if (spec.heading && (i === 0 || code[i - 1] === "\n")) {
        const m = /^#{1,6} .*/.exec(code.slice(i, code.indexOf("\n", i) < 0 ? n : code.indexOf("\n", i)));
        if (m) {
          out += '<span class="tok-kw">' + esc(m[0]) + "</span>";
          i += m[0].length;
          continue;
        }
      }
      // line comment
      if (spec.com && code.startsWith(spec.com, i)) {
        let j = code.indexOf("\n", i);
        if (j < 0) j = n;
        out += '<span class="tok-com">' + esc(code.slice(i, j)) + "</span>";
        i = j;
        continue;
      }
      // block comment
      if (spec.blk && code.startsWith(spec.blk[0], i)) {
        let j = code.indexOf(spec.blk[1], i + spec.blk[0].length);
        j = j < 0 ? n : j + spec.blk[1].length;
        out += '<span class="tok-com">' + esc(code.slice(i, j)) + "</span>";
        i = j;
        continue;
      }
      // string
      if (spec.str && spec.str.includes(code[i])) {
        const q = code[i];
        let j = i + 1;
        while (j < n) {
          if (code[j] === "\\" && !(spec.rawStr && j > i && code[j - 1] === "\\")) { j += 2; continue; }
          if (code[j] === q) { j++; break; }
          if (q !== "`" && code[j] === "\n") break;
          j++;
        }
        out += '<span class="tok-str">' + esc(code.slice(i, Math.min(j, n))) + "</span>";
        i = Math.min(j, n);
        continue;
      }
      // number
      HL_NUM.lastIndex = i;
      const nm = HL_NUM.exec(code);
      if (nm) {
        out += '<span class="tok-num">' + esc(nm[0]) + "</span>";
        i += nm[0].length;
        continue;
      }
      // identifier / keyword
      HL_IDENT.lastIndex = i;
      const im = HL_IDENT.exec(code);
      if (im) {
        out += kws.has(im[0])
          ? '<span class="tok-kw">' + esc(im[0]) + "</span>"
          : esc(im[0]);
        i += im[0].length;
        continue;
      }
      out += esc(code[i]);
      i++;
    }
    return out;
  } catch (_) {
    return null; // any tokenizer failure → plain text
  }
}

/* ================= 4. dom helpers & components ================= */

/**
 * el("div", {class: "x", title: "t", dataset: {cs: "..."},
 *        onclick: fn, html: "<safe string>"}, ...children)
 * Strings become text nodes; null/undefined children are skipped.
 */
function el(tag, attrs) {
  const node = document.createElement(tag);
  if (attrs) {
    for (const k in attrs) {
      const v = attrs[k];
      if (v === null || v === undefined) continue;
      if (k === "class") node.className = v;
      else if (k === "html") node.innerHTML = v;
      else if (k === "value") node.value = v;
      else if (k === "dataset") { for (const d in v) node.setAttribute("data-" + d, v[d]); }
      else if (k.slice(0, 2) === "on" && typeof v === "function") node.addEventListener(k.slice(2), v);
      else node.setAttribute(k, v);
    }
  }
  for (let i = 2; i < arguments.length; i++) {
    const c = arguments[i];
    if (c === null || c === undefined || c === false) continue;
    node.appendChild(typeof c === "string" || typeof c === "number"
      ? document.createTextNode(String(c)) : c);
  }
  return node;
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
  return node;
}

function skeleton(rows) {
  const wrap = el("div", { class: "panel", "aria-busy": "true" });
  for (let i = 0; i < (rows || 6); i++) {
    const w = 30 + ((i * 37) % 60);
    wrap.appendChild(el("div", { class: "skeleton skel-row", style: "width:" + w + "%" }));
  }
  return wrap;
}

const EMPTY_GLYPH = '<svg width="38" height="38" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.2" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 7v6" stroke-linecap="round"/><circle cx="12" cy="16.2" r="0.6" fill="currentColor" stroke="none"/></svg>';

function emptyState(title, msg, hint) {
  const box = el("div", { class: "empty-state" },
    el("div", { class: "empty-glyph", html: EMPTY_GLYPH }),
    el("div", { class: "empty-title" }, title),
    msg ? el("div", { class: "empty-msg" }, msg) : null);
  if (hint) box.appendChild(el("div", { class: "empty-hint" }, hint));
  return box;
}

function mountError(root, err, title) {
  clear(root).appendChild(emptyState(title || "Something went wrong", String(err && err.message ? err.message : err)));
}

/** Clipboard write with a textarea fallback; rejects on hard failure. */
function copyText(text) {
  if (typeof navigator !== "undefined" && navigator.clipboard && navigator.clipboard.writeText) {
    return navigator.clipboard.writeText(text);
  }
  return new Promise((resolve, reject) => {
    const body = document.body || document.documentElement;
    const ta = document.createElement("textarea");
    ta.value = text;
    body.appendChild(ta);
    if (ta.select) ta.select();
    let ok = false;
    try { ok = document.execCommand ? document.execCommand("copy") : false; } catch (_) { ok = false; }
    if (ta.parentNode) ta.parentNode.removeChild(ta);
    ok ? resolve() : reject(new Error("copy command rejected"));
  });
}

function copyButton(getText, label) {
  const btn = el("button", { class: "btn btn-sm", type: "button" }, label || "copy");
  btn.addEventListener("click", async () => {
    const orig = btn.textContent;
    try {
      const text = await Promise.resolve(typeof getText === "function" ? getText() : getText);
      await copyText(text);
      btn.textContent = "copied";
    } catch (_) {
      btn.textContent = "copy failed";
    }
    setTimeout(() => { btn.textContent = orig; }, 1200);
  });
  return btn;
}

function flashBtn(btn, label, restore) {
  const orig = btn.textContent;
  btn.textContent = label;
  setTimeout(() => { btn.textContent = restore || orig; }, 1200);
}

/** Copy a permalink to the current view. */
function permalinkButton(title) {
  const btn = el("button", {
    class: "btn btn-sm permalink-btn", type: "button",
    title: title || "copy link to this view",
  }, "link");
  btn.addEventListener("click", async () => {
    try {
      const href = typeof location !== "undefined" && location.href
        ? String(location.href) : "#" + String(location.hash || "");
      await copyText(href);
      flashBtn(btn, "copied", "link");
    } catch (_) {
      flashBtn(btn, "copy failed", "link");
    }
  });
  return btn;
}

const OP_META = {
  add:    { letter: "A", cls: "add",    label: "Added" },
  modify: { letter: "M", cls: "modify", label: "Modified" },
  delete: { letter: "D", cls: "delete", label: "Deleted" },
  rename: { letter: "R", cls: "rename", label: "Renamed" },
};

/**
 * Hook-required change list, shared by changeset & compare views. Each row
 * grows a diff expander; below the row sits a lazily-filled unified diff
 * block (old = oldRev @ src_path|path, new = newRev @ path).
 */
function changeList(changes, newRev, oldRev) {
  const groups = { add: [], modify: [], delete: [], rename: [], other: [] };
  for (const c of changes || []) (groups[c.op] || groups.other).push(c);
  const wrap = el("div", { class: "change-list" });
  const ctx = { newRev: newRev, oldRev: oldRev };
  const jobs = [];
  const order = ["add", "modify", "delete", "rename", "other"];
  for (const op of order) {
    const list = groups[op];
    if (!list.length) continue;
    const meta = OP_META[op] || { letter: "T", cls: "other", label: "Changed" };
    wrap.appendChild(el("div", { class: "op-group-title" }, meta.label + " · " + list.length));
    for (const c of list) {
      const row = el("div", { class: "change-row", dataset: { op: c.op || "other" } });
      let job = null;
      if (c.mode !== "gitlink") {
        const exp = el("button", {
          class: "diff-expander", type: "button",
          "aria-expanded": "false", "aria-label": "show diff",
        }, "▸");
        const block = el("div", { class: "diff-block", hidden: "" });
        const metaEl = el("span", { class: "diff-meta mono" });
        job = { exp: exp, block: block, meta: metaEl, change: c, loaded: false, loading: null, open: false, wantOpen: false };
        exp.addEventListener("click", () => {
          if (job.loaded) setDiffOpen(job, !job.open);
          else loadDiff(job, ctx, false); // force-open after load
        });
        row.appendChild(exp);
      }
      row.appendChild(el("span", { class: "change-badge " + meta.cls }, meta.letter));
      if (c.op === "rename" && c.src_path) {
        row.appendChild(el("span", { class: "change-path" },
          el("span", { class: "change-src" }, c.src_path),
          el("span", { class: "change-arrow" }, " → "),
          c.path));
      } else {
        row.appendChild(el("span", { class: "change-path" }, c.path));
      }
      if (c.mode && c.mode !== "regular") {
        row.appendChild(el("span", { class: "mode-chip" }, c.mode));
      }
      if (job) {
        row.appendChild(job.meta);
        row.appendChild(el("span", { class: "spacer" }));
      }
      if ((c.op === "add" || c.op === "modify" || c.op === "rename") &&
          c.mode !== "gitlink" && newRev) {
        row.appendChild(el("a", { class: "view-file", href: fileHref(newRev, c.path) }, "view file"));
      }
      wrap.appendChild(row);
      if (job) {
        wrap.appendChild(job.block);
        jobs.push(job);
      }
    }
  }
  if (!wrap.firstChild) wrap.appendChild(emptyState("No changes", null, "this diff is empty"));
  // eager load decides auto-expand (≤ DIFF_AUTO_MAX lines) vs collapsed
  for (const job of jobs) loadDiff(job, ctx, true);
  return wrap;
}

function personBlock(role, idn) {
  if (!idn) return null;
  return el("div", { class: "person" },
    el("div", { class: "person-role" }, role),
    el("div", null,
      el("span", { class: "person-name" }, idn.name || "—"),
      " ",
      el("span", { class: "person-mail" }, idn.email ? "<" + idn.email + ">" : "")),
    el("div", { class: "person-date" }, civilTime(idn)));
}

function logRow(e) {
  const row = el("div", { class: "log-row" });
  row.setAttribute("data-cs", e.id || "");
  const head = el("div", { class: "log-head" },
    el("a", { class: "log-id mono", href: "#/changeset/" + encodeURIComponent(e.id || "") }, shortId(e.id)),
    el("span", { class: "log-msg" }, firstLine(e.message)));
  if (e.parents && e.parents.length > 1) head.appendChild(el("span", { class: "badge-merge" }, "merge"));
  row.appendChild(head);
  const rest = restLines(e.message);
  if (rest) row.appendChild(el("div", { class: "log-msg-rest" }, rest));
  const when = e.author && typeof e.author.time_secs === "number" ? e.author.time_secs : null;
  const meta = el("div", { class: "log-meta" });
  meta.appendChild(document.createTextNode((e.author && e.author.name) || "unknown"));
  if (when !== null) {
    meta.appendChild(document.createTextNode(" · "));
    meta.appendChild(el("span", { class: "log-time", title: civilTime(e.author) }, relativeTime(when)));
  }
  row.appendChild(meta);
  return row;
}

function breadcrumb(rev, path, finalFile) {
  const nav = el("nav", { id: "breadcrumbs", "aria-label": "Path" });
  nav.appendChild(el("a", { href: browseHref(rev, "") }, revName(rev)));
  const segs = path ? path.split("/") : [];
  let acc = "";
  segs.forEach((seg, idx) => {
    nav.appendChild(el("span", { class: "crumb-sep" }, " / "));
    const isLast = idx === segs.length - 1;
    acc = acc ? acc + "/" + seg : seg;
    const isFile = finalFile && isLast;
    if (isFile) {
      nav.appendChild(el("span", { class: "crumb-here" }, seg));
    } else {
      nav.appendChild(el("a", { href: browseHref(rev, acc) }, seg));
    }
  });
  return nav;
}

function revName(rev) {
  const r = state.refs.find((x) => x.name === rev);
  return r ? rev.replace(/^refs\/heads\//, "") : shortId(rev, 12);
}

/* ---------- agent hand-off modal ---------- */

/** Only one dialog at a time; `modalClose` is set while open. */
let modalClose = null;

/** Host for clone/--addr hints: strip port; wildcards fall back to the web host. */
function pickHost(addr) {
  if (addr && !/^0\.0\.0\.0/.test(addr) && !/^\[::\]/.test(addr)) {
    return String(addr).replace(/:\d+$/, "");
  }
  return (typeof location !== "undefined" && location.hostname) || "localhost";
}

/** The markdown prompt an operator can paste into any agent. */
function buildAgentPrompt(info, refs) {
  info = info || {};
  refs = refs || [];
  const L = [];
  const mainRef = refs.find((r) => r.name === "refs/heads/main") || refs[0];
  const host = pickHost(info.addr);
  const root = info.root || "(unknown)";
  const origin = (typeof location !== "undefined" && location.origin) || "(this web UI)";
  L.push(
    "# Working with this gardenbed repository", "",
    "Gardenbed is source control built for agents: one central server owns the repository;",
    "workers keep SPARSE working copies — you fetch only the paths you need. The unit of",
    "work is a changeset; publishing fast-forwards main.", "",
    "## Repository facts", "",
    "- main tip: " + (mainRef ? mainRef.cs : "(no refs yet)"),
    "- refs:");
  if (!refs.length) L.push("  - (none yet)");
  for (const r of refs) L.push("  - " + r.name + "  " + String(r.cs || "").slice(0, 12));
  L.push(
    "- web UI: " + origin + "; web server version " + (info.version || "(unknown)") +
      "; repo root on the server machine: " + root, "",
    "## Option A — MCP (queries only, no checkout)", "",
    "If your agent host supports MCP servers, register:", "",
    "    gardenbed-mcp --root " + (info.root || "(the repo root from your operator)"), "",
    "Gives you log / diff / file / blame tools with zero checkout.", "",
    "## Option B — bed CLI (full read/write)", "",
    "Ask your operator for the gRPC endpoint (or start it on the repo machine:",
    "`gardenbed-server --root " + root + " --addr " + host + ":9418`). Then:", "",
    "    bed clone http://" + host + ":9418        # creates .bed, fetches nothing",
    "    bed sparse add crates                   # materialize only what you need",
    "    bed status",
    "    GARDENBED_AUTHOR=\"Name <email>\" bed commit -m \"...\"",
    "    bed publish                             # fast-forward main; if refused: bed rebase main, then publish again", "",
    "## Rules", "",
    "- History queries without checkout: bed log / bed diff / bed file / bed blame accept refs,",
    "  full 64-hex ids, or unambiguous prefixes.",
    "- publish never merges for you; on conflict rebase and republish — your changeset is kept.",
    "- Only commit inside a sparse working copy after `bed sparse add`-ing the paths you touch.", "",
    "Full docs: docs/quickstart.md and docs/cli.md in the gardenbed repository.");
  return L.join("\n");
}

function openAgentModal() {
  if (modalClose) return;
  const trigger = document.getElementById("agent-prompt-btn");
  if (trigger) trigger.disabled = true;

  const pre = el("pre", { id: "agent-prompt-text" }, "gathering repository facts…");
  const closeBtn = el("button", { class: "btn btn-sm modal-close", type: "button", "aria-label": "Close dialog" }, "✕");
  const copyBtn = el("button", { id: "copy-prompt-btn", class: "btn primary", type: "button" }, "Copy prompt");
  const modal = el("div", {
    id: "agent-modal", role: "dialog", "aria-modal": "true",
    "aria-labelledby": "agent-modal-title", tabindex: "-1",
  },
    el("div", { class: "modal-head" },
      el("h2", { id: "agent-modal-title" }, "Hand this repo to an agent"),
      closeBtn),
    el("p", { class: "modal-intro" },
      "gardenbed is source control for many agents working one repo. The prompt below tells any agent how to connect, check out only what it needs, and publish — copy it into your agent's instructions."),
    pre,
    el("div", { class: "modal-actions" }, copyBtn));
  const backdrop = el("div", { class: "modal-backdrop" }, modal);

  const onKey = (ev) => {
    if (ev.key === "Escape") { if (ev.preventDefault) ev.preventDefault(); close(); }
  };
  const close = () => {
    if (!modalClose) return;
    modalClose = null;
    if (document.removeEventListener) document.removeEventListener("keydown", onKey);
    if (backdrop.parentNode) backdrop.parentNode.removeChild(backdrop);
    if (trigger) { trigger.disabled = false; if (trigger.focus) trigger.focus(); }
  };
  backdrop.addEventListener("click", (ev) => { if (ev.target === backdrop) close(); });
  closeBtn.addEventListener("click", close);
  copyBtn.addEventListener("click", async () => {
    const orig = copyBtn.textContent;
    try {
      await copyText(pre.textContent || "");
      copyBtn.textContent = "Copied ✓";
      pre.className = "copied";
    } catch (_) {
      copyBtn.textContent = "Copy failed — select and copy manually";
    }
    setTimeout(() => { copyBtn.textContent = orig; pre.className = ""; }, 2000);
  });

  if (document.addEventListener) document.addEventListener("keydown", onKey);
  (document.body || document.documentElement).appendChild(backdrop);
  modalClose = close;
  if (modal.focus) modal.focus();

  const refsP = state.refs.length
    ? Promise.resolve(state.refs)
    : api("/api/refs").then((d) => (d && d.refs) || []).catch(() => []);
  Promise.all([api("/api/server-info").catch(() => ({})), refsP]).then(([info, refs]) => {
    pre.textContent = buildAgentPrompt(info, refs);
  });
}

/* ---------- commit graph (pure SVG lanes) ---------- */

const TL_ROW_H = 68;   // must match --tl-row-h in styles.css
const GRAPH_W = 120, LANE_X0 = 14, LANE_W = 16;

function laneColor(i) {
  return i === 0 ? "#8ec07c" : "hsl(" + ((140 + i * 47) % 360) + ",40%,53%)";
}

function svgEl(tag, attrs) {
  const n = document.createElementNS("http://www.w3.org/2000/svg", tag);
  if (attrs) for (const k in attrs) n.setAttribute(k, attrs[k]);
  return n;
}

/**
 * Lane assignment, newest→oldest: a commit consumes the first lane expecting
 * it (else takes the leftmost free lane); its first parent continues that
 * lane, extra parents open right-side lanes. Edges are drawn at consumption
 * time from the lane's origin node; open lanes get a dangling stub.
 */
function buildCommitGraph(entries) {
  const H = Math.max(entries.length * TL_ROW_H, TL_ROW_H);
  const svg = svgEl("svg", {
    class: "commit-graph", width: GRAPH_W, height: H,
    viewBox: "0 0 " + GRAPH_W + " " + H, "aria-hidden": "true",
  });
  const X = (lane) => LANE_X0 + lane * LANE_W;
  const Y = (row) => row * TL_ROW_H + TL_ROW_H / 2;
  const lanes = []; // {expect, origin:{row,lane}, color}
  const edges = [];
  const nodes = [];

  entries.forEach((e, i) => {
    const parents = e.parents || [];
    let li = lanes.findIndex((l) => l.expect === e.id);
    if (li < 0) {
      li = lanes.findIndex((l) => !l.expect);
      if (li < 0) { li = lanes.length; lanes.push({ color: laneColor(li) }); }
      lanes[li].origin = null;
    }
    const lane = lanes[li];
    if (lane.origin) {
      edges.push({ x1: X(lane.origin.lane), y1: Y(lane.origin.row), x2: X(li), y2: Y(i), c: lane.color });
    }
    nodes.push({ x: X(li), y: Y(i), c: lane.color, merge: parents.length > 1 });
    lane.origin = { row: i, lane: li };
    lane.expect = parents.length ? parents[0] : null;
    for (let pi = 1; pi < parents.length; pi++) {
      let ni = lanes.findIndex((l, idx) => idx > li && !l.expect);
      if (ni < 0) { ni = lanes.length; lanes.push({ color: laneColor(ni) }); }
      lanes[ni].expect = parents[pi];
      lanes[ni].origin = { row: i, lane: li };
    }
  });
  lanes.forEach((l, li) => {
    if (l.expect && l.origin) {
      edges.push({ x1: X(l.origin.lane), y1: Y(l.origin.row), x2: X(li), y2: H - 6, c: l.color, stub: true });
    }
  });

  for (const g of edges) {
    const d = g.stub && g.x1 === g.x2
      ? "M " + g.x1 + " " + g.y1 + " L " + g.x2 + " " + g.y2
      : g.x1 === g.x2
        ? "M " + g.x1 + " " + g.y1 + " L " + g.x2 + " " + g.y2
        : "M " + g.x1 + " " + g.y1 + " C " + g.x1 + " " + (g.y1 + 26) + ", " + g.x2 + " " + (g.y2 - 26) + ", " + g.x2 + " " + g.y2;
    svg.appendChild(svgEl("path", {
      class: "graph-edge", d: d, fill: "none", stroke: g.c,
      "stroke-width": "2", "stroke-linecap": "round", opacity: g.stub ? "0.45" : "0.85",
    }));
  }
  for (const nd of nodes) {
    if (nd.merge) {
      svg.appendChild(svgEl("circle", {
        class: "merge-node", cx: nd.x, cy: nd.y, r: 8.5,
        fill: "none", stroke: nd.c, "stroke-width": "1.6",
      }));
    }
    svg.appendChild(svgEl("circle", {
      class: "graph-node", cx: nd.x, cy: nd.y, r: 5,
      fill: nd.c, stroke: "var(--bg-raised)", "stroke-width": "1.5",
    }));
  }
  return svg;
}

/* ---------- activity heatmap & authors chart ---------- */

function civilDateKey(idn) {
  return civilTime(idn).split(" ")[0];
}

function buildHeatmap(entries) {
  const DAY = 86400;
  const counts = {};
  let max = 0;
  let newestKey = null;
  for (const e of entries) {
    if (!e.author) continue;
    const k = civilDateKey(e.author);
    if (!k) continue;
    counts[k] = (counts[k] || 0) + 1;
    if (counts[k] > max) max = counts[k];
    if (!newestKey || k > newestKey) newestKey = k;
  }
  // Anchor the 12-week window to the newest changeset's own civil date, so a
  // stale repo shows its real activity window; empty log falls back to today.
  const endIdx = newestKey
    ? Math.floor(Date.parse(newestKey + "T00:00:00Z") / (DAY * 1000))
    : Math.floor(Math.floor(Date.now() / 1000) / DAY);
  let start = endIdx - 83;
  start -= new Date(start * DAY * 1000).getUTCDay(); // align first column to Sunday
  const wrap = el("div", { id: "activity-heatmap", class: "mono" });
  for (let d = start; d <= endIdx; d++) {
    const key = new Date(d * DAY * 1000).toISOString().slice(0, 10);
    const c = counts[key] || 0;
    const level = c === 0 ? 0 : Math.min(4, Math.ceil((c / max) * 4));
    wrap.appendChild(el("span", {
      class: "heat-cell",
      "data-level": String(level),
      "data-date": key,
      "data-count": String(c),
      title: c + " changesets on " + key,
    }));
  }
  return wrap;
}

function buildAuthors(entries, top) {
  const counts = {};
  for (const e of entries) {
    const name = (e.author && e.author.name) || "unknown";
    counts[name] = (counts[name] || 0) + 1;
  }
  let rows = Object.keys(counts).map((k) => [k, counts[k]]).sort((a, b) => b[1] - a[1]);
  const rest = rows.slice(top || 8);
  rows = rows.slice(0, top || 8);
  if (rest.length) rows.push(["others", rest.reduce((a, r) => a + r[1], 0)]);
  const maxN = rows.length ? rows[0][1] : 1;
  const wrap = el("div", { id: "authors-chart" });
  if (!rows.length) {
    wrap.appendChild(emptyState("No authors yet", null, "changesets will attribute their authors here"));
    return wrap;
  }
  for (const [name, n] of rows) {
    const pct = Math.max(2, Math.round((n / maxN) * 100));
    wrap.appendChild(el("div", { class: "author-row" },
      el("span", { class: "author-name", title: name }, name),
      el("span", { class: "author-track" },
        el("span", { class: "author-bar", style: "width:" + pct + "%" })),
      el("span", { class: "author-count mono" }, String(n))));
  }
  return wrap;
}

/* ---------- unified diff (myers, no deps) ---------- */

const DIFF_AUTO_MAX = 80; // auto-expand diffs with at most this many lines

/** Classic Myers diff over line arrays → [{type:'ctx'|'add'|'del', text}]. */
function diffLines(aStr, bStr) {
  const a = aStr ? String(aStr).split("\n") : [];
  const b = bStr ? String(bStr).split("\n") : [];
  const N = a.length, M = b.length;
  if (!N && !M) return [];
  const max = N + M, OFF = max;
  let v = new Int32Array(2 * max + 1);
  const trace = [];
  let found = max;
  outer: for (let d = 0; d <= max; d++) {
    trace.push(v.slice());
    for (let k = -d; k <= d; k += 2) {
      let x = (k === -d || (k !== d && v[OFF + k - 1] < v[OFF + k + 1])) ? v[OFF + k + 1] : v[OFF + k - 1] + 1;
      let y = x - k;
      while (x < N && y < M && a[x] === b[y]) { x++; y++; }
      v[OFF + k] = x;
      if (x >= N && y >= M) { found = d; break outer; }
    }
  }
  const ops = [];
  let x = N, y = M;
  for (let d = found; d > 0; d--) {
    const vv = trace[d];
    const k = x - y;
    const prevK = (k === -d || (k !== d && vv[OFF + k - 1] < vv[OFF + k + 1])) ? k + 1 : k - 1;
    const prevX = vv[OFF + prevK], prevY = prevX - prevK;
    while (x > prevX && y > prevY) { ops.push({ type: "ctx", text: a[x - 1] }); x--; y--; }
    if (x === prevX) { ops.push({ type: "add", text: b[y - 1] }); y--; }
    else { ops.push({ type: "del", text: a[x - 1] }); x--; }
  }
  while (x > 0 && y > 0) { ops.push({ type: "ctx", text: a[x - 1] }); x--; y--; }
  ops.reverse();
  return ops;
}

/** Group ops into hunks with up to 3 context lines; longer runs collapse away. */
function buildHunks(ops) {
  const CTX = 3;
  const keep = ops.map((op, i) => {
    if (op.type !== "ctx") return true;
    for (let j = Math.max(0, i - CTX); j <= Math.min(ops.length - 1, i + CTX); j++) {
      if (ops[j].type !== "ctx") return true;
    }
    return false;
  });
  const hunks = [];
  let on = 1, nn = 1, cur = null;
  let ri = 0; // counts of kept items in current run
  ops.forEach((op, i) => {
    if (keep[i] && (i === 0 || !keep[i - 1])) {
      cur = { oldStart: on, newStart: nn, oldLines: 0, newLines: 0, items: [] };
      hunks.push(cur);
      ri = 0;
    }
    if (keep[i] && cur) {
      cur.items.push({ type: op.type, text: op.text, oldNo: op.type !== "add" ? on : 0, newNo: op.type !== "del" ? nn : 0 });
      ri++;
    }
    if (keep[i] && cur) {
      if (op.type !== "add") cur.oldLines++;
      if (op.type !== "del") cur.newLines++;
    }
    if (op.type !== "add") on++;
    if (op.type !== "del") nn++;
  });
  // git-style: single-line ranges show just the start
  for (const h of hunks) {
    h.header = "@@ -" + h.oldStart + "," + h.oldLines + " +" + h.newStart + "," + h.newLines + " @@";
  }
  return hunks;
}

/* diff job plumbing: one per change row */
function setDiffOpen(job, open) {
  job.open = open;
  job.exp.setAttribute("aria-expanded", String(open));
  job.exp.textContent = open ? "▾" : "▸";
  if (open) job.block.removeAttribute("hidden");
  else job.block.setAttribute("hidden", "");
}

function renderDiffNote(job, text) {
  while (job.block.firstChild) job.block.removeChild(job.block.firstChild);
  job.block.appendChild(el("div", { class: "diff-note" }, text));
  job.noteOnly = true;
}

function renderDiffHunks(job, hunks) {
  while (job.block.firstChild) job.block.removeChild(job.block.firstChild);
  for (const h of hunks) {
    job.block.appendChild(el("div", { class: "diff-hunk" }, h.header));
    for (const it of h.items) {
      const sign = it.type === "add" ? "+" : it.type === "del" ? "-" : " ";
      job.block.appendChild(el("div", { class: "diff-line diff-" + it.type },
        el("span", { class: "dn" }, it.oldNo ? String(it.oldNo) : ""),
        el("span", { class: "dn" }, it.newNo ? String(it.newNo) : ""),
        el("span", { class: "diff-sign" }, sign),
        el("span", { class: "diff-text" }, it.text)));
    }
  }
}

async function loadDiff(job, ctx, auto) {
  if (job.loading) { job.wantOpen = true; return job.loading; }
  if (job.loaded) { if (!auto) setDiffOpen(job, !job.open); return Promise.resolve(); }
  const c = job.change;
  job.meta.textContent = "loading…";
  job.loading = (async () => {
    try {
      const oldPath = c.op === "rename" && c.src_path ? c.src_path : c.path;
      let of = null, nf = null;
      const tasks = [];
      if (ctx.oldRev && c.op !== "add") tasks.push(fetchFileCached(ctx.oldRev, oldPath).then((r) => { of = r; }));
      if (c.op !== "delete") tasks.push(fetchFileCached(ctx.newRev, c.path).then((r) => { nf = r; }));
      await Promise.all(tasks);
      const unusable = (f) => f && (f.encoding !== "utf8" || f.truncated);
      if (unusable(of) || unusable(nf)) {
        renderDiffNote(job, "binary or too large to diff");
      } else {
        const a = of ? String(of.content || "") : "";
        const b = nf ? String(nf.content || "") : "";
        if (c.op === "rename" && a === b) {
          renderDiffNote(job, "content unchanged — path only");
        } else {
          const ops = diffLines(a, b);
          renderDiffHunks(job, buildHunks(ops));
          job.lineCount = ops.length;
          job.meta.textContent = job.lineCount + " line" + (job.lineCount === 1 ? "" : "s");
        }
      }
      job.loaded = true;
      const wantsOpen = auto
        ? (job.lineCount === undefined ? true : job.lineCount <= DIFF_AUTO_MAX)
        : !job.open;
      setDiffOpen(job, wantsOpen);
      if (job.lineCount === undefined) job.meta.textContent = "";
    } catch (err) {
      renderDiffNote(job, "could not load diff: " + (err && err.message ? err.message : err));
      job.loaded = true;
      job.meta.textContent = "";
      setDiffOpen(job, true);
    } finally {
      job.loading = null;
    }
  })();
  return job.loading;
}

/* ---------- live polling (tip watcher) ---------- */

let liveTimer = null;
let lastTips = {};
let toastEl = null;
let toastTimer = null;

function hideToast() {
  if (toastEl && toastEl.parentNode) toastEl.parentNode.removeChild(toastEl);
  toastEl = null;
  if (toastTimer) { clearTimeout(toastTimer); toastTimer = null; }
}

async function showLiveToast(ref, oldCs, newCs) {
  let suffix = "";
  try {
    const log = await api("/api/log?" + qs({ cs: newCs, limit: 50 }));
    let n = 0, found = false;
    for (const e of log.entries || []) { if (e.id === oldCs) { found = true; break; } n++; }
    if (found) suffix = " (+" + n + (n === 1 ? " changeset)" : " changesets)");
  } catch (_) { /* count unavailable — ids alone tell the story */ }
  if (!toastEl) {
    toastEl = el("div", { id: "live-toast", role: "status" });
    (document.body || document.documentElement).appendChild(toastEl);
  } else {
    while (toastEl.firstChild) toastEl.removeChild(toastEl.firstChild); // replace, reset timer
  }
  const refName = ref.name.replace(/^refs\/(heads\/)?/, "");
  toastEl.appendChild(el("span", { class: "toast-text" },
    refName + " moved: ",
    el("span", { class: "mono" }, shortId(oldCs, 8)),
    " → ",
    el("span", { class: "mono" }, shortId(newCs, 8)),
    suffix));
  toastEl.appendChild(el("button", {
    class: "btn btn-sm toast-refresh", type: "button",
    onclick: () => { hideToast(); route(); },
  }, "refresh"));
  toastEl.appendChild(el("button", {
    class: "btn btn-sm toast-close", type: "button", "aria-label": "dismiss",
    onclick: hideToast,
  }, "✕"));
  clearTimeout(toastTimer);
  toastTimer = setTimeout(hideToast, 12000);
}

async function pollTick() {
  if (typeof document !== "undefined" && document.hidden) return; // paused while hidden
  let data;
  try { data = await api("/api/refs"); } catch (_) { return; }
  const fresh = (data && data.refs) || [];
  const hadBaseline = !!Object.keys(lastTips).length;
  const prev = lastTips;
  lastTips = {};
  for (const r of fresh) lastTips[r.name] = r.cs;
  const sel = document.getElementById("ref-select");
  if (sel && state.refsLoaded) {
    const known = new Set(fresh.map((r) => r.name));
    state.refs = fresh;
    while (sel.firstChild) sel.removeChild(sel.firstChild);
    for (const r of fresh) sel.appendChild(option(r.name, r.name));
    if (known.has(state.rev)) sel.value = state.rev;
  }
  if (!hadBaseline) return; // first poll after load only records the baseline
  const moved = fresh.find((r) => r.name === state.rev && prev[r.name] && prev[r.name] !== r.cs);
  if (moved) showLiveToast(moved, prev[moved.name], moved.cs);
}

function startLivePoll() {
  if (typeof setInterval !== "undefined") liveTimer = setInterval(pollTick, 10000);
  if (document.addEventListener) {
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden) pollTick(); // catch up immediately on refocus
    });
  }
}

/* ---------- file finder (fuzzy) ---------- */

let finderFiles = { rev: null, paths: null }; // cached per rev
let overlayDepth = 0; // any modal open → global keys suppressed

/** Subsequence score: contiguous runs > word starts (after / - _ .) > scattered. */
function fuzzyScore(q, name) {
  const ql = q.toLowerCase(), nl = name.toLowerCase();
  let score = 0, qi2 = 0, last = -2, start = 0, first = -1;
  const matched = new Array(name.length).fill(false);
  for (let i = 0; i < nl.length && qi2 < ql.length;) {
    if (nl[i] === ql[qi2]) {
      matched[i] = true;
      if (last === i - 1) score += 8; // contiguous run
      else if (i > 0 && "/-_.".includes(name[i - 1])) score += 5; // word start
      else score += 1;
      if (first < 0) first = i;
      last = i;
      qi2++;
      i++;
    } else i++;
  }
  if (qi2 < ql.length) return null; // not a subsequence
  score -= first * 0.05 + (name.length - ql.length) * 0.01; // early, short names win ties
  void start;
  return { score, matched };
}

function openFileFinder() {
  if (overlayDepth) return;
  const rev = currentRev();
  overlayDepth++;
  const prev = { focus: document.activeElement && document.activeElement.focus ? document.activeElement : null };

  const input = el("input", {
    id: "file-finder-input", class: "input", type: "text",
    placeholder: "type to filter files at " + revName(rev) + "…",
    "aria-label": "filter files",
  });
  const countEl = el("div", { class: "finder-count mono" }, "loading…");
  const listEl = el("div", { class: "finder-list mono" });
  const panel = el("div", {
    id: "file-finder", role: "dialog", "aria-modal": "true",
    "aria-labelledby": "file-finder-title", tabindex: "-1",
  },
    el("div", { class: "modal-head" },
      el("h2", { id: "file-finder-title" }, "Find file"),
      el("button", { class: "btn btn-sm modal-close", type: "button", "aria-label": "Close", onclick: () => close() }, "✕")),
    input, countEl, listEl);
  const backdrop = el("div", { class: "modal-backdrop" }, panel);

  let idx = 0, rows = [];
  const close = () => {
    overlayDepth--;
    if (document.removeEventListener) document.removeEventListener("keydown", onKey);
    if (backdrop.parentNode) backdrop.parentNode.removeChild(backdrop);
    if (prev.focus && prev.focus.focus) prev.focus.focus();
  };
  const selectIdx = (n) => {
    idx = Math.max(0, Math.min(rows.length - 1, n));
    rows.forEach((r, i2) => {
      const kls = "finder-row" + (i2 === idx ? " kbd-selected" : "");
      r.setAttribute("class", kls);
    });
  };
  const render = (all, q) => {
    while (listEl.firstChild) listEl.removeChild(listEl.firstChild);
    rows = [];
    let shown = all;
    let scored = null;
    if (q) {
      scored = [];
      for (const p of all) {
        const s = fuzzyScore(q, p.path);
        if (s) scored.push({ p, s });
      }
      scored.sort((a, b) => b.s.score - a.s.score);
      shown = scored;
    }
    countEl.textContent = shown.length + " of " + all.length + " files";
    for (const it of shown.slice(0, 50)) {
      const p = scored ? it.p : it;
      const a = el("a", { class: "finder-row", href: fileHref(rev, p.path) });
      // highlight matched chars
      if (scored) {
        let buf = "", hl = false;
        const pushSeg = (txt, isHl) => {
          if (!txt) return;
          a.appendChild(isHl ? el("b", {}, txt) : document.createTextNode(txt));
        };
        for (let i2 = 0; i2 < p.path.length; i2++) {
          const m = it.s.matched[i2];
          if (m !== hl) { pushSeg(buf, hl); buf = ""; hl = m; }
          buf += p.path[i2];
        }
        pushSeg(buf, hl);
      } else {
        a.appendChild(document.createTextNode(p.path));
      }
      a.appendChild(el("span", { class: "finder-kind" }, p.kind));
      a.addEventListener("click", () => close());
      listEl.appendChild(a);
      rows.push(a);
    }
    if (!shown.length) listEl.appendChild(el("div", { class: "finder-empty" }, "no matches"));
    selectIdx(0);
  };

  const onKey = (ev) => {
    if (ev.key === "Escape") { ev.preventDefault && ev.preventDefault(); close(); }
    else if (ev.key === "ArrowDown") { ev.preventDefault && ev.preventDefault(); selectIdx(idx + 1); }
    else if (ev.key === "ArrowUp") { ev.preventDefault && ev.preventDefault(); selectIdx(idx - 1); }
    else if (ev.key === "Enter") {
      const a = rows[idx];
      if (a) { ev.preventDefault && ev.preventDefault(); location.hash = a.getAttribute("href"); close(); }
    }
  };
  input.addEventListener("keydown", onKey);
  if (document.addEventListener) document.addEventListener("keydown", onKey);
  input.addEventListener("input", () => {
    if (finderFiles.paths && finderFiles.rev === rev) render(finderFiles.paths, input.value.trim());
  });

  backdrop.addEventListener("click", (ev) => { if (ev.target === backdrop) close(); });
  (document.body || document.documentElement).appendChild(backdrop);
  if (input.focus) input.focus();

  const ensure = finderFiles.rev === rev && finderFiles.paths
    ? Promise.resolve(finderFiles.paths)
    : api("/api/files?" + qs({ cs: rev, limit: 2000 })).then((d) => {
        finderFiles = { rev: rev, paths: (d && d.paths) || [] };
        return finderFiles.paths;
      });
  ensure.then((paths) => render(paths, input.value.trim()))
    .catch((err) => {
      countEl.textContent = "";
      while (listEl.firstChild) listEl.removeChild(listEl.firstChild);
      listEl.appendChild(el("div", { class: "finder-empty" }, "could not list files: " + (err && err.message ? err.message : err)));
    });
}

/* ---------- keyboard navigation + help ---------- */

let kbdSel = -1;

function primaryRows() {
  const root = app();
  if (!root || !root.querySelectorAll) return [];
  for (const sel of ["#search-results .search-line", ".tree-row", ".log-row", ".change-row"]) {
    const rows = root.querySelectorAll(sel);
    if (rows.length) return Array.prototype.slice.call(rows);
  }
  return [];
}

function moveKbdSel(dir, rows) {
  if (!rows.length) return;
  if (kbdSel < 0) kbdSel = 0; else kbdSel = Math.max(0, Math.min(rows.length - 1, kbdSel + dir));
  rows.forEach((r, i) => {
    const has = String(r.className || "").split(" ").includes("kbd-selected");
    if (i === kbdSel && !has) {
      r.setAttribute("class", (String(r.className || "") + " kbd-selected").trim());
      if (r.scrollIntoView) r.scrollIntoView({ block: "nearest" });
    } else if (i !== kbdSel && has) {
      r.setAttribute("class",
        String(r.className || "").split(" ").filter((c) => c && c !== "kbd-selected").join(" "));
    }
  });
}

function openKbdSel(rows) {
  if (kbdSel < 0 || kbdSel >= rows.length) return;
  const row = rows[kbdSel];
  const links = row.querySelectorAll ? row.querySelectorAll("a") : [];
  for (const a of links) {
    const h = a.getAttribute && a.getAttribute("href");
    if (h && h.charAt(0) === "#") { location.hash = h; return; }
  }
}

let kbdHelpEl = null;
function toggleKbdHelp() {
  if (kbdHelpEl) {
    overlayDepth--;
    if (kbdHelpEl.parentNode) kbdHelpEl.parentNode.removeChild(kbdHelpEl);
    kbdHelpEl = null;
    return;
  }
  overlayDepth++;
  const rows = [
    ["j / k", "move selection down / up"],
    ["Enter", "open selected row"],
    ["t", "file finder… fuzzy filter at the selected ref"],
    ["/", "focus the grep box"],
    ["?", "this help"],
    ["Esc", "close dialogs"],
  ];
  const table = el("div", { class: "kbd-table" });
  for (const [k, d] of rows) {
    table.appendChild(el("div", { class: "kbd-row" },
      el("kbd", {}, k), el("span", {}, d)));
  }
  kbdHelpEl = el("div", { class: "modal-backdrop" },
    el("div", { id: "kbd-help", role: "dialog", "aria-modal": "true", tabindex: "-1" },
      el("div", { class: "modal-head" },
        el("h2", null, "Keyboard shortcuts"),
        el("button", { class: "btn btn-sm modal-close", type: "button", "aria-label": "Close", onclick: toggleKbdHelp }, "✕")),
      table));
  kbdHelpEl.addEventListener("click", (ev) => { if (ev.target === kbdHelpEl) toggleKbdHelp(); });
  (document.body || document.documentElement).appendChild(kbdHelpEl);
}

function isFormTarget(t) {
  if (!t) return false;
  const tag = String(t.tagName || "").toUpperCase();
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || !!t.isContentEditable;
}

function onGlobalKey(ev) {
  if (!ev) return;
  if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
  if (isFormTarget(ev.target)) return;
  if (modalClose || overlayDepth) {
    if (ev.key === "Escape" && kbdHelpEl) toggleKbdHelp();
    return;
  }
  const rows = primaryRows();
  switch (ev.key) {
    case "j": moveKbdSel(1, rows); ev.preventDefault && ev.preventDefault(); break;
    case "k": moveKbdSel(-1, rows); ev.preventDefault && ev.preventDefault(); break;
    case "Enter": openKbdSel(rows); break;
    case "t": openFileFinder(); break;
    case "/": {
      const gs = document.getElementById("global-search");
      if (gs && gs.focus) { gs.focus(); ev.preventDefault && ev.preventDefault(); }
      break;
    }
    case "?": toggleKbdHelp(); break;
    default: return;
  }
}

/* ---------- readme panel + minimal markdown renderer ----------
 * Untrusted input: everything user-controlled goes through textContent or
 * esc() (the shared highlighter escapes first); we only ever create our own
 * whitelisted tags via createElement. */

function findReadme(entries) {
  let best = null, bestRank = -1;
  for (const e of entries || []) {
    if (e.kind !== "file") continue;
    const m = /^readme(\.[\w]+)?$/i.exec(e.name);
    if (!m) continue;
    const ext = (m[1] || "").toLowerCase();
    const rank = ext === ".md" ? 3 : ext === ".markdown" ? 2
      : (ext === ".txt" || ext === "") ? 1 : 0;
    if (rank > bestRank) { best = e; bestRank = rank; }
  }
  return best;
}

function isMdName(name) { return /\.(md|markdown)$/i.test(name); }

/** Resolve `href` against the readme's directory; null when it escapes the repo. */
function resolveRepoPath(href, ctx) {
  let h = String(href || "").trim();
  const hi = h.indexOf("#");
  if (hi >= 0) h = h.slice(0, hi);          // in-repo routes don't carry fragments
  if (!h) return null;
  const wasDir = /\/$/.test(h);
  const segs = (h.startsWith("/") ? h : (ctx.dir ? ctx.dir + "/" + h : h)).split("/");
  const out = [];
  for (const s of segs) {
    if (!s || s === ".") continue;
    if (s === "..") { if (!out.length) return null; out.pop(); continue; }
    out.push(s);
  }
  if (!out.length) return null;
  return { path: out.join("/"), isDir: wasDir };
}

/**
 * Classify a link target:
 *   external — https?://, mailto:
 *   anchor   — in-panel #fragments (click intercepted for scroll, no routing)
 *   repo     — relative/absolute paths resolved within the repo
 *   null     — javascript:, data:, vbscript:, //... → .md-badlink
 */
function resolveLink(href, ctx) {
  const h = String(href || "").trim();
  if (!h) return null;
  if (/^(https?:|mailto:)/i.test(h)) return { kind: "external", href: h };
  if (h.startsWith("#")) return { kind: "anchor", href: h };
  if (h.startsWith("//") || /^[a-z][a-z0-9+.-]*:/i.test(h)) return null;
  const rp = resolveRepoPath(h, ctx);
  if (!rp) return null;
  return rp.isDir
    ? { kind: "repo", href: browseHref(ctx.rev, rp.path) }
    : { kind: "repo", href: fileHref(ctx.rev, rp.path) };
}

function b64encode(s) {
  try {
    if (typeof btoa !== "undefined") return btoa(unescape(encodeURIComponent(s)));
  } catch (_) { /* fall through to manual */ }
  const bin = unescape(encodeURIComponent(s));
  const T = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let out = "";
  for (let i = 0; i < bin.length; i += 3) {
    const a = bin.charCodeAt(i), b = i + 1 < bin.length ? bin.charCodeAt(i + 1) : NaN, c = i + 2 < bin.length ? bin.charCodeAt(i + 2) : NaN;
    out += T[a >> 2] + T[((a & 3) << 4) | (b >> 4 || 0)] + (isNaN(b) ? "=" : T[((b & 15) << 2) | (c >> 6 || 0)]) + (isNaN(c) ? "=" : T[c & 63]);
  }
  return out;
}

const MD_IMG_MIME = {
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg",
  gif: "image/gif", webp: "image/webp", svg: "image/svg+xml",
};

function mdImage(alt, src, ctx) {
  const altText = alt || src || "";
  const bad = () => el("em", { class: "md-img-alt" }, "[img: " + altText + "]");
  if (/^https?:\/\//i.test(src)) return el("a", { class: "md-link", href: src }, altText); // never fetch remote
  const rp = resolveRepoPath(src, ctx);
  const ext = rp ? (rp.path.match(/\.([a-z0-9]+)$/i) || [])[1] : null;
  if (!rp || rp.isDir || !ext || !MD_IMG_MIME[ext.toLowerCase()]) return bad();
  const wrap = el("span", { class: "md-img-wrap" });
  const img = el("img", { class: "md-img", alt: altText, loading: "lazy" });
  wrap.appendChild(img);
  api("/api/file?" + qs({ cs: ctx.rev, path: rp.path })).then((f) => {
    if (!f || !f.content) throw new Error("empty");
    const b64 = f.encoding === "base64" ? f.content : b64encode(f.content);
    img.setAttribute("src", "data:" + MD_IMG_MIME[ext.toLowerCase()] + ";base64," + b64);
  }).catch(() => {
    while (wrap.firstChild) wrap.removeChild(wrap.firstChild);
    wrap.appendChild(bad());
  });
  return wrap;
}

function mdLink(text, href, ctx) {
  const r = resolveLink(href, ctx);
  const a = r
    ? el("a", { class: "md-link", href: r.href })
    : el("span", { class: "md-badlink" });
  if (r && r.kind === "anchor") {
    const frag = r.href.slice(1);
    a.addEventListener("click", (ev) => {
      if (ev.preventDefault) ev.preventDefault(); // scroll in-panel, never route
      let target = null;
      try {
        if (ctx.body && /^[a-z0-9-]+$/i.test(frag)) target = ctx.body.querySelectorAll("#" + frag)[0] || null;
      } catch (_) { target = null; }
      if (target && target.scrollIntoView) target.scrollIntoView({ behavior: "smooth", block: "start" });
    });
  }
  mdInlineAppend(a, text, ctx); // bold/`code` inside link text must work
  return a;
}

/** Inline markdown → child nodes of `sink`; only our own tags, text via textContent. */
function mdInlineAppend(sink, text, ctx) {
  let i = 0, buf = "";
  const flush = () => { if (buf) { sink.appendChild(document.createTextNode(buf)); buf = ""; } };
  while (i < text.length) {
    let m;
    if (text[i] === "!" && text[i + 1] === "[") {
      m = /^!\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/.exec(text.slice(i));
      if (m) { flush(); sink.appendChild(mdImage(m[1], m[2], ctx)); i += m[0].length; continue; }
    }
    if (text[i] === "[") {
      m = /^\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/.exec(text.slice(i));
      if (m) { flush(); sink.appendChild(mdLink(m[1], m[2], ctx)); i += m[0].length; continue; }
    }
    const two = text.slice(i, i + 2);
    if (two === "**" || two === "~~") {
      const j = text.indexOf(two, i + 2);
      if (j > i + 2) {
        flush();
        const node = el(two === "**" ? "strong" : "s", {});
        mdInlineAppend(node, text.slice(i + 2, j), ctx);
        sink.appendChild(node);
        i = j + 2;
        continue;
      }
    }
    const c = text[i];
    if ((c === "*" || c === "_") && (i === 0 || !/[\w]/.test(text[i - 1]))) {
      const j = text.indexOf(c, i + 1);
      // word-bounded: opener not after a word char (done above), closer not
      // before whitespace and not followed by a word char (no some_file_name italics)
      if (j > i + 1 && !/\s/.test(text[j - 1]) && (j + 1 >= text.length || !/[\w]/.test(text[j + 1]))) {
        flush();
        const em = el("em", {});
        mdInlineAppend(em, text.slice(i + 1, j), ctx);
        sink.appendChild(em);
        i = j + 1;
        continue;
      }
    }
    if (c === "`") {
      const j = text.indexOf("`", i + 1);
      if (j > i + 1) { flush(); sink.appendChild(el("code", {}, text.slice(i + 1, j))); i = j + 1; continue; }
    }
    m = /^https?:\/\/[^\s()<>"'\]]+/.exec(text.slice(i));
    if (m) { flush(); sink.appendChild(el("a", { class: "md-link", href: m[0] }, m[0])); i += m[0].length; continue; }
    buf += c;
    i++;
  }
  flush();
}

function mdCodeBlock(code, lang) {
  const c = el("code", { class: "code-block" });
  const hl = lang ? highlight(code, "fence." + lang) : null; // highlighter escapes first
  if (hl) c.innerHTML = hl; else c.textContent = code;
  return el("pre", {}, c);
}

function isMdBlockStart(line) {
  return /^```/.test(line) || /^#{1,6}\s/.test(line) ||
    /^\s*(?:-{3,}|\*{3,})\s*$/.test(line) || /^\s*>/.test(line) ||
    /^\s*(?:[-*+]|\d+\.)\s+/.test(line);
}

function mdList(items, ctx) {
  let pos = 0;
  const level = (indent) => {
    const node = el(items[pos].ordered ? "ol" : "ul", {});
    while (pos < items.length && items[pos].indent === indent) {
      const it = items[pos];
      pos++;
      const li = el("li", {});
      mdInlineAppend(li, it.text, ctx);
      node.appendChild(li);
      if (pos < items.length && items[pos].indent > indent) li.appendChild(level(items[pos].indent));
    }
    return node;
  };
  return level(items.length ? items[0].indent : 0);
}

/** Markdown text → block elements; fail-safe: unknown syntax = literal paragraph text. */
function mdBlocks(text, ctx) {
  const lines = String(text == null ? "" : text).replace(/\r\n?/g, "\n").split("\n");
  const usedIds = {};
  const slug = (s) => {
    const base = s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "section";
    let id = base, n = 1;
    while (usedIds[id]) id = base + "-" + (++n);
    usedIds[id] = 1;
    return id;
  };
  const blocks = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }
    let m = /^```(\w*)\s*$/.exec(line);
    if (m) {
      const buf = [];
      let j = i + 1;
      while (j < lines.length && !/^```\s*$/.test(lines[j])) buf.push(lines[j++]);
      blocks.push(mdCodeBlock(buf.join("\n"), m[1]));
      i = j < lines.length ? j + 1 : j;
      continue;
    }
    m = /^(#{1,6})\s+(.+)$/.exec(line);
    if (m) {
      const txt = m[2].replace(/\s*#+\s*$/, "");
      const h = el("h" + m[1].length, { id: slug(txt) });
      mdInlineAppend(h, txt, ctx);
      blocks.push(h);
      i++;
      continue;
    }
    if (/^\s*(?:-{3,}|\*{3,})\s*$/.test(line)) { blocks.push(el("hr", {})); i++; continue; }
    if (/^\s*>/.test(line)) {
      const buf = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) buf.push(lines[i++].replace(/^\s*>\s?/, ""));
      const bq = el("blockquote", {});
      const p = el("p", {});
      mdInlineAppend(p, buf.join(" "), ctx);
      bq.appendChild(p);
      blocks.push(bq);
      continue;
    }
    if (/^\s*(?:[-*+]|\d+\.)\s+/.test(line)) {
      const items = [];
      while (i < lines.length) {
        m = /^(\s*)([-*+]|\d+)\.?\s+(.*)$/.exec(lines[i]);
        if (!m) break;
        items.push({ indent: m[1].length, ordered: /\d/.test(m[2][0]), text: m[3] });
        i++;
      }
      blocks.push(mdList(items, ctx));
      continue;
    }
    const buf = [];
    while (i < lines.length && lines[i].trim() && !isMdBlockStart(lines[i])) buf.push(lines[i++]);
    const p = el("p", {});
    mdInlineAppend(p, buf.join(" "), ctx);
    blocks.push(p);
  }
  return blocks;
}

function buildReadmePanel(f, rev, rpath) {
  const name = rpath.split("/").pop();
  const section = el("section", { id: "readme-panel", class: "panel" },
    el("div", { class: "readme-head" },
      el("span", { class: "readme-icon", html: ICONS.file }),
      el("a", { class: "readme-name mono", href: fileHref(rev, rpath) }, name)));
  let body;
  if (f.encoding === "utf8" && isMdName(name)) {
    const ctx = { dir: rpath.includes("/") ? rpath.slice(0, rpath.lastIndexOf("/")) : "", rev: rev, body: null };
    body = el("div", { class: "md-body" });
    ctx.body = body;
    for (const b of mdBlocks(f.content, ctx)) body.appendChild(b);
  } else {
    body = el("pre", { class: "md-body md-plain" }, f.content || "");
  }
  section.appendChild(body);
  return section;
}

/* ================= 5. views ================= */

const app = () => document.getElementById("app");

function viewOverview(root) {
  document.title = "gardenbed — overview";
  setNav("overview");
  clear(root).appendChild(skeleton(8));
  const rev = currentRev();
  // Refs/stats are independent of history: on an unseeded repo the log
  // fetch 404s ("no ref or changeset matching main") and must not discard
  // the rest of the overview. A non-404 log error still fails the page.
  const logP = api("/api/log?" + qs({ cs: rev, limit: 500 })).catch((err) => {
    if (err && err.status === 404) return { entries: [] };
    throw err;
  });
  Promise.all([
    api("/api/refs"),
    api("/api/stats"),
    logP,
  ]).then(([refs, stats, log]) => {
    clear(root);
    const counters = (stats && stats.counters) || {};
    const totalReq = Object.keys(counters).reduce((a, k) => a + (Number(counters[k]) || 0), 0);
    const tip = (refs.refs || []).find((r) => r.name === rev);

    // hero + chips
    const hero = el("section", { class: "hero" },
      el("h1", null, "one repo, many agents"),
      el("div", { class: "hero-sub" },
        "Every change is a content-addressed changeset. This surface is read-only: history, refs, trees, blame."));
    const chips = el("div", { id: "stat-chips" },
      el("div", { class: "chip" },
        el("div", { class: "chip-label" }, "refs"),
        el("div", { class: "chip-value" }, String((refs.refs || []).length))),
      el("div", { class: "chip" },
        el("div", { class: "chip-label" }, "tip · " + revName(rev)),
        el("div", { class: "chip-value" }, tip ? shortId(tip.cs, 12) : "—")));
    // Repo stats counters bump only inside gRPC handlers, so in a
    // gardenbed-web-only process they read 0 forever; show the chip only
    // when there is something to report (and label the source honestly).
    if (totalReq > 0) {
      chips.appendChild(el("div", { class: "chip" },
        el("div", { class: "chip-label" }, "grpc requests served"),
        el("div", { class: "chip-value" }, String(totalReq))));
    }
    hero.appendChild(chips);
    root.appendChild(hero);

    // ref cards
    const cards = el("div", { id: "refs-cards" });
    for (const r of refs.refs || []) {
      cards.appendChild(el("a", { class: "ref-card", href: "#/changeset/" + encodeURIComponent(r.cs), dataset: { ref: r.name } },
        el("div", { class: "ref-name" }, r.name),
        el("div", { class: "ref-id" }, shortId(r.cs, 12))));
    }
    if ((refs.refs || []).length) root.appendChild(cards);

    // two-column: recent history + stats
    const twoCol = el("div", { class: "two-col" });
    const recent = el("section", { class: "panel" },
      el("h2", { class: "panel-title" }, "Recent · " + revName(rev)));
    const recentList = el("div", { id: "recent-list" });
    const all = log.entries || [];
    if (!all.length) {
      recentList.appendChild(emptyState("No history yet", null, "the first seed has not landed"));
    }
    for (const e of all.slice(0, 15)) recentList.appendChild(logRow(e));
    recent.appendChild(recentList);
    recent.appendChild(el("div", { style: "margin-top:10px" },
      el("a", { class: "btn btn-sm", href: "#/log?cs=" + encodeURIComponent(rev) }, "full history →")));
    twoCol.appendChild(recent);

    const rightCol = el("div", { class: "stack" });
    const authorsPanel = el("section", { class: "panel" },
      el("h2", { class: "panel-title" }, "Authors"));
    authorsPanel.appendChild(buildAuthors(all, 8));
    rightCol.appendChild(authorsPanel);

    const statsPanel = el("section", { class: "panel" },
      el("h2", { class: "panel-title" }, "Request counters"));
    statsPanel.appendChild(statsTable(counters));
    rightCol.appendChild(statsPanel);
    twoCol.appendChild(rightCol);
    root.appendChild(twoCol);

    const activityPanel = el("section", { class: "panel" },
      el("h2", { class: "panel-title" }, "Activity"));
    activityPanel.appendChild(buildHeatmap(all));
    root.appendChild(activityPanel);
  }).catch((err) => mountError(root, err, "Could not load overview"));
}

function statsTable(counters) {
  const wrap = el("div", { id: "stats-table" });
  const keys = Object.keys(counters).sort((a, b) => counters[b] - counters[a]);
  if (!keys.length) {
    wrap.appendChild(emptyState("No counters", null, "the server has not recorded requests yet"));
    return wrap;
  }
  const max = Math.max(1, ...keys.map((k) => Number(counters[k]) || 0));
  for (const k of keys) {
    const v = Number(counters[k]) || 0;
    const pct = Math.max(2, Math.round((v / max) * 100));
    wrap.appendChild(el("div", { class: "stats-row" },
      el("span", { class: "stats-name", title: k }, k),
      el("span", { class: "stats-bar-track" },
        el("span", { class: "stats-bar", style: "width:" + pct + "%" })),
      el("span", { class: "stats-count" }, String(v))));
  }
  return wrap;
}

function viewLog(root, query) {
  const cs = query.get("cs") || currentRev();
  let limit = parseInt(query.get("limit"), 10) || 50;
  document.title = "gardenbed — history";
  setNav("history");

  const load = (lim) => {
    clear(root).appendChild(skeleton(10));
    api("/api/log?" + qs({ cs: cs, limit: lim })).then((data) => {
      clear(root);
      root.appendChild(el("h1", { class: "page-title" }, "History"));
      root.appendChild(el("div", { class: "page-sub" },
        revName(cs) + " · tip " + shortId(data.resolved, 12)));
      const entries = data.entries || [];
      const list = el("div", {
        id: "log-list",
        class: "panel timeline" + (entries.length ? " has-graph" : ""),
      });
      if (!entries.length) {
        list.appendChild(emptyState("No history", null, "this ref has no changesets"));
      }
      for (const e of entries) list.appendChild(logRow(e));
      if (entries.length) list.insertBefore(buildCommitGraph(entries), list.firstChild);
      root.appendChild(list);
      if (data.entries && data.entries.length === lim) {
        root.appendChild(el("button", {
          id: "load-more", class: "btn", type: "button",
          onclick: () => load(lim + 50),
        }, "load more"));
      }
    }).catch((err) => mountError(root, err, "Could not load history"));
  };
  load(limit);
}

function viewChangeset(root, id) {
  document.title = "gardenbed — " + shortId(id, 12);
  setNav(null);
  clear(root).appendChild(skeleton(8));
  api("/api/changeset?" + qs({ id: id })).then((cs) => {
    clear(root);
    document.title = "gardenbed — seed " + shortId(cs.id, 12);
    root.appendChild(el("h1", { class: "page-title" }, "Changeset"));

    const header = el("header", { id: "cs-header", class: "panel" });
    const idRow = el("div", { class: "cs-id-row" },
      el("span", { class: "cs-id" }, cs.id),
      copyButton(() => cs.id, "copy id"),
      permalinkButton("copy link to this changeset"));
    idRow.id = "cs-header-row"; // keep #cs-header on the panel (hook)
    header.appendChild(idRow);
    header.appendChild(el("div", { class: "cs-message" }, cs.message || "(no message)"));
    header.appendChild(el("div", { class: "cs-people" },
      personBlock("author", cs.author),
      personBlock("committer", cs.committer)));
    if (cs.parents && cs.parents.length) {
      const chips = el("div", { class: "parent-chips" });
      chips.appendChild(el("span", { class: "page-sub", style: "margin:4px 8px 0 0" },
        cs.parents.length > 1 ? "parents (merge):" : "parent:"));
      for (const p of cs.parents) {
        chips.appendChild(el("a", { class: "parent-chip", href: "#/changeset/" + encodeURIComponent(p), title: p }, shortId(p, 12)));
      }
      header.appendChild(chips);
    } else {
      header.appendChild(el("div", { class: "page-sub", style: "margin:4px 0 0" }, "root changeset — no parents"));
    }
    // diffstat chips + proportional bar
    const dCounts = { add: 0, modify: 0, delete: 0, rename: 0, other: 0 };
    for (const c of cs.changes || []) {
      if (dCounts[c.op] !== undefined) dCounts[c.op]++; else dCounts.other++;
    }
    const dTotal = dCounts.add + dCounts.modify + dCounts.delete + dCounts.rename + dCounts.other;
    if (dTotal) {
      const SYM = { add: "+", modify: "~", delete: "−", rename: "⇒", other: "?" };
      const LBL = { add: "added", modify: "modified", delete: "deleted", rename: "renamed", other: "changed" };
      const dWrap = el("div", { class: "diffstat" });
      for (const op of ["add", "modify", "delete", "rename", "other"]) {
        if (!dCounts[op]) continue;
        dWrap.appendChild(el("span", { class: "diff-chip mono", "data-op": op },
          SYM[op] + dCounts[op] + " " + LBL[op]));
      }
      header.appendChild(dWrap);
      const bar = el("div", { class: "diff-bar" });
      for (const op of ["add", "modify", "delete", "rename", "other"]) {
        if (!dCounts[op]) continue;
        bar.appendChild(el("span", {
          class: "diff-seg diff-seg-" + op,
          style: "width:" + ((dCounts[op] / dTotal) * 100).toFixed(2) + "%",
        }));
      }
      header.appendChild(bar);
    }
    root.appendChild(header);

    const chPanel = el("section", { class: "panel" });
    chPanel.appendChild(el("h2", { class: "panel-title" },
      "Changes · " + ((cs.changes || []).length)));
    chPanel.appendChild(changeList(cs.changes || [], cs.id, cs.parents && cs.parents[0]));
    root.appendChild(chPanel);

    if (cs.parents && cs.parents.length) {
      root.appendChild(el("div", { style: "margin-top:14px" },
        el("a", { class: "btn btn-sm",
          href: "#/compare?a=" + encodeURIComponent(cs.parents[0]) + "&b=" + encodeURIComponent(cs.id) },
          "compare with first parent →")));
    }
  }).catch((err) => mountError(root, err, "Changeset not found"));
}

const ICONS = {
  directory: '<svg width="13" height="13" viewBox="0 0 16 16"><path d="M1 3h5l2 2h7v8H1z" fill="none" stroke="#8ec07c" stroke-width="1.4" stroke-linejoin="round"/></svg>',
  file: '<svg width="13" height="13" viewBox="0 0 16 16"><path d="M3 1h7l3 3v11H3z" fill="none" stroke="#9a947f" stroke-width="1.4" stroke-linejoin="round"/></svg>',
  symlink: '<svg width="13" height="13" viewBox="0 0 16 16"><path d="M3 1h7l3 3v11H3z" fill="none" stroke="#d8a657" stroke-width="1.4" stroke-linejoin="round"/><path d="M6 11l4-4m0 0H7m3 0v3" fill="none" stroke="#d8a657" stroke-width="1.3"/></svg>',
  gitlink: '<svg width="13" height="13" viewBox="0 0 16 16"><rect x="2" y="2" width="12" height="12" rx="3" fill="none" stroke="#b58ce0" stroke-width="1.4"/><path d="M8 5v6M5 8h6" stroke="#b58ce0" stroke-width="1.3"/></svg>',
};

function viewBrowse(root, rev, path) {
  rev = rev || currentRev();
  path = path || "";
  document.title = "gardenbed — browse " + (path || revName(rev));
  setNav("browse");
  clear(root).appendChild(skeleton(8));
  api("/api/tree?" + qs({ cs: rev, path: path })).then((tree) => {
    clear(root);
    root.appendChild(el("h1", { class: "page-title" }, "Browse"));
    root.appendChild(el("div", { class: "browse-actions" },
      el("button", {
        id: "file-finder-btn", class: "btn btn-sm", type: "button",
        onclick: () => openFileFinder(),
      }, "find file"),
      el("span", { class: "hint" }, "or press t")));
    root.appendChild(breadcrumb(rev, path, false));
    const panel = el("section", { class: "panel", style: "padding:6px 8px" });
    const scroller = el("div", { class: "table-scroll" });
    const table = el("table", { class: "tree-table" });
    const entries = tree.entries || [];
    if (!entries.length) {
      panel.appendChild(emptyState("Empty directory", null, "this tree has no entries"));
    }
    for (const ent of entries) {
      const full = path ? path + "/" + ent.name : ent.name;
      const isDir = ent.kind === "directory";
      const href = isDir ? browseHref(rev, full) : fileHref(rev, full);
      const idShort = typeof ent.id === "string" ? shortId(ent.id, 12) : "";
      const row = el("tr", { class: "tree-row", dataset: { kind: ent.kind, name: ent.name } },
        el("td", { class: "tree-icon", html: ICONS[ent.kind] || ICONS.file }),
        el("td", { class: "tree-name" },
          el("a", { href: href }, ent.name + (isDir ? "/" : ""))),
        el("td", { class: "tree-kind" },
          ent.kind + (ent.mode && ent.mode !== "regular" && ent.kind === "file" ? " · " + ent.mode : "")),
        el("td", { class: "tree-id" }, idShort));
      table.appendChild(row);
    }
    scroller.appendChild(table);
    if (entries.length) panel.appendChild(scroller);
    root.appendChild(panel);
    // README panel below the listing (GitHub layout); silent if it won't read.
    const readme = findReadme(entries);
    if (readme) {
      const rpath = path ? path + "/" + readme.name : readme.name;
      api("/api/file?" + qs({ cs: rev, path: rpath }))
        .then((f) => root.appendChild(buildReadmePanel(f, rev, rpath)))
        .catch(() => {});
    }
  }).catch((err) => {
    mountError(root, err, err && err.status === 400 ? "Not a directory" : "Could not load tree");
  });
}

function viewFile(root, rev, path) {
  rev = rev || currentRev();
  if (!path) {
    setNav("browse");
    document.title = "gardenbed — file";
    clear(root).appendChild(emptyState("No file specified", null, "open files from the browser view"));
    return;
  }
  document.title = "gardenbed — " + path;
  setNav("browse");
  clear(root).appendChild(skeleton(10));
  api("/api/file?" + qs({ cs: rev, path: path })).then((f) => {
    clear(root);

    // header
    const header = el("div", { id: "file-header" });
    header.appendChild(breadcrumb(rev, path, true));
    if (f.mode && f.mode !== "regular") header.appendChild(el("span", { class: "mode-chip" }, f.mode));
    header.appendChild(el("span", { class: "file-size" }, humanSize(f.size)));
    header.appendChild(el("span", { class: "spacer" }));
    header.appendChild(copyButton(() => f.content, "copy content"));
    header.appendChild(permalinkButton("copy link to this file view"));
    let blameBtn = null;
    if (f.encoding === "utf8") {
      blameBtn = el("button", { id: "blame-toggle", class: "btn btn-sm", type: "button",
        "aria-pressed": "false" }, "blame");
      header.appendChild(blameBtn);
    }
    root.appendChild(header);

    // banners
    if (f.truncated) {
      root.appendChild(el("div", { class: "banner warn" },
        "showing first 512 KiB — this file is larger (" + humanSize(f.size) + ")"));
    }
    if (f.mode === "symlink") {
      root.appendChild(el("div", { class: "banner info" }, "symlink → " + f.content));
    }

    // body
    if (f.encoding === "base64" && f.mode !== "symlink") {
      root.appendChild(el("div", { class: "banner info" },
        "binary file (base64, " + (typeof f.size === "number" ? humanSize(f.size) : "?") + ")"));
      root.appendChild(el("div", { class: "code-wrap" },
        el("pre", { class: "b64-body" }, wrap64(f.content))));
      return;
    }

    const lines = String(f.content == null ? "" : f.content).split("\n");
    let hl = f.mode === "symlink" ? null : highlight(f.content, path);
    if (hl !== null && hl.split("\n").length !== lines.length) hl = null; // token crossed a line: bail
    const wrap = el("div", { class: "code-wrap" });
    const codeLines = el("div", { id: "code-lines" });
    const hlLines = hl === null ? null : hl.split("\n");
    lines.forEach((line, i) => {
      const gutter = el("span", { class: "gutter" }, String(i + 1));
      const content = hlLines
        ? el("code", { class: "code-content", html: hlLines[i] === "" ? " " : hlLines[i] })
        : el("code", { class: "code-content" }, line === "" ? " " : line);
      content.setAttribute("style", "white-space:pre");
      codeLines.appendChild(el("div", { class: "code-line" }, gutter, content));
    });
    wrap.appendChild(codeLines);
    root.appendChild(wrap);

    // blame toggle
    if (blameBtn) {
      let blameOn = false;
      let blameData = null;
      blameBtn.addEventListener("click", async () => {
        if (!blameOn && !blameData) {
          blameBtn.disabled = true;
          blameBtn.textContent = "loading…";
          try {
            blameData = await api("/api/blame?" + qs({ cs: rev, path: path }));
          } catch (err) {
            blameBtn.disabled = false;
            blameBtn.textContent = "blame";
            root.insertBefore(el("div", { class: "banner warn" },
              "blame failed: " + (err && err.message ? err.message : err)), wrap);
            return;
          }
          blameBtn.disabled = false;
        }
        blameOn = !blameOn;
        blameBtn.textContent = blameOn ? "hide blame" : "blame";
        blameBtn.setAttribute("aria-pressed", String(blameOn));
        if (blameOn) {
          blameAgeMap(blameData).then((ages) => {
            if (blameOn) applyBlame(codeLines, blameData, lines.length, ages);
          });
        } else {
          applyBlame(codeLines, null, lines.length, null);
        }
      });
    }
  }).catch((err) => {
    mountError(root, err, err && err.status === 404 ? "File not found" : "Could not load file");
  });
}

/**
 * Age heat for blame: rank the file's distinct changesets by committer time
 * and interpolate hue 30 (oldest, warm brown) → 95 (newest, spring green).
 * Missing/failed fetches simply fade out of `out`; callers fall back to the
 * deterministic per-id hash color.
 */
async function blameAgeMap(data) {
  const ids = [];
  for (const r of (data && data.ranges) || []) if (r.cs && !ids.includes(r.cs)) ids.push(r.cs);
  const info = {};
  await Promise.all(ids.map((id) =>
    fetchChangesetCached(id).then((cs) => { info[id] = cs; }).catch(() => {})));
  const ranked = ids
    .filter((id) => info[id])
    .sort((a, b) => commitTime(info[a]) - commitTime(info[b]));
  const out = {};
  ranked.forEach((id, i) => {
    const t = ranked.length > 1 ? i / (ranked.length - 1) : 1;
    const who = info[id].committer || info[id].author || {};
    out[id] = { hue: 30 + t * 65, name: who.name || "", date: civilTime(who) };
  });
  return out;
}

function commitTime(cs) {
  const c = (cs && (cs.committer || cs.author)) || {};
  return typeof c.time_secs === "number" ? c.time_secs : 0;
}

function wrap64(s) {
  return String(s || "").replace(/.{1,76}/g, (m) => m + "\n");
}

/** Paint or clear blame gutter blocks on #code-lines; ageMeta colors by
 *  relative age in this file (see blameAgeMap), absent → per-id hash hue. */
function applyBlame(codeLines, data, totalLines, ageMeta) {
  const rows = [];
  for (let i = 0; i < codeLines.children.length; i++) {
    const c = codeLines.children[i];
    if (c.className && String(c.className).split(" ").includes("code-line")) rows.push(c);
  }
  // reset pass: restore plain numbered gutters
  rows.forEach((row, idx) => {
    const g = row.children[0];
    if (!g) return;
    if (g._blameClick) { g.removeEventListener("click", g._blameClick); g._blameClick = null; }
    g.className = "gutter";
    g.removeAttribute("data-cs");
    g.removeAttribute("title");
    g.removeAttribute("style");
    while (g.firstChild) g.removeChild(g.firstChild);
    g.appendChild(document.createTextNode(String(idx + 1)));
  });
  if (!data) return;
  for (const range of data.ranges || []) {
    const lo = Math.max(1, range.lo || 1);
    const hi = Math.min(totalLines, Math.min(rows.length, range.hi || lo));
    const info = ageMeta && ageMeta[String(range.cs || "")];
    const h = info ? Math.round(info.hue) : idHue(String(range.cs || ""));
    const short = String(range.cs || "").slice(0, 8);
    const style = info
      ? "background:hsl(" + h + ",40%,24%);border-left:3px solid hsl(" + h + ",55%,62%);color:#f0ead8"
      : "background:hsl(" + h + ",30%,26%);border-left:3px solid hsl(" + h + ",45%,52%);color:#f0ead8";
    const title = info
      ? short + " · " + info.name + (info.date ? " · " + info.date : "") +
        " — colored by relative age in this file"
      : String(range.cs || short);
    for (let ln = lo; ln <= hi; ln++) {
      const g = rows[ln - 1] && rows[ln - 1].children[0];
      if (!g) continue;
      g.className = "gutter gutter-block";
      g.setAttribute("data-cs", short);
      g.setAttribute("title", title);
      g.setAttribute("style", style);
      while (g.firstChild) g.removeChild(g.firstChild);
      if (ln === lo) g.appendChild(document.createTextNode(short));
      const target = range.cs;
      const click = () => { location.hash = "#/changeset/" + encodeURIComponent(target); };
      g._blameClick = click;
      g.addEventListener("click", click);
    }
  }
}

function viewCompare(root, query) {
  document.title = "gardenbed — compare";
  setNav("compare");
  const a0 = query.get("a") || "";
  const b0 = query.get("b") || "";
  clear(root);
  root.appendChild(el("h1", { class: "page-title" }, "Compare"));

  const aIn = el("input", { id: "cmp-a", class: "input", type: "text",
    placeholder: "ref, id, or prefix", value: a0, "aria-label": "base revision a" });
  const bIn = el("input", { id: "cmp-b", class: "input", type: "text",
    placeholder: "ref, id, or prefix", value: b0, "aria-label": "target revision b" });
  const run = el("button", { id: "cmp-run", class: "btn primary", type: "submit" }, "Compare");

  const escClear = (ev) => {
    if (ev.key === "Escape") { aIn.value = ""; bIn.value = ""; aIn.focus(); }
  };
  aIn.addEventListener("keydown", escClear);
  bIn.addEventListener("keydown", escClear);

  const form = el("form", {
    class: "cmp-form",
    onsubmit: (ev) => {
      ev.preventDefault();
      location.hash = "#/compare?" + qs({ a: aIn.value.trim(), b: bIn.value.trim() });
    },
  },
    el("div", { class: "cmp-field" }, el("label", { "for": "cmp-a" }, "a — base"), aIn),
    el("div", { class: "cmp-field" }, el("label", { "for": "cmp-b" }, "b — target"), bIn),
    run);
  root.appendChild(form);

  const results = el("div", { id: "cmp-results" });
  root.appendChild(results);

  if (!a0 || !b0) {
    results.appendChild(emptyState("Pick two revisions", null,
      "enter a base (a) and a target (b) — refs, full ids, or hex prefixes"));
    return;
  }

  results.appendChild(skeleton(6));
  api("/api/diff?" + qs({ a: a0, b: b0 })).then((d) => {
    clear(results);
    results.appendChild(el("div", { class: "cmp-head mono" },
      el("span", { title: d.a }, shortId(d.a, 8) + "…"),
      el("span", { class: "cmp-arrow" }, "→"),
      el("span", { title: d.b }, shortId(d.b, 8) + "…"),
      el("span", { class: "cmp-count" }, "  " + ((d.changes || []).length) + " changes")));
    const panel = el("section", { class: "panel" });
    panel.appendChild(changeList(d.changes || [], d.b, d.a));
    results.appendChild(panel);
  }).catch((err) => {
    clear(results).appendChild(emptyState("Compare failed", String(err && err.message ? err.message : err)));
  });
}

/** Wrap case-insensitive occurrences of q in <mark>, untrusted-safe. */
function markMatches(node, text, q) {
  const tl = String(text).toLowerCase(), ql = String(q).toLowerCase();
  if (!ql) { node.textContent = String(text); return node; }
  let i = 0, idx, marked = false;
  while ((idx = tl.indexOf(ql, i)) >= 0) {
    if (idx > i) node.appendChild(document.createTextNode(text.slice(i, idx)));
    node.appendChild(el("mark", {}, text.slice(idx, idx + ql.length)));
    i = idx + ql.length;
    marked = true;
  }
  if (i < text.length) node.appendChild(document.createTextNode(text.slice(i)));
  if (!marked && !i) node.textContent = String(text);
  return node;
}

function viewSearch(root, query) {
  const q0 = (query.get("q") || "").trim();
  const rev = currentRev();
  document.title = "gardenbed — grep " + q0;
  setNav(null);
  clear(root);
  root.appendChild(el("h1", { class: "page-title" }, "Search"));
  const results = el("div", { id: "search-results" });
  root.appendChild(results);
  if (!q0) {
    results.appendChild(emptyState("Nothing to grep", null, "type in the grep box (or press /) and hit Enter"));
    return;
  }
  results.appendChild(skeleton(6));
  api("/api/search?" + qs({ cs: rev, q: q0, limit: 100 })).then((d) => {
    clear(results);
    const files = d.files || [];
    results.appendChild(el("div", { class: "search-head" },
      files.length + " file" + (files.length === 1 ? "" : "s") + " match ",
      el("span", { class: "mono" }, "“" + q0 + "”"),
      " at " + revName(rev) + " · " + shortId(d.cs || "", 8)));
    if (d.truncated) {
      results.appendChild(el("div", { class: "banner warn" }, "result set truncated — showing the server-capped first matches"));
    }
    if (!files.length) {
      results.appendChild(emptyState("No matches", null, 'no matches for "' + q0 + '" — try a shorter substring'));
      return;
    }
    for (const f of files) {
      const matches = (f.matches || []).slice(0, 20);
      const sf = el("div", { class: "search-file panel" },
        el("div", { class: "search-file-head" },
          el("a", { class: "mono", href: fileHref(rev, f.path) }, f.path),
          el("span", { class: "search-count mono" }, matches.length + (matches.length === 1 ? " match" : " matches"))));
      for (const m of matches) {
        sf.appendChild(el("div", { class: "search-line" },
          el("a", { class: "search-ln mono", href: fileHref(rev, f.path) }, String(m.line)),
          markMatches(el("code", { class: "search-text" }), String(m.text == null ? "" : m.text), q0)));
      }
      results.appendChild(sf);
    }
  }).catch((err) => {
    clear(results).appendChild(emptyState("Search failed", String(err && err.message ? err.message : err)));
  });
}

function viewNotFound(root, path) {
  document.title = "gardenbed — 404";
  setNav(null);
  clear(root).appendChild(emptyState(
    "404 — nothing grows here",
    "unknown route: #" + (path || "/"),
    "try Overview, History, Browse, or Compare from the top bar"));
}

/* ================= 6. router & boot ================= */

function setNav(key) {
  const links = document.querySelectorAll
    ? document.querySelectorAll(".nav a")
    : [];
  const list = Array.prototype.slice.call(links);
  for (const a of list) {
    if (a.getAttribute("data-nav") === key) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
}

function parseHash() {
  let h = location.hash || "";
  if (h.charAt(0) === "#") h = h.slice(1);
  const qIdx = h.indexOf("?");
  const path = qIdx >= 0 ? h.slice(0, qIdx) : h;
  const query = new URLSearchParams(qIdx >= 0 ? h.slice(qIdx + 1) : "");
  const segs = path.split("/").filter((s) => s.length > 0).map((s) => {
    try { return decodeURIComponent(s); } catch (_) { return s; }
  });
  return { path: path, segs: segs, query: query };
}

function route() {
  kbdSel = -1; // keyboard selection is per-page
  const root = app();
  const r = parseHash();
  const segs = r.segs;
  if (segs.length === 0) return viewOverview(root);
  switch (segs[0]) {
    case "log":
      return viewLog(root, r.query);
    case "changeset":
      if (!segs[1]) return viewNotFound(root, r.path);
      return viewChangeset(root, segs[1]);
    case "browse":
      return viewBrowse(root, segs[1] || "", segs.slice(2).join("/"));
    case "file":
      return viewFile(root, segs[1] || "", segs.slice(2).join("/"));
    case "compare":
      return viewCompare(root, r.query);
    case "search":
      return viewSearch(root, r.query);
    default:
      return viewNotFound(root, r.path);
  }
}

/* Test/maintainer handle: intentional, stable, undocumented surface. */
const __gb = { pollTick, diffLines, buildHunks, fuzzyScore };
if (typeof window !== "undefined") window.__gb = __gb;

function boot() {
  addEventListener("hashchange", route);
  const agentBtn = document.getElementById("agent-prompt-btn");
  if (agentBtn) agentBtn.addEventListener("click", openAgentModal);
  const gs = document.getElementById("global-search");
  if (gs) {
    gs.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") {
        const v = String(gs.value || "").trim();
        if (v) { location.hash = "#/search?" + qs({ q: v }); ev.preventDefault && ev.preventDefault(); }
      }
    });
  }
  if (document.addEventListener) document.addEventListener("keydown", onGlobalKey);
  // Populate the ref selector, then render (selector load is non-blocking).
  loadRefs().then(route, route);
}

if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    (document.addEventListener || addEventListener)("DOMContentLoaded", boot);
  } else {
    boot();
  }
}
