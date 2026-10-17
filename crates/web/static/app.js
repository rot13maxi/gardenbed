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

function emptyState(title, msg, hint) {
  const box = el("div", { class: "empty-state" },
    el("div", { class: "empty-title" }, title),
    msg ? el("div", { class: "empty-msg" }, msg) : null);
  if (hint) box.appendChild(el("div", { class: "empty-hint" }, hint));
  return box;
}

function mountError(root, err, title) {
  clear(root).appendChild(emptyState(title || "Something went wrong", String(err && err.message ? err.message : err)));
}

function copyButton(getText, label) {
  const btn = el("button", { class: "btn btn-sm", type: "button" }, label || "copy");
  btn.addEventListener("click", async () => {
    const orig = btn.textContent;
    try {
      const text = await Promise.resolve(typeof getText === "function" ? getText() : getText);
      if (typeof navigator !== "undefined" && navigator.clipboard && navigator.clipboard.writeText) {
        await navigator.clipboard.writeText(text);
      } else {
        const ta = document.createElement("textarea");
        ta.value = text;
        document.body.appendChild(ta);
        ta.select();
        document.execCommand("copy");
        document.body.removeChild(ta);
      }
      btn.textContent = "copied";
    } catch (_) {
      btn.textContent = "copy failed";
    }
    setTimeout(() => { btn.textContent = orig; }, 1200);
  });
  return btn;
}

const OP_META = {
  add:    { letter: "A", cls: "add",    label: "Added" },
  modify: { letter: "M", cls: "modify", label: "Modified" },
  delete: { letter: "D", cls: "delete", label: "Deleted" },
  rename: { letter: "R", cls: "rename", label: "Renamed" },
};

/** Hook-required change list, shared by changeset & compare views. */
function changeList(changes, fileRev) {
  const groups = { add: [], modify: [], delete: [], rename: [], other: [] };
  for (const c of changes || []) (groups[c.op] || groups.other).push(c);
  const wrap = el("div", { class: "change-list" });
  const order = ["add", "modify", "delete", "rename", "other"];
  for (const op of order) {
    const list = groups[op];
    if (!list.length) continue;
    const meta = OP_META[op] || { letter: "T", cls: "other", label: "Changed" };
    wrap.appendChild(el("div", { class: "op-group-title" }, meta.label + " · " + list.length));
    for (const c of list) {
      const row = el("div", { class: "change-row", dataset: { op: c.op || "other" } });
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
      if ((c.op === "add" || c.op === "modify" || c.op === "rename") &&
          c.mode !== "gitlink" && fileRev) {
        row.appendChild(el("a", { class: "view-file", href: fileHref(fileRev, c.path) }, "view file"));
      }
      wrap.appendChild(row);
    }
  }
  if (!wrap.firstChild) wrap.appendChild(emptyState("No changes", null, "this diff is empty"));
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

/* ================= 5. views ================= */

const app = () => document.getElementById("app");

function viewOverview(root) {
  document.title = "gardenbed — overview";
  setNav("overview");
  clear(root).appendChild(skeleton(8));
  const rev = currentRev();
  Promise.all([
    api("/api/refs"),
    api("/api/stats"),
    api("/api/log?" + qs({ cs: rev, limit: 15 })),
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
        el("div", { class: "chip-value" }, tip ? shortId(tip.cs, 12) : "—")),
      el("div", { class: "chip" },
        el("div", { class: "chip-label" }, "requests served"),
        el("div", { class: "chip-value" }, String(totalReq))));
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
    if (!log.entries || !log.entries.length) {
      recentList.appendChild(emptyState("No history yet", null, "the first seed has not landed"));
    }
    for (const e of log.entries || []) recentList.appendChild(logRow(e));
    recent.appendChild(recentList);
    recent.appendChild(el("div", { style: "margin-top:10px" },
      el("a", { class: "btn btn-sm", href: "#/log?cs=" + encodeURIComponent(rev) }, "full history →")));
    twoCol.appendChild(recent);

    const statsPanel = el("section", { class: "panel" },
      el("h2", { class: "panel-title" }, "Request counters"));
    statsPanel.appendChild(statsTable(counters));
    twoCol.appendChild(statsPanel);
    root.appendChild(twoCol);
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
      const list = el("div", { id: "log-list", class: "panel timeline" });
      if (!data.entries || !data.entries.length) {
        list.appendChild(emptyState("No history", null, "this ref has no changesets"));
      }
      for (const e of data.entries || []) list.appendChild(logRow(e));
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
      copyButton(() => cs.id, "copy id"));
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
    root.appendChild(header);

    const chPanel = el("section", { class: "panel" });
    chPanel.appendChild(el("h2", { class: "panel-title" },
      "Changes · " + ((cs.changes || []).length)));
    chPanel.appendChild(changeList(cs.changes || [], cs.id));
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
        applyBlame(codeLines, blameOn ? blameData : null, lines.length);
      });
    }
  }).catch((err) => {
    mountError(root, err, err && err.status === 404 ? "File not found" : "Could not load file");
  });
}

function wrap64(s) {
  return String(s || "").replace(/.{1,76}/g, (m) => m + "\n");
}

/** Paint or clear blame gutter blocks on #code-lines. */
function applyBlame(codeLines, data, totalLines) {
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
    const hue = idHue(String(range.cs || ""));
    const short = String(range.cs || "").slice(0, 8);
    for (let ln = lo; ln <= hi; ln++) {
      const g = rows[ln - 1] && rows[ln - 1].children[0];
      if (!g) continue;
      g.className = "gutter gutter-block";
      g.setAttribute("data-cs", short);
      g.setAttribute("title", range.cs || short);
      g.setAttribute("style",
        "background:hsl(" + hue + ",30%,26%);" +
        "border-left:3px solid hsl(" + hue + ",45%,52%);color:#f0ead8");
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
    panel.appendChild(changeList(d.changes || [], d.b));
    results.appendChild(panel);
  }).catch((err) => {
    clear(results).appendChild(emptyState("Compare failed", String(err && err.message ? err.message : err)));
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
    default:
      return viewNotFound(root, r.path);
  }
}

function boot() {
  addEventListener("hashchange", route);
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
