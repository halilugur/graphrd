// GraphRD web UI: a dependency-free single-page app over the repository
// protocol and the GraphRD REST API.
"use strict";

const $ = (sel, root = document) => root.querySelector(sel);
const view = $("#view");

// ------------------------------------------------------------------ dom helpers

function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else if (v === true) el.setAttribute(k, "");
    else el.setAttribute(k, v);
  }
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

const panel = (title, actions, ...body) => h("section", { class: "panel" },
  title || actions ? h("header", { class: "panel-head" }, title ? h("h2", {}, title) : null, actions) : null,
  ...body);

const pageHead = (title, lede, actions) => h("div", { class: "page-head" },
  h("div", {}, h("h1", {}, title), lede ? h("p", { class: "lede" }, lede) : null),
  actions ? h("div", { class: "row" }, actions) : null);

function toast(msg, err = false) {
  const t = h("div", { class: "toast", role: err ? "alert" : "status" }, h("span", { class: "dot" + (err ? " err" : "") }), msg);
  $("#toasts").append(t);
  setTimeout(() => t.remove(), err ? 8000 : 3500);
}

/// In-page dialog. Resolves to the input value (prompt), true (confirm) or null.
function dialog({ title, text, confirm = "OK", danger = false, input = null }) {
  return new Promise((resolve) => {
    const field = input ? h("input", { type: "text", id: "dialog-input", value: input.value || "", placeholder: input.placeholder || "", required: true }) : null;
    const close = (v) => { bd.remove(); resolve(v); };
    const bd = h("div", { class: "backdrop", onclick: (e) => { if (e.target === bd) close(null); } },
      h("form", { class: "dialog", role: "dialog", "aria-modal": "true", "aria-label": title,
        onsubmit: (e) => { e.preventDefault(); close(field ? field.value.trim() || null : true); },
        onkeydown: (e) => { if (e.key === "Escape") close(null); } },
      h("h2", {}, title),
      text ? h("p", {}, text) : null,
      field ? h("label", { class: "field" }, input.label, field) : null,
      h("div", { class: "row end" },
        h("button", { type: "button", class: "ghost", onclick: () => close(null) }, "Cancel"),
        h("button", { type: "submit", class: danger ? "danger" : "primary" }, confirm))));
    document.body.append(bd);
    setTimeout(() => (field || bd.querySelector("button[type=submit]")).focus(), 0);
  });
}

const store = {
  get(k, d) { try { const v = localStorage.getItem(k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch { /* storage unavailable */ } },
};

const fmt = (n) => Number(n || 0).toLocaleString();
const enc = encodeURIComponent;

// ------------------------------------------------------------------ http + auth

const auth = {
  get token() { try { return sessionStorage.getItem("graphrd.token"); } catch { return null; } },
  set token(t) { try { t ? sessionStorage.setItem("graphrd.token", t) : sessionStorage.removeItem("graphrd.token"); } catch { /* ignore */ } },
};

class AuthRequired extends Error {}

async function http(method, url, { body, headers = {}, accept } = {}) {
  if (accept) headers.Accept = accept;
  headers["X-GraphRD-UI"] = "1";
  if (auth.token) headers.Authorization = auth.token;
  const res = await fetch(url, { method, body, headers });
  if (res.status === 401 && !url.startsWith("/rest/login")) {
    auth.token = null;
    showLogin();
    throw new AuthRequired("Sign in to continue.");
  }
  if (!res.ok) {
    const text = await res.text();
    let msg = text;
    try { msg = JSON.parse(text).message || text; } catch { /* plain text */ }
    throw new Error(msg || `${res.status} ${res.statusText}`);
  }
  return res;
}
const getJSON = async (url) => (await http("GET", url, { accept: "application/json" })).json();
const form = (obj) => new URLSearchParams(obj).toString();
const FORM = { "Content-Type": "application/x-www-form-urlencoded" };
const JSONH = { "Content-Type": "application/json" };

async function download(url, filename, accept) {
  try {
    const blob = await (await http("GET", url, { accept })).blob();
    const a = h("a", { href: URL.createObjectURL(blob), download: filename });
    document.body.append(a);
    a.click();
    setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 1000);
  } catch (e) { if (!(e instanceof AuthRequired)) toast(e.message, true); }
}

// ------------------------------------------------------------------ state

const state = { repos: [], repo: store.get("graphrd.repo", ""), namespaces: [] };
const repoUrl = (path = "") => `/repositories/${enc(state.repo)}${path}`;
const restRepo = (path = "", id = state.repo) => `/rest/repositories/${enc(id)}${path}`;

async function loadRepos() {
  state.repos = await getJSON("/rest/repositories");
  if (!state.repos.some((r) => r.id === state.repo)) state.repo = state.repos[0]?.id || "";
  store.set("graphrd.repo", state.repo);
  $("#repo-select").replaceChildren(...(state.repos.length
    ? state.repos.map((r) => h("option", { value: r.id, selected: r.id === state.repo }, r.id))
    : [h("option", { value: "" }, "No repositories")]));
  await Promise.all([loadNamespaces(), refreshRepoStats()]);
}

async function refreshRepoStats() {
  const dl = $("#repo-stats");
  if (!state.repo) { dl.replaceChildren(); return; }
  try {
    const [size, cfg] = await Promise.all([getJSON(restRepo("/size")), getJSON(restRepo())]);
    dl.replaceChildren(
      h("dt", {}, "Explicit"), h("dd", {}, fmt(size.explicit)),
      h("dt", {}, "Inferred"), h("dd", {}, fmt(size.inferred)),
      h("dt", {}, "Ruleset"), h("dd", {}, cfg.params?.ruleset?.value || "empty"));
  } catch { dl.replaceChildren(); }
}

async function loadNamespaces() {
  state.namespaces = [];
  if (!state.repo) return;
  try {
    const data = await (await http("GET", repoUrl("/namespaces"), { accept: "application/sparql-results+json" })).json();
    state.namespaces = data.results.bindings.map((b) => [b.prefix.value, b.namespace.value]).sort((a, b) => b[1].length - a[1].length);
  } catch { /* repository may be gone */ }
}

const prefixHeader = () => state.namespaces.slice().sort((a, b) => a[0].localeCompare(b[0])).map(([p, n]) => `PREFIX ${p}: <${n}>`).join("\n");

// ------------------------------------------------------------------ RDF terms

function abbreviate(iri) {
  for (const [p, n] of state.namespaces) {
    if (iri.startsWith(n) && /^[\w\-.]*$/.test(iri.slice(n.length))) return `${p}:${iri.slice(n.length)}`;
  }
  return `<${iri}>`;
}

/// role: "subj" | "pred" | "obj" — colours IRIs by their position in a triple.
function termNode(t, role = "obj") {
  if (!t) return h("span", { class: "t muted" }, "—");
  if (t.type === "triple") {
    const v = t.value;
    return h("span", { class: "t quoted" }, "<< ", termNode(v.subject, "subj"), " ", termNode(v.predicate, "pred"), " ", termNode(v.object), " >>");
  }
  if (t.type === "uri") {
    return h("a", { class: `t iri ${role}`, href: `#/explore?iri=${enc(t.value)}`, title: t.value }, abbreviate(t.value));
  }
  if (t.type === "bnode") return h("span", { class: "t bnode" }, `_:${t.value}`);
  const tag = t["xml:lang"] ? `@${t["xml:lang"]}` : t.datatype ? `^^${abbreviate(t.datatype)}` : "";
  return h("span", { class: "t lit" }, `"${t.value}"`, tag ? h("span", { class: "dt" }, tag) : null);
}

const termKey = (t) => (!t ? "" : t.type === "triple"
  ? `triple|${termKey(t.value.subject)}|${termKey(t.value.predicate)}|${termKey(t.value.object)}`
  : `${t.type}|${t.value}|${t.datatype || ""}|${t["xml:lang"] || ""}`);

function sparqlTerm(t) {
  if (t.type === "uri") return `<${t.value}>`;
  if (t.type === "bnode") return `_:${t.value}`;
  const v = JSON.stringify(t.value);
  return t["xml:lang"] ? `${v}@${t["xml:lang"]}` : t.datatype ? `${v}^^<${t.datatype}>` : v;
}

/// Parse canonical N-Triples/N-Quads (as GraphRD writes them) into term objects.
function parseNT(text) {
  const out = [];
  const re = /<([^>]*)>|_:(\S+)|"((?:[^"\\]|\\.)*)"(?:@([\w-]+)|\^\^<([^>]*)>)?/g;
  const unesc = (s) => s.replace(/\\(u[0-9A-Fa-f]{4}|U[0-9A-Fa-f]{8}|.)/g, (_, e) =>
    e[0] === "u" || e[0] === "U" ? String.fromCodePoint(parseInt(e.slice(1), 16)) : ({ n: "\n", r: "\r", t: "\t", '"': '"', "\\": "\\" }[e] ?? e));
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    const terms = [];
    let m;
    re.lastIndex = 0;
    while ((m = re.exec(line))) {
      if (m[1] !== undefined) terms.push({ type: "uri", value: m[1] });
      else if (m[2] !== undefined) terms.push({ type: "bnode", value: m[2] });
      else {
        const t = { type: "literal", value: unesc(m[3]) };
        if (m[4]) t["xml:lang"] = m[4];
        if (m[5]) t.datatype = m[5];
        terms.push(t);
      }
    }
    if (terms.length >= 3) out.push(terms.slice(0, 3));
  }
  return out;
}

// ------------------------------------------------------------------ SPARQL

const UPDATE_WORDS = ["INSERT", "DELETE", "LOAD", "CLEAR", "DROP", "CREATE", "ADD", "MOVE", "COPY", "WITH"];

/// First query-form keyword, skipping comments, IRIs, strings and the prologue.
function queryKind(q) {
  let i = 0;
  while (i < q.length) {
    const c = q[i];
    if (c === "#") { while (i < q.length && q[i] !== "\n") i++; continue; }
    if (c === "<") { const j = q.indexOf(">", i); i = j < 0 ? q.length : j + 1; continue; }
    if (c === '"' || c === "'") { i++; while (i < q.length && q[i] !== c) i += q[i] === "\\" ? 2 : 1; i++; continue; }
    const m = /^[A-Za-z]+/.exec(q.slice(i, i + 12));
    if (m) {
      const k = m[0].toUpperCase();
      if (k === "SELECT" || k === "ASK") return "select";
      if (k === "CONSTRUCT" || k === "DESCRIBE") return "graph";
      if (UPDATE_WORDS.includes(k)) return "update";
      i += m[0].length;
      continue;
    }
    i++;
  }
  return "select";
}

async function runSparql(q, { infer = true } = {}) {
  const kind = queryKind(q);
  const t0 = performance.now();
  if (kind === "update") {
    await http("POST", repoUrl("/statements"), { body: form({ update: q }), headers: FORM });
    return { kind, ms: performance.now() - t0 };
  }
  const accept = kind === "graph" ? "application/n-triples" : "application/sparql-results+json";
  const res = await http("POST", repoUrl(), { body: form({ query: q, infer }), headers: FORM, accept });
  if (kind === "graph") {
    return { kind, vars: ["subject", "predicate", "object"], rows: parseNT(await res.text()), ms: performance.now() - t0 };
  }
  const json = await res.json();
  if ("boolean" in json) return { kind: "ask", value: json.boolean, ms: performance.now() - t0 };
  const vars = json.head.vars;
  return { kind, vars, rows: json.results.bindings.map((b) => vars.map((v) => b[v])), ms: performance.now() - t0 };
}

const select = async (q, opts) => (await runSparql(q, opts)).rows || [];

const KEYWORDS = new Set(("SELECT ASK CONSTRUCT DESCRIBE WHERE FROM NAMED PREFIX BASE DISTINCT REDUCED OPTIONAL UNION MINUS GRAPH " +
  "SERVICE SILENT BIND AS VALUES FILTER EXISTS NOT IN LIMIT OFFSET ORDER BY ASC DESC GROUP HAVING INSERT DELETE DATA " +
  "LOAD CLEAR DROP CREATE ADD MOVE COPY TO INTO WITH USING DEFAULT ALL UNDEF A TRUE FALSE SEPARATOR").split(" "));

/// Tokenize SPARQL into highlighted spans (for the editor overlay).
function highlight(src) {
  const out = [];
  const re = /(#[^\n]*)|("""[\s\S]*?"""|'''[\s\S]*?'''|"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*')|(<[^<>"{}|^`\\\s]*>)|([?$][\w]+)|([A-Za-z_][\w.-]*)?:([\w.%-]*[\w%-])?|(\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b)|([A-Za-z_]\w*)/g;
  let last = 0;
  let m;
  while ((m = re.exec(src))) {
    if (m.index > last) out.push(document.createTextNode(src.slice(last, m.index)));
    const [text] = m;
    let cls = null;
    if (m[1]) cls = "com";
    else if (m[2]) cls = "str";
    else if (m[3]) cls = "iri";
    else if (m[4]) cls = "var";
    else if (text.includes(":") && m[8] === undefined) cls = "pn";
    else if (m[7]) cls = "num";
    else if (m[8]) {
      const up = m[8].toUpperCase();
      cls = KEYWORDS.has(up) ? "kw" : src[re.lastIndex] === "(" ? "fn" : null;
    }
    out.push(cls ? h("span", { class: `tok-${cls}` }, text) : document.createTextNode(text));
    last = re.lastIndex;
    if (text.length === 0) re.lastIndex++;
  }
  out.push(document.createTextNode(src.slice(last) + "\n"));
  return out;
}

function sparqlEditor(initial, onRun) {
  const ta = h("textarea", { id: "sparql-editor", spellcheck: "false", autocapitalize: "off", autocomplete: "off", "aria-label": "SPARQL query" });
  ta.value = initial;
  const pre = h("pre", { "aria-hidden": "true" });
  const gutter = h("div", { class: "gutter", "aria-hidden": "true" });
  const sync = () => {
    pre.replaceChildren(...highlight(ta.value));
    const lines = ta.value.split("\n").length;
    gutter.textContent = Array.from({ length: lines }, (_, i) => i + 1).join("\n");
    pre.scrollTop = ta.scrollTop;
    pre.scrollLeft = ta.scrollLeft;
  };
  ta.addEventListener("input", sync);
  ta.addEventListener("scroll", () => { pre.scrollTop = ta.scrollTop; pre.scrollLeft = ta.scrollLeft; gutter.scrollTop = ta.scrollTop; });
  ta.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); onRun(); }
    if (e.key === "Tab" && !e.shiftKey) {
      e.preventDefault();
      ta.setRangeText("  ", ta.selectionStart, ta.selectionEnd, "end");
      sync();
    }
  });
  sync();
  const el = h("div", { class: "editor" }, gutter, h("div", { class: "code" }, pre, ta));
  return { el, textarea: ta, get value() { return ta.value; }, set value(v) { ta.value = v; sync(); } };
}

function resultsTable(vars, rows, { pageSize = 100, mark, roles } = {}) {
  const wrap = h("div");
  let page = 0;
  const pages = Math.max(1, Math.ceil(rows.length / pageSize));
  const render = () => {
    const slice = rows.slice(page * pageSize, (page + 1) * pageSize);
    const table = h("table", {},
      h("thead", {}, h("tr", {}, h("th", { class: "n" }, "#"),
        vars.map((v) => h("th", {}, h("span", { class: "var" }, roles ? v : `?${v}`))), mark ? h("th", {}, "") : null)),
      h("tbody", {}, slice.map((row, i) => h("tr", {},
        h("td", { class: "n" }, page * pageSize + i + 1),
        row.map((t, j) => h("td", {}, termNode(t, roles?.[j] || (j === 0 ? "subj" : "obj")))),
        mark ? h("td", { class: "actions" }, mark(row)) : null))));
    const pager = pages > 1 ? h("div", { class: "runbar" },
      h("button", { class: "sm", disabled: page === 0, onclick: () => { page--; render(); } }, "Previous"),
      h("span", { class: "status" }, `Page ${page + 1} of ${pages}`),
      h("button", { class: "sm", disabled: page >= pages - 1, onclick: () => { page++; render(); } }, "Next")) : null;
    wrap.replaceChildren(...[rows.length ? h("div", { class: "table-wrap" }, table) : h("div", { class: "empty" }, "No matching statements."), pager].filter(Boolean));
  };
  render();
  return wrap;
}

// ------------------------------------------------------------------ router

const routes = {};

function go(route, params = {}) {
  const qs = new URLSearchParams(params).toString();
  location.hash = `#/${route === "home" ? "" : route}${qs ? "?" + qs : ""}`;
}

// ---- phone navigation drawer
function setDrawer(open) {
  const was = document.body.classList.contains("drawer-open");
  if (open && !was) {
    // the drawer slides in under the finger: ignore taps on it until it has settled, so the
    // tap that opened it (or its synthetic click) does not also follow a link in it
    const sb = $("#sidebar");
    sb.style.pointerEvents = "none";
    setTimeout(() => { sb.style.pointerEvents = ""; }, 380);
  }
  document.body.classList.toggle("drawer-open", open);
  $("#scrim").hidden = !open;
  $("#menu-toggle").setAttribute("aria-expanded", String(open));
  if (open && !was) $("#menu-close").focus({ preventScroll: true });
}
$("#menu-toggle").addEventListener("click", () => setDrawer(!document.body.classList.contains("drawer-open")));
$("#menu-close").addEventListener("click", () => { setDrawer(false); $("#menu-toggle").focus(); });
$("#scrim").addEventListener("click", () => setDrawer(false));
$("#topbar-repo").addEventListener("click", () => setDrawer(true));
document.addEventListener("keydown", (e) => { if (e.key === "Escape" && document.body.classList.contains("drawer-open")) { setDrawer(false); $("#menu-toggle").focus(); } });
matchMedia("(min-width: 861px)").addEventListener("change", (m) => { if (m.matches) setDrawer(false); });

async function render() {
  const [path, qs] = location.hash.replace(/^#\/?/, "").split("?");
  const route = path || "home";
  const params = Object.fromEntries(new URLSearchParams(qs || ""));
  for (const a of document.querySelectorAll("#nav a")) {
    const on = a.dataset.route === route;
    a.classList.toggle("active", on);
    if (on) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current");
  }
  // the phone top bar names the page and repository; a route change closes the drawer
  const current = document.querySelector(`#nav a[data-route="${route}"]`);
  $("#topbar-page").textContent = current ? current.textContent.trim() : "Overview";
  $("#topbar-repo").textContent = state.repo || "";
  setDrawer(false);
  const page = routes[route] || routes.home;
  try {
    if (!["home", "repositories", "users", "cluster", "monitoring", "llm"].includes(route) && !state.repo) {
      view.replaceChildren(h("div", { class: "page" }, noRepo()));
      return;
    }
    view.replaceChildren(await page(params));
  } catch (e) {
    if (e instanceof AuthRequired) { view.replaceChildren(); return; }
    view.replaceChildren(h("div", { class: "page" }, panel("This page could not load", null, h("div", { class: "panel-body" }, h("pre", { class: "error-box" }, e.message)))));
  }
}

function noRepo() {
  return panel(null, null, h("div", { class: "empty" },
    h("b", {}, "No repository yet"),
    h("span", {}, "A repository holds your RDF statements and its inference ruleset."),
    h("button", { class: "primary", onclick: () => go("repositories") }, "Create a repository")));
}

// ------------------------------------------------------------------ overview

function barList(rows, cls) {
  if (!rows.length) return h("div", { class: "empty" }, "Nothing here yet.");
  const max = Math.max(...rows.map((r) => Number(r[1]?.value || 0)), 1);
  return h("div", { class: "bars" }, rows.map(([t, n]) => {
    const v = Number(n?.value || 0);
    return h("div", { class: `bar ${cls}` }, termNode(t, cls === "p" ? "pred" : "obj"), h("span", { class: "c" }, fmt(v)),
      h("div", { class: "track" }, h("div", { class: "fill", style: `width:${(v / max) * 100}%` })));
  }));
}

const stat = (v, k, small = false) => h("div", { class: "stat" }, h("span", { class: "v" + (small ? " small" : "") }, v), h("span", { class: "k" }, k));

routes.home = async () => {
  if (!state.repo) return h("div", { class: "page" }, pageHead("Welcome to GraphRD", "An RDF database with SPARQL 1.1 and rule-based inference. It speaks the repository protocol and its own management REST API."), noRepo());
  const [size, cfg, graphs, classes, preds] = await Promise.all([
    getJSON(restRepo("/size")),
    getJSON(restRepo()),
    select("SELECT (COUNT(DISTINCT ?g) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }", { infer: false }).catch(() => []),
    select("SELECT ?class (COUNT(?s) AS ?n) WHERE { ?s a ?class } GROUP BY ?class ORDER BY DESC(?n) LIMIT 8").catch(() => []),
    select("SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p ORDER BY DESC(?n) LIMIT 8", { infer: false }).catch(() => []),
  ]);
  return h("div", { class: "page" },
    pageHead(state.repo, cfg.title || "Repository overview",
      [h("button", { onclick: async (e) => {
        e.target.disabled = true;
        try {
          const r = await (await http("POST", `/rest/rank/compute?repository=${enc(state.repo)}`)).json();
          toast(`RDF Rank computed for ${fmt(r.nodes)} resources. Query it with rank:score.`);
        } catch (err) { toast(err.message, true); }
        e.target.disabled = false;
      } }, "Compute RDF Rank"),
      h("button", { onclick: () => go("import") }, "Import data"), h("button", { class: "primary", onclick: () => go("sparql") }, "Query")]),
    h("section", { class: "panel stats" },
      stat(fmt(size.explicit), "explicit statements"),
      stat(fmt(size.inferred), "inferred by the ruleset"),
      stat(fmt(graphs[0]?.[0]?.value ?? 0), "named graphs"),
      stat(cfg.params?.ruleset?.value || "empty", "ruleset", true)),
    h("div", { class: "cols" },
      panel("Largest classes", h("button", { class: "sm ghost", onclick: () => go("explore", { tab: "classes" }) }, "All classes"),
        h("div", { class: "panel-body" }, barList(classes, "o"))),
      panel("Most used predicates", null, h("div", { class: "panel-body" }, barList(preds, "p")))));
};

// ------------------------------------------------------------------ repositories

routes.repositories = async () => {
  const rulesets = await getJSON("/rest/repositories/ruleset").catch(() => ["empty", "rdfs", "rdfsplus-optimized", "owl-horst-optimized"]);
  const listBody = h("div", { class: "panel-body flush" });
  const refresh = async () => {
    await loadRepos();
    if (!state.repos.length) {
      listBody.replaceChildren(h("div", { class: "empty" }, h("b", {}, "No repositories yet"), "Create one with the form above."));
      return;
    }
    const rows = await Promise.all(state.repos.map(async (r) => ({
      r,
      size: await getJSON(restRepo("/size", r.id)).catch(() => ({})),
      cfg: await getJSON(restRepo("", r.id)).catch(() => ({})),
    })));
    listBody.replaceChildren(h("div", { class: "table-wrap" }, h("table", {},
      h("thead", {}, h("tr", {}, ["Repository", "Ruleset", "Search", "Explicit", "Inferred", ""].map((c, i) => h("th", { class: i === 3 || i === 4 ? "num" : null }, c)))),
      h("tbody", {}, rows.map(({ r, size, cfg }) => h("tr", {},
        h("td", {}, h("div", { class: "row" }, h("b", {}, r.id), r.id === state.repo ? h("span", { class: "chip accent" }, "active") : null),
          r.title ? h("div", { class: "muted small" }, r.title) : null),
        h("td", {}, rulesetSelect(r.id, cfg, rulesets)),
        h("td", {}, ftsToggle(r.id, cfg)),
        h("td", { class: "num" }, fmt(size.explicit)),
        h("td", { class: "num" }, fmt(size.inferred)),
        h("td", { class: "actions" }, h("div", { class: "row" },
          r.id === state.repo ? null : h("button", { class: "sm", onclick: () => { setRepo(r.id); refresh(); } }, "Make active"),
          h("button", { class: "sm danger", onclick: async () => {
            const ok = await dialog({ title: `Delete ${r.id}?`, text: `This removes the repository and all ${fmt(size.explicit)} statements in it. It cannot be undone.`, confirm: "Delete repository", danger: true });
            if (!ok) return;
            try { await http("DELETE", restRepo("", r.id)); toast(`Deleted ${r.id}`); refresh(); } catch (e) { toast(e.message, true); }
          } }, "Delete")))))))));
  };

  const id = h("input", { type: "text", id: "new-repo-id", placeholder: "e.g. products", pattern: "[\\w.\\-]+", required: true });
  const title = h("input", { type: "text", id: "new-repo-title", placeholder: "What this data is" });
  const ruleset = h("select", { id: "new-repo-ruleset" }, rulesets.map((r) => h("option", { value: r, selected: r === "rdfsplus-optimized" }, r)));
  const sameAs = h("input", { type: "checkbox", id: "new-repo-sameas", checked: true });
  const ftsBox = h("input", { type: "checkbox", id: "new-repo-fts" });
  const shaclBox = h("input", { type: "checkbox", id: "new-repo-shacl" });
  const create = h("form", { class: "panel-body", onsubmit: async (e) => {
    e.preventDefault();
    const params = {
      ruleset: { name: "ruleset", label: "Ruleset", value: ruleset.value },
      disableSameAs: { name: "disableSameAs", label: "Disable owl:sameAs", value: String(!sameAs.checked) },
      enableFtsIndex: { name: "enableFtsIndex", label: "Enable full-text search (FTS) index", value: String(ftsBox.checked) },
      isShacl: { name: "isShacl", label: "Enable SHACL validation", value: String(shaclBox.checked) },
    };
    try {
      await http("POST", "/rest/repositories", { body: JSON.stringify({ id: id.value.trim(), title: title.value, type: "graphrd", params }), headers: JSONH });
      toast(`Created ${id.value.trim()}`);
      setRepo(id.value.trim());
      id.value = title.value = "";
      refresh();
    } catch (err) { toast(err.message, true); }
  } },
  h("div", { class: "cols", style: "gap:14px" },
    h("label", { class: "field" }, "Id", id, h("span", { class: "hint" }, "Letters, digits, dot, dash and underscore.")),
    h("label", { class: "field" }, "Title", title),
    h("label", { class: "field" }, "Ruleset", ruleset, h("span", { class: "hint" }, "Which entailments are materialized."))),
  h("div", { class: "row" },
    h("label", { class: "row small muted", for: "new-repo-sameas" }, sameAs, "Reason over owl:sameAs"),
    h("label", { class: "row small muted", for: "new-repo-fts" }, ftsBox, "Full-text index"),
    h("label", { class: "row small muted", for: "new-repo-shacl" }, shaclBox, "SHACL validation"),
    h("div", { class: "grow" }),
    h("button", { class: "primary", type: "submit" }, "Create repository")));

  await refresh();
  const restoreInput = h("input", { type: "file", id: "restore-file", accept: ".tar", hidden: true });
  restoreInput.addEventListener("change", async () => {
    const f = restoreInput.files[0];
    if (!f) return;
    if (!(await dialog({ title: `Restore ${f.name}?`, text: "Repositories in the archive replace existing ones with the same id.", confirm: "Restore", danger: true }))) return;
    try {
      const res = await http("POST", "/rest/recovery/restore", { body: await f.arrayBuffer(), headers: { "Content-Type": "application/x-tar" } });
      const out = await res.json();
      toast(`Restored ${out.restored.map((r) => `${r.id} (${fmt(r.statements)})`).join(", ")}`);
      refresh();
    } catch (e) { toast(e.message, true); }
  });
  return h("div", { class: "page" },
    pageHead("Repositories", "Each repository is an independent store with its own inference ruleset.", [
      h("button", { onclick: () => download("/rest/recovery/backup", "graphrd-backup.tar", "application/x-tar") }, "Download backup"),
      h("button", { onclick: () => restoreInput.click() }, "Restore backup"), restoreInput]),
    panel("New repository", null, create),
    panel("All repositories", null, listBody));
};

function rulesetSelect(id, cfg, rulesets) {
  const current = cfg.params?.ruleset?.value || "empty";
  const sel = h("select", { "aria-label": `Ruleset of ${id}` }, rulesets.map((r) => h("option", { value: r, selected: r === current }, r)));
  sel.addEventListener("change", async () => {
    const params = { ...(cfg.params || {}), ruleset: { name: "ruleset", label: "Ruleset", value: sel.value } };
    try {
      await http("PUT", restRepo("", id), { body: JSON.stringify({ id, params }), headers: JSONH });
      toast(`${id} now uses ${sel.value}. Inferences were recomputed.`);
      render();
      refreshRepoStats();
    } catch (e) { toast(e.message, true); }
  });
  return sel;
}

const ftsOn = (cfg) => cfg.params?.enableFtsIndex?.value === "true";

function ftsToggle(id, cfg) {
  const box = h("input", { type: "checkbox", checked: ftsOn(cfg), "aria-label": `Full-text index for ${id}` });
  box.addEventListener("change", async () => {
    const params = { ...(cfg.params || {}), enableFtsIndex: { name: "enableFtsIndex", label: "Enable full-text search (FTS) index", value: String(box.checked) } };
    try {
      await http("PUT", restRepo("", id), { body: JSON.stringify({ id, params }), headers: JSONH });
      cfg.params = params;
      toast(box.checked ? `Full-text index built for ${id}` : `Full-text index removed from ${id}`);
    } catch (e) { box.checked = !box.checked; toast(e.message, true); }
  });
  return h("label", { class: "row small muted" }, box, "Full-text");
}

function setRepo(id) {
  state.repo = id;
  store.set("graphrd.repo", id);
  $("#repo-select").value = id;
  loadNamespaces();
  refreshRepoStats();
}

// ------------------------------------------------------------------ SPARQL page

const SAMPLE = "SELECT ?subject ?predicate ?object\nWHERE {\n  ?subject ?predicate ?object .\n}\nLIMIT 100";

routes.sparql = async (params) => {
  const key = `graphrd.query.${state.repo}`;
  let infer = store.get("graphrd.infer", true);
  const status = h("span", { class: "status", role: "status" });
  const results = h("div");
  const resultsHead = h("h2", {}, "Results");
  const savedSel = h("select", { id: "saved-queries", "aria-label": "Saved queries" });
  const editor = sparqlEditor(params.query || store.get(key, SAMPLE), () => run());

  const loadSaved = async () => {
    const list = await getJSON("/rest/sparql/saved-queries").catch(() => []);
    savedSel.replaceChildren(h("option", { value: "" }, list.length ? "Open saved query" : "No saved queries"), ...list.map((q) => h("option", { value: q.body }, q.name)));
  };
  savedSel.addEventListener("change", () => { if (savedSel.value) { editor.value = savedSel.value; savedSel.value = ""; } });

  const inferToggle = h("div", { class: "segmented", role: "group", "aria-label": "Statements to include" },
    ...[[true, "With inferred"], [false, "Explicit only"]].map(([v, label]) => h("button", { type: "button", "aria-pressed": String(infer === v),
      onclick: (e) => { infer = v; store.set("graphrd.infer", v); for (const b of e.target.parentNode.children) b.setAttribute("aria-pressed", String(b === e.target)); } }, label)));

  async function run() {
    store.set(key, editor.value);
    status.className = "status";
    status.textContent = "Running…";
    try {
      const r = await runSparql(editor.value, { infer });
      const ms = `${Math.round(r.ms)} ms`;
      if (r.kind === "update") {
        status.className = "status ok";
        status.textContent = `Update applied in ${ms}`;
        resultsHead.textContent = "Results";
        results.replaceChildren(h("div", { class: "empty" }, h("b", {}, "Update applied"), "The repository was changed."));
        refreshRepoStats();
      } else if (r.kind === "ask") {
        status.textContent = `Answered in ${ms}`;
        resultsHead.textContent = "Answer";
        results.replaceChildren(h("div", { class: "empty" }, h("b", {}, r.value ? "Yes" : "No"), r.value ? "ASK found at least one match." : "ASK found no match."));
      } else {
        status.className = "status ok";
        status.textContent = `${fmt(r.rows.length)} ${r.kind === "graph" ? "statements" : "rows"} · ${ms}`;
        resultsHead.textContent = r.kind === "graph" ? "Statements" : "Results";
        results.replaceChildren(resultsTable(r.vars, r.rows, r.kind === "graph" ? { roles: ["subj", "pred", "obj"] } : {}));
      }
    } catch (e) {
      if (e instanceof AuthRequired) return;
      status.className = "status err";
      status.textContent = "Query failed";
      resultsHead.textContent = "Error";
      results.replaceChildren(h("div", { class: "panel-body" }, h("pre", { class: "error-box" }, e.message)));
    }
  }

  const exportAs = (accept, ext) => {
    if (queryKind(editor.value) === "update") return toast("An update has no results to download.", true);
    download(repoUrl() + "?" + form({ query: editor.value, infer }), `results.${ext}`, accept);
  };

  await loadSaved();
  results.replaceChildren(h("div", { class: "empty" }, "Run the query to see results here."));
  if (params.run) setTimeout(run, 0);
  const askBar = await askPanel(editor, (sparql) => store.set(key, sparql), run);
  return h("div", { class: "page" },
    pageHead("SPARQL", null, [savedSel,
      h("button", { onclick: () => { editor.value = prefixHeader() + "\n\n" + editor.value.replace(/^(\s*PREFIX[^\n]*\n)*\s*/i, ""); } }, "Insert prefixes"),
      h("button", { onclick: async () => {
        const name = await dialog({ title: "Save query", input: { label: "Name", placeholder: "e.g. People by age" }, confirm: "Save" });
        if (!name) return;
        try {
          await http("POST", "/rest/sparql/saved-queries", { body: JSON.stringify({ name, body: editor.value, shared: false }), headers: JSONH });
          toast(`Saved "${name}"`);
          loadSaved();
        } catch (e) { toast(e.message, true); }
      } }, "Save")]),
    askBar,
    h("section", { class: "panel" }, editor.el,
      h("div", { class: "runbar" },
        h("button", { class: "primary", onclick: run }, "Run query"),
        h("span", { class: "small muted" }, h("kbd", {}, "⌘"), " ", h("kbd", {}, "Enter")),
        inferToggle,
        h("div", { class: "grow" }),
        status)),
    panel(resultsHead, h("div", { class: "row" },
      h("span", { class: "small muted" }, "Download"),
      h("button", { class: "sm", onclick: () => exportAs("text/csv", "csv") }, "CSV"),
      h("button", { class: "sm", onclick: () => exportAs("application/sparql-results+json", "json") }, "JSON"),
      h("button", { class: "sm", onclick: () => exportAs("text/turtle", "ttl") }, "Turtle")),
    h("div", { class: "panel-body flush" }, results)));
};

// ------------------------------------------------------------------ questions in plain language

/// The AI sparkles, the mark of everything the AI assistant does.
function sparkles(cls = "ai-icon") {
  const svg = svgEl("svg", { class: cls, viewBox: "0 0 20 20", "aria-hidden": "true" });
  for (const d of ["M8.5 3.5l1.4 3.6 3.6 1.4-3.6 1.4-1.4 3.6-1.4-3.6-3.6-1.4 3.6-1.4z", "M14.5 11.5l.7 1.8 1.8.7-1.8.7-.7 1.8-.7-1.8-1.8-.7 1.8-.7z"]) svg.append(svgEl("path", { d }));
  return svg;
}

/// The "Ask AI" bar above the SPARQL editor, when a language model is configured.
/// GraphRD answers: while the model works a "GraphRD is writing" badge pulses, then the query is
/// typed into the editor; "Ask and run" runs it once written. `keep` stores the written query.
async function askPanel(editor, keep, run) {
  const status = await getJSON("/rest/llm/status").catch(() => ({ enabled: false }));
  if (!status.enabled) return null;
  const question = h("input", { type: "text", id: "ask-question", placeholder: "e.g. Which people over 30 work for ACME?", "aria-label": "Question", autocomplete: "off" });
  const note = h("div", { class: "ask-note", role: "status" });
  const buttons = [];
  const badge = (busy) => {
    const mark = svgEl("svg", { class: "mark", viewBox: "0 0 32 32", "aria-hidden": "true" });
    for (const [cx, cy, c] of [["9", "9", "s"], ["23", "9", "p"], ["16", "23", "o"]]) mark.append(svgEl("circle", { cx, cy, r: "4.5", class: c }));
    return h("span", { class: "gr-badge" + (busy ? " busy" : "") }, mark, "GraphRD AI");
  };
  const say = (busy, text, cls = "") => note.replaceChildren(badge(busy), h("span", { class: "ask-text " + cls }, text),
    ...(busy ? [h("span", { class: "dots", "aria-hidden": "true" }, h("i"), h("i"), h("i"))] : []));
  const ask = async (andRun) => {
    if (!question.value.trim()) { question.focus(); return; }
    buttons.forEach((b) => (b.disabled = true));
    say(true, "is reading your question and writing a query");
    try {
      const res = await http("POST", restRepo("/nl-query"), { body: JSON.stringify({ question: question.value.trim() }), headers: JSONH, accept: "application/json" });
      const a = await res.json();
      say(true, "is writing the query");
      await typeQuery(editor, a.sparql);
      keep(a.sparql);
      say(false, "wrote this query from your question. Check it before trusting the answer.", "ok");
      if (andRun) run();
    } catch (e) {
      if (e instanceof AuthRequired) return;
      say(false, "could not write a query: " + e.message, "err");
    } finally {
      buttons.forEach((b) => (b.disabled = false));
    }
  };
  const write = h("button", { type: "submit", class: "ai-btn" }, sparkles(), "Write query");
  const both = h("button", { type: "button", class: "ai-btn primary", onclick: () => ask(true) }, sparkles(), "Ask and run");
  buttons.push(write, both);
  return h("form", { class: "panel ask", onsubmit: (e) => { e.preventDefault(); ask(false); } },
    h("div", { class: "runbar" },
      h("label", { class: "field grow", for: "ask-question" },
        h("span", { class: "ai-label" }, sparkles(), "Ask AI", h("span", { class: "hint" }, "in plain language; GraphRD AI writes the SPARQL")),
        question),
      write, both),
    note);
}

/// Type `text` into the SPARQL editor as if someone were writing it (about a second and a half,
/// whatever its length), with a blinking cursor; at once when the reader prefers less motion.
/// The editor keeps a "Written by GraphRD" tag until it is edited.
function typeQuery(editor, text) {
  const el = editor.el;
  el.querySelector(".gr-tag")?.remove();
  const tag = h("span", { class: "gr-tag" }, sparkles(), "Written by GraphRD AI");
  const done = () => {
    editor.value = text;
    editor.textarea.readOnly = false;
    el.classList.remove("writing");
    el.append(tag);
    editor.textarea.addEventListener("input", () => tag.remove(), { once: true });
  };
  if (matchMedia("(prefers-reduced-motion: reduce)").matches) { done(); return Promise.resolve(); }
  editor.textarea.readOnly = true;
  el.classList.add("writing");
  const step = Math.max(1, Math.ceil(text.length / 90));
  return new Promise((resolve) => {
    let i = 0;
    const frame = () => {
      // a little faster across spaces and line ends, so it reads as writing words
      i = Math.min(text.length, i + step + (/\s/.test(text[i] || "") ? 1 : 0));
      editor.value = text.slice(0, i);
      if (i < text.length) requestAnimationFrame(frame);
      else { done(); resolve(); }
    };
    editor.value = "";
    requestAnimationFrame(frame);
  });
}

const LLM_PRESETS = {
  anthropic: { url: "https://api.anthropic.com", model: "claude-opus-5-5", key: "sk-ant-…" },
  openai: { url: "https://api.openai.com/v1", model: "gpt-5", key: "sk-…" },
};

routes.llm = async () => {
  const state0 = await getJSON("/rest/llm");
  const cur = state0.saved || state0.startup || {};
  const api = h("select", { id: "llm-api" },
    h("option", { value: "auto" }, "Automatic (from the endpoint or key)"),
    h("option", { value: "anthropic" }, "Anthropic Messages API"),
    h("option", { value: "openai" }, "OpenAI-compatible chat completions"));
  api.value = cur.api || "auto";
  const url = h("input", { type: "url", id: "llm-url", value: cur.url || "", placeholder: "https://api.anthropic.com · http://127.0.0.1:11434/v1", autocomplete: "off" });
  const key = h("input", { type: "password", id: "llm-key", autocomplete: "new-password", placeholder: cur.keySet ? "•••••• saved (leave empty to keep it)" : "if the server needs one" });
  const clearKey = h("input", { type: "checkbox", id: "llm-key-clear" });
  const model = h("input", { type: "text", id: "llm-model", value: cur.model || "", placeholder: "claude-opus-5-5 · gpt-5 · qwen3-coder", autocomplete: "off" });
  const effort = h("select", { id: "llm-effort" }, h("option", { value: "" }, "The model's default"), ...["low", "medium", "high", "xhigh", "max"].map((e) => h("option", { value: e }, e)));
  effort.value = cur.effort || "";
  const timeout = h("input", { type: "number", id: "llm-timeout", min: "1", max: "3600", value: cur.timeoutSecs || "", placeholder: "120" });
  const testNote = h("span", { class: "status", role: "status" });
  const fillHints = () => {
    const p = LLM_PRESETS[api.value];
    url.placeholder = p ? p.url : "https://api.anthropic.com · http://127.0.0.1:11434/v1";
    model.placeholder = p ? p.model : "claude-opus-5-5 · gpt-5 · qwen3-coder";
  };
  api.addEventListener("change", fillHints);
  fillHints();
  const body = () => {
    const b = { api: api.value, url: url.value.trim(), model: model.value.trim(), effort: effort.value, timeoutSecs: Number(timeout.value) || 0 };
    if (clearKey.checked) b.key = "";
    else if (key.value) b.key = key.value;
    return JSON.stringify(b);
  };
  const a = state0.active;
  const summary = a
    ? h("div", { class: "panel-body" },
        h("div", { class: "row" }, h("span", { class: "chip ok" }, "On"), h("span", { class: "muted" }, "using"), h("b", {}, a.model), h("span", { class: "muted" }, "at"), h("code", {}, a.url)),
        h("p", { class: "small muted" }, `${a.api === "anthropic" ? "Anthropic Messages API" : "OpenAI-compatible chat completions"} · ${a.keySet ? "with a key" : "no key"} · from ${a.source}`))
    : h("div", { class: "panel-body" }, h("div", { class: "row" }, h("span", { class: "chip" }, "Off"),
        h("span", { class: "muted" }, "The AI assistant is off. Connect a language model below, or start the server with ANTHROPIC_API_KEY or OPENAI_API_KEY set.")));
  return h("div", { class: "page" },
    pageHead(h("span", { class: "ai-title" }, sparkles(), "AI Assistant"), "GraphRD AI turns questions asked in plain language into SPARQL, on the SPARQL page. Here you connect the language model behind it. The model sees a summary of the repository's classes and properties, with example values."),
    panel("Status", null, summary),
    panel("Language model", null, h("form", { class: "panel-body", onsubmit: async (e) => {
      e.preventDefault();
      try {
        await http("PUT", "/rest/llm", { body: body(), headers: JSONH });
        toast(state0.persistent ? "Saved; in use now" : "In use now (this server keeps nothing on disk)");
        render();
      } catch (err) { if (!(err instanceof AuthRequired)) toast(err.message, true); }
    } },
      h("div", { class: "cols", style: "gap:14px" },
        h("label", { class: "field" }, "Endpoint", url, h("span", { class: "hint" }, "Up to /v1 for OpenAI-style servers. Empty: the provider's own API.")),
        h("label", { class: "field" }, "Model", model, h("span", { class: "hint" }, "As the server names it. Required for servers other than Anthropic's and OpenAI's."))),
      h("div", { class: "cols", style: "gap:14px" },
        h("div", { class: "field-group" },
          h("label", { class: "field" }, "API key", key,
            h("span", { class: "hint" }, "Stored on the server in a file only its user can read; never shown again.")),
          cur.keySet ? h("div", { class: "row small" }, clearKey, h("label", { for: "llm-key-clear" }, "Remove the saved key")) : null),
        h("label", { class: "field" }, "Protocol", api)),
      h("div", { class: "cols", style: "gap:14px" },
        h("label", { class: "field" }, "Effort", effort),
        h("label", { class: "field" }, "Time limit (seconds)", timeout)),
      h("div", { class: "row end" },
        testNote,
        h("button", { type: "button", onclick: async () => {
          testNote.className = "status";
          testNote.textContent = "Calling the model…";
          try {
            const r = await (await http("POST", "/rest/llm/test", { body: body(), headers: JSONH, accept: "application/json" })).json();
            testNote.className = r.ok ? "status ok" : "status err";
            testNote.textContent = r.ok ? `${r.model} answered in ${fmt(r.ms)} ms` : r.error;
          } catch (err) { if (!(err instanceof AuthRequired)) { testNote.className = "status err"; testNote.textContent = err.message; } }
        } }, "Test"),
        state0.saved ? h("button", { type: "button", class: "ghost", onclick: async () => {
          if (!(await dialog({ title: "Use the startup settings?", text: "The settings saved here are removed; the server goes back to its command-line options and environment.", confirm: "Remove saved settings" }))) return;
          try { await http("DELETE", "/rest/llm"); toast("Back to the startup settings"); render(); } catch (err) { toast(err.message, true); }
        } }, "Reset") : null,
        h("button", { class: "primary", type: "submit" }, "Save")))));
};

// ------------------------------------------------------------------ graphql

const GRAPHQL_SAMPLE = `# Types and fields come from the data: see "Schema".
# Mutations: createType(id, input), updateType(id, input), deleteType(id)
{
  __typename
}`;

routes.graphql = async () => {
  const key = `graphrd.graphql.${state.repo}`;
  const url = `/rest/repositories/${enc(state.repo)}/graphql`;
  const status = h("span", { class: "status", role: "status" });
  const out = h("pre", { class: "json-out" }, "Run the request to see the response here.");
  const editor = sparqlEditor(store.get(key, GRAPHQL_SAMPLE), () => run());
  editor.textarea.setAttribute("aria-label", "GraphQL request");

  async function run() {
    store.set(key, editor.value);
    status.className = "status";
    status.textContent = "Running…";
    const t0 = performance.now();
    try {
      const res = await http("POST", url, { body: editor.value, headers: { "Content-Type": "application/graphql" }, accept: "application/json" });
      const body = await res.json();
      const ms = `${Math.round(performance.now() - t0)} ms`;
      out.textContent = JSON.stringify(body, null, 2);
      if (body.errors) {
        status.className = "status err";
        status.textContent = `Failed · ${ms}`;
      } else {
        status.className = "status ok";
        status.textContent = `Done · ${ms}`;
        if (/^\s*(#[^\n]*\n\s*)*mutation\b/.test(editor.value)) refreshRepoStats();
      }
    } catch (e) {
      if (e instanceof AuthRequired) return;
      status.className = "status err";
      status.textContent = "Request failed";
      out.textContent = e.message;
    }
  }

  const showSchema = async () => {
    try {
      const sdl = await (await http("GET", `${url}/schema`)).text();
      out.textContent = sdl;
      status.className = "status";
      status.textContent = "Schema";
    } catch (e) { if (!(e instanceof AuthRequired)) toast(e.message, true); }
  };

  return h("div", { class: "page" },
    pageHead("GraphQL", "Queries and mutations over a schema derived from the data.", [h("button", { onclick: showSchema }, "Schema")]),
    h("section", { class: "panel" }, editor.el,
      h("div", { class: "runbar" },
        h("button", { class: "primary", onclick: run }, "Run"),
        h("span", { class: "small muted" }, h("kbd", {}, "⌘"), " ", h("kbd", {}, "Enter")),
        h("div", { class: "grow" }),
        status)),
    panel("Response", null, h("div", { class: "panel-body" }, out)));
};

// ------------------------------------------------------------------ import

const FORMATS = [
  ["text/turtle", "Turtle", ["ttl", "n3"]],
  ["application/rdf+xml", "RDF/XML", ["rdf", "owl", "xml"]],
  ["application/ld+json", "JSON-LD", ["jsonld", "json"]],
  ["application/n-triples", "N-Triples", ["nt"]],
  ["application/trig", "TriG", ["trig"]],
  ["application/n-quads", "N-Quads", ["nq"]],
];
const formatFor = (name) => (FORMATS.find(([, , exts]) => exts.includes(name.split(".").pop().toLowerCase())) || FORMATS[0])[0];

routes.import = async () => {
  const graph = h("input", { type: "text", id: "import-graph", placeholder: "Leave empty for the default graph" });
  const base = h("input", { type: "text", id: "import-base", placeholder: "Used to resolve relative IRIs" });
  const log = h("div", { class: "log" }, h("div", { class: "empty" }, "Imports you run appear here."));
  const logItem = (ok, name, detail) => {
    if (log.querySelector(".empty")) log.replaceChildren();
    log.prepend(h("div", { class: "log-item" }, h("span", { class: "dot" + (ok ? "" : " err") }),
      h("div", {}, h("b", {}, name), h("div", { class: ok ? "muted" : "status err" }, detail)),
      h("span", { class: "small muted" }, new Date().toLocaleTimeString())));
  };

  const send = async (name, body, contentType) => {
    const q = new URLSearchParams();
    if (graph.value.trim()) q.append("context", `<${graph.value.trim()}>`);
    if (base.value.trim()) q.append("baseURI", base.value.trim());
    try {
      const before = await getJSON(restRepo("/size"));
      const t0 = performance.now();
      await http("POST", repoUrl("/statements") + (q.toString() ? "?" + q : ""), { body, headers: { "Content-Type": contentType } });
      const after = await getJSON(restRepo("/size"));
      logItem(true, name, `${fmt(after.explicit - before.explicit)} statements added, ${fmt(after.inferred - before.inferred)} inferred · ${Math.round(performance.now() - t0)} ms`);
      toast(`Imported ${name}`);
      refreshRepoStats();
      loadNamespaces();
    } catch (e) {
      if (e instanceof AuthRequired) return;
      logItem(false, name, e.message);
    }
  };

  const fileInput = h("input", { type: "file", id: "import-files", multiple: true, hidden: true, accept: FORMATS.flatMap(([, , e]) => e.map((x) => "." + x)).join(",") });
  const drop = h("div", { class: "dropzone", tabindex: "0", role: "button", "aria-label": "Choose RDF files to import",
    onclick: () => fileInput.click(), onkeydown: (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); fileInput.click(); } } },
  h("b", {}, "Drop RDF files here or choose files"),
  h("span", { class: "small" }, "Turtle, RDF/XML, JSON-LD, N-Triples, TriG and N-Quads. The format follows the file extension."));
  const handleFiles = async (files) => { for (const f of files) await send(f.name, await f.arrayBuffer(), formatFor(f.name)); };
  fileInput.addEventListener("change", () => handleFiles([...fileInput.files]));
  drop.addEventListener("dragover", (e) => { e.preventDefault(); drop.classList.add("over"); });
  drop.addEventListener("dragleave", () => drop.classList.remove("over"));
  drop.addEventListener("drop", (e) => { e.preventDefault(); drop.classList.remove("over"); handleFiles([...e.dataTransfer.files]); });

  const text = h("textarea", { id: "import-text", rows: "8", placeholder: "@prefix ex: <http://example.org/> .\nex:alice ex:knows ex:bob ." });
  const textFormat = h("select", { id: "import-format", "aria-label": "Format of the pasted text" }, FORMATS.map(([mt, label]) => h("option", { value: mt }, label)));

  const files = await getJSON(restRepo("/import/server")).catch(() => []);
  const serverList = files.length
    ? h("div", { class: "table-wrap" }, h("table", {}, h("tbody", {}, files.map((f) => h("tr", {},
      h("td", {}, h("span", { class: "t" }, f.name)), h("td", { class: "num" }, `${fmt(f.size)} B`),
      h("td", { class: "actions" }, h("button", { class: "sm", onclick: async () => {
        try {
          await http("POST", restRepo("/import/server"), {
            body: JSON.stringify({ fileNames: [f.name], importSettings: { context: graph.value.trim(), baseURI: base.value.trim() || null } }), headers: JSONH });
          const rec = (await getJSON(restRepo("/import/upload"))).find((r) => r.name === f.name);
          if (rec?.status === "ERROR") throw new Error(rec.message);
          logItem(true, f.name, `${fmt(rec?.addedStatements)} statements added from the server`);
          refreshRepoStats();
        } catch (e) { if (!(e instanceof AuthRequired)) logItem(false, f.name, e.message); }
      } }, "Import")))))))
    : h("p", { class: "small muted", style: "margin:0" }, "Files placed in the server's data/import folder are listed here, so large files can be loaded without uploading.");

  return h("div", { class: "page" },
    pageHead("Import", "Load RDF into the active repository. Inference runs as soon as the data is in."),
    panel("Destination", null, h("div", { class: "panel-body cols", style: "gap:14px" },
      h("label", { class: "field" }, "Named graph", graph),
      h("label", { class: "field" }, "Base IRI", base))),
    h("div", { class: "cols" },
      panel("Files", null, h("div", { class: "panel-body" }, drop, fileInput)),
      panel("Paste RDF", null, h("div", { class: "panel-body" }, text,
        h("div", { class: "row" }, textFormat, h("div", { class: "grow" }),
          h("button", { class: "primary", onclick: () => { if (text.value.trim()) send("Pasted text", text.value, textFormat.value); } }, "Import text"))))),
    panel("Server files", null, h("div", { class: "panel-body" }, serverList)),
    panel("Activity", null, log));
};

// ------------------------------------------------------------------ explore

routes.explore = async (params) => {
  if (params.iri) return resourceView(params.iri);
  const tab = params.tab || "search";
  const tabs = h("div", { class: "tabs", role: "tablist" },
    [["search", "Search"], ["classes", "Classes"], ["hierarchy", "Class hierarchy"]].map(([k, label]) =>
      h("button", { role: "tab", "aria-selected": String(k === tab), onclick: () => go("explore", { tab: k }) }, label)));
  const body = tab === "classes" ? await classesView() : tab === "hierarchy" ? await hierarchyView() : await searchView(params.q);
  return h("div", { class: "page" },
    pageHead("Explore", "Find resources and follow their links. Select any IRI to open it."),
    h("section", { class: "panel" }, tabs, body));
};

async function searchView(initial = "") {
  const input = h("input", { type: "search", id: "explore-search", value: initial, placeholder: "An IRI, ex:alice, or words — \"exact phrase\", prefix*, a OR b, -exclude" });
  const out = h("div");
  // live IRI suggestions from the autocomplete index
  const list = h("datalist", { id: "explore-suggest" });
  input.setAttribute("list", "explore-suggest");
  let timer = null;
  input.addEventListener("input", () => {
    clearTimeout(timer);
    const v = input.value.trim();
    if (v.length < 2 || /\s|"/.test(v)) return;
    timer = setTimeout(async () => {
      const r = await getJSON(`/rest/autocomplete/query?repository=${enc(state.repo)}&q=${enc(v.split(":").pop())}`).catch(() => ({ suggestions: [] }));
      list.replaceChildren(...r.suggestions.slice(0, 20).map((s) => h("option", { value: s.value })));
    }, 150);
  });
  const doSearch = async () => {
    const v = input.value.trim();
    if (!v) return;
    if (/^(https?|urn|file|mailto):/.test(v) || /^<.*>$/.test(v)) return go("explore", { iri: v.replace(/^<|>$/g, "") });
    const pn = v.match(/^([\w-]*):(\S*)$/);
    const ns = pn && state.namespaces.find(([p]) => p === pn[1]);
    if (ns) return go("explore", { iri: ns[1] + pn[2] });
    const cfg = await getJSON(restRepo()).catch(() => ({}));
    let rows;
    if (ftsOn(cfg)) {
      const q = JSON.stringify(v);
      rows = await select(`PREFIX g: <http://graphrd.org/fn#>
SELECT ?resource ?text ?score WHERE { ?text g:fts ${q} . ?resource ?p ?text BIND(g:ftsScore(?text, ${q}) AS ?score) } ORDER BY DESC(?score) LIMIT 200`);
      out.replaceChildren(resultsTable(["resource", "matching text", "score"], rows, { roles: ["subj", "obj", "obj"] }));
    } else {
      const esc = v.replace(/[.*+?^${}()|[\]\\]/g, "\\\\$&").replace(/"/g, '\\"');
      rows = await select(`SELECT DISTINCT ?resource ?text WHERE { ?resource ?p ?text FILTER(isLiteral(?text) && REGEX(STR(?text), "${esc}", "i")) } LIMIT 200`);
      out.replaceChildren(h("p", { class: "small muted", style: "margin:0;padding:10px 16px" }, "Scanning all literals. Turn on the full-text index under Repositories for ranked, faster search."),
        resultsTable(["resource", "matching text"], rows, { roles: ["subj", "obj"] }));
    }
  };
  input.addEventListener("keydown", (e) => { if (e.key === "Enter") doSearch(); });
  if (initial) setTimeout(doSearch, 0);
  return h("div", {}, h("div", { class: "panel-body" }, h("div", { class: "row" }, h("div", { class: "grow" }, input, list), h("button", { class: "primary", onclick: doSearch }, "Search"))), out);
}

async function classesView() {
  const rows = await select("SELECT ?class (COUNT(?s) AS ?instances) WHERE { ?s a ?class } GROUP BY ?class ORDER BY DESC(?instances) LIMIT 500");
  return resultsTable(["class", "instances"], rows, { roles: ["obj", "obj"] });
}

async function hierarchyView() {
  const [rows, counts] = await Promise.all([
    select("PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> SELECT ?c ?super WHERE { ?c rdfs:subClassOf ?super FILTER(?c != ?super && isIRI(?super)) }", { infer: false }),
    select("SELECT ?c (COUNT(?s) AS ?n) WHERE { ?s a ?c } GROUP BY ?c"),
  ]);
  if (!rows.length) return h("div", { class: "empty" }, h("b", {}, "No class hierarchy"), "The repository has no rdfs:subClassOf statements.");
  const count = new Map(counts.map(([c, n]) => [c.value, n.value]));
  const children = new Map();
  const hasParent = new Set();
  for (const [c, s] of rows) {
    if (!children.has(s.value)) children.set(s.value, []);
    children.get(s.value).push(c.value);
    hasParent.add(c.value);
  }
  const roots = [...children.keys()].filter((k) => !hasParent.has(k)).sort();
  const build = (iri, seen) => h("li", {}, termNode({ type: "uri", value: iri }),
    count.has(iri) ? h("span", { class: "count" }, fmt(count.get(iri))) : null,
    children.has(iri) && !seen.has(iri) ? h("ul", {}, children.get(iri).sort().map((c) => build(c, new Set([...seen, iri])))) : null);
  return h("div", { class: "panel-body tree" }, h("ul", {}, roots.map((r) => build(r, new Set()))));
}

const RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

function groupByPredicate(rows, explicit) {
  const groups = new Map();
  for (const [p, o] of rows) {
    if (!groups.has(p.value)) groups.set(p.value, { p, items: [] });
    groups.get(p.value).items.push({ o, inferred: !explicit.has(termKey(p) + "§" + termKey(o)) });
  }
  return [...groups.values()];
}

function propertyGrid(groups, objectRole) {
  if (!groups.length) return h("div", { class: "empty" }, "None.");
  return h("div", { class: "props" }, groups.flatMap(({ p, items }) => [
    h("div", { class: "pred-cell" }, termNode(p, "pred")),
    h("div", { class: "objs" }, items.map(({ o, inferred }) => h("div", { class: "obj" }, termNode(o, objectRole),
      inferred ? h("span", { class: "chip inferred", title: "Derived by the ruleset" }, "inferred") : null))),
  ]));
}

async function resourceView(iri) {
  const node = `<${iri}>`;
  const [outAll, outExp, inAll, inExp, graphs] = await Promise.all([
    select(`SELECT ?p ?o WHERE { ${node} ?p ?o } LIMIT 5000`, { infer: true }),
    select(`SELECT ?p ?o WHERE { ${node} ?p ?o } LIMIT 5000`, { infer: false }),
    select(`SELECT ?p ?s WHERE { ?s ?p ${node} } LIMIT 5000`, { infer: true }),
    select(`SELECT ?p ?s WHERE { ?s ?p ${node} } LIMIT 5000`, { infer: false }),
    select(`SELECT DISTINCT ?g WHERE { GRAPH ?g { { ${node} ?p ?o } UNION { ?s ?p ${node} } } }`).catch(() => []),
  ]);
  const key = ([p, o]) => termKey(p) + "§" + termKey(o);
  const out = groupByPredicate(outAll, new Set(outExp.map(key)));
  const inc = groupByPredicate(inAll, new Set(inExp.map(key)));
  const types = out.find((g) => g.p.value === RDF_TYPE);
  const props = out.filter((g) => g.p.value !== RDF_TYPE);
  const total = outAll.length + inAll.length;
  return h("div", { class: "page" },
    h("div", { class: "resource-head" },
      h("div", { class: "row" }, h("button", { class: "sm ghost", onclick: () => history.back() }, "← Back"), h("span", { class: "eyebrow" }, "Resource")),
      h("h1", {}, abbreviate(iri)),
      h("div", { class: "iri-full" }, iri),
      types ? h("div", { class: "types" }, types.items.map(({ o, inferred }) => h("span", { class: "chip" + (inferred ? " inferred" : "") }, termNode(o)))) : null,
      graphs.length ? h("div", { class: "small muted" }, "In graphs ", graphs.map(([g]) => termNode(g)).flatMap((n, i) => (i ? [", ", n] : [n]))) : null),
    total === 0 ? panel(null, null, h("div", { class: "empty" }, h("b", {}, "No statements mention this IRI"), "Check the spelling, or switch to the repository that holds it.")) : null,
    panel("Properties", h("span", { class: "chip" }, fmt(props.reduce((n, g) => n + g.items.length, 0))), propertyGrid(props, "obj")),
    panel("Referenced by", h("span", { class: "chip" }, fmt(inAll.length)), propertyGrid(inc, "subj")),
    h("div", { class: "row" },
      h("button", { class: "primary", onclick: () => go("visual", { iri }) }, "Visualize"),
      h("button", { onclick: () => go("sparql", { query: `DESCRIBE ${node}`, run: 1 }) }, "Open as DESCRIBE query"),
      h("button", { onclick: () => download(repoUrl() + "?" + form({ query: `DESCRIBE ${node}` }), "resource.ttl", "text/turtle") }, "Download as Turtle")));
}

// ------------------------------------------------------------------ graphs

routes.graphs = async () => {
  const res = await http("GET", repoUrl("/contexts"), { accept: "application/sparql-results+json" });
  const contexts = (await res.json()).results.bindings.map((b) => b.contextID);
  const sizes = await Promise.all(contexts.map((c) => http("GET", repoUrl("/size") + "?" + form({ context: sparqlTerm(c) })).then((r) => r.text())));
  const defaultSize = await (await http("GET", repoUrl("/size") + "?context=null")).text();
  const exportFormat = h("select", { id: "export-format", "aria-label": "Export format" }, FORMATS.map(([mt, label]) => h("option", { value: mt }, label)));
  const ext = () => FORMATS.find(([mt]) => mt === exportFormat.value)[2][0];
  const row = (label, term, size) => h("tr", {},
    h("td", {}, label),
    h("td", { class: "num" }, fmt(size)),
    h("td", { class: "actions" }, h("div", { class: "row" },
      h("button", { class: "sm", onclick: () => go("sparql", { query: term ? `SELECT ?s ?p ?o WHERE {\n  GRAPH ${sparqlTerm(term)} { ?s ?p ?o }\n}\nLIMIT 100` : "SELECT ?s ?p ?o\nFROM <urn:graphrd:default>\nWHERE { ?s ?p ?o }\nLIMIT 100", run: 1 }) }, "Browse"),
      term && term.type === "uri" ? h("button", { class: "sm", onclick: () => go("visual", { graph: term.value }) }, "Visualize") : null,
      h("button", { class: "sm", onclick: () => download(repoUrl("/statements") + "?" + form({ context: term ? sparqlTerm(term) : "null", infer: "false" }), `graph.${ext()}`, exportFormat.value) }, "Export"),
      h("button", { class: "sm danger", onclick: async () => {
        const ok = await dialog({ title: "Clear graph?", text: `This removes all ${fmt(size)} statements from ${term ? term.value : "the default graph"}.`, confirm: "Clear graph", danger: true });
        if (!ok) return;
        try { await http("DELETE", repoUrl("/statements") + "?" + form({ context: term ? sparqlTerm(term) : "null" })); toast("Graph cleared"); render(); refreshRepoStats(); }
        catch (e) { toast(e.message, true); }
      } }, "Clear"))));
  return h("div", { class: "page" },
    pageHead("Named Graphs", "Statements are grouped into named graphs. The default graph holds statements loaded without a graph.",
      [exportFormat, h("button", { class: "primary", onclick: () => download(repoUrl("/statements") + "?infer=false", `${state.repo}.${ext()}`, exportFormat.value) }, "Export repository")]),
    panel(null, null, h("div", { class: "panel-body flush" }, h("div", { class: "table-wrap" }, h("table", {},
      h("thead", {}, h("tr", {}, h("th", {}, "Graph"), h("th", { class: "num" }, "Statements"), h("th", {}, ""))),
      h("tbody", {}, row(h("span", { class: "muted" }, "Default graph"), null, defaultSize), contexts.map((c, i) => row(termNode(c), c, sizes[i]))))))));
};

// ------------------------------------------------------------------ namespaces

routes.namespaces = async () => {
  await loadNamespaces();
  const prefix = h("input", { type: "text", id: "ns-prefix", placeholder: "ex", required: true });
  const ns = h("input", { type: "text", id: "ns-iri", placeholder: "http://example.org/", required: true });
  const list = state.namespaces.slice().sort((a, b) => a[0].localeCompare(b[0]));
  return h("div", { class: "page" },
    pageHead("Namespaces", "Prefixes shorten IRIs in results and can be used in queries without declaring them."),
    panel("Add or change a prefix", null, h("form", { class: "panel-body", onsubmit: async (e) => {
      e.preventDefault();
      try { await http("PUT", repoUrl(`/namespaces/${enc(prefix.value.trim())}`), { body: ns.value.trim(), headers: { "Content-Type": "text/plain" } }); toast(`Saved ${prefix.value.trim()}:`); render(); }
      catch (err) { toast(err.message, true); }
    } }, h("div", { class: "row" },
      h("label", { class: "field" }, "Prefix", prefix),
      h("label", { class: "field grow" }, "Namespace IRI", ns),
      h("button", { class: "primary", type: "submit", style: "align-self:flex-end" }, "Save")))),
    panel(null, null, h("div", { class: "panel-body flush" }, h("div", { class: "table-wrap" }, h("table", {},
      h("thead", {}, h("tr", {}, h("th", {}, "Prefix"), h("th", {}, "Namespace"), h("th", {}, ""))),
      h("tbody", {}, list.map(([p, n]) => h("tr", {},
        h("td", {}, h("b", { class: "t" }, `${p}:`)),
        h("td", {}, h("span", { class: "t muted" }, n)),
        h("td", { class: "actions" }, h("button", { class: "sm danger", onclick: async () => {
          try { await http("DELETE", repoUrl(`/namespaces/${enc(p)}`)); toast(`Removed ${p}:`); render(); }
          catch (e) { toast(e.message, true); }
        } }, "Remove"))))))))));
};

// ------------------------------------------------------------------ users

const ROLE_PRESETS = [
  ["ROLE_USER", "User: only the repositories granted below"],
  ["ROLE_REPO_MANAGER", "Repository manager: read, write and create every repository"],
  ["ROLE_ADMIN", "Administrator: everything, including users"],
];

routes.users = async () => {
  const users = await getJSON("/rest/security/users");
  const name = h("input", { type: "text", id: "user-name", placeholder: "e.g. alice", required: true });
  const pass = h("input", { type: "password", id: "user-pass", autocomplete: "new-password", required: true });
  const role = h("select", { id: "user-role" }, ROLE_PRESETS.map(([v, l]) => h("option", { value: v }, l)));
  const read = h("input", { type: "text", id: "user-read", placeholder: "zoo, lab  (or * for all)" });
  const write = h("input", { type: "text", id: "user-write", placeholder: "lab" });
  const grants = () => {
    const list = role.value === "ROLE_USER" ? ["ROLE_USER"] : [role.value, "ROLE_USER"];
    const repos = (s) => s.split(",").map((x) => x.trim()).filter(Boolean);
    repos(read.value).forEach((r) => list.push(`READ_REPO_${r}`));
    repos(write.value).forEach((r) => list.push(`WRITE_REPO_${r}`));
    return list;
  };
  return h("div", { class: "page" },
    pageHead("Users", "Who can sign in, and what each person may read, change or manage."),
    panel("Add a user", null, h("form", { class: "panel-body", onsubmit: async (e) => {
      e.preventDefault();
      try {
        await http("POST", `/rest/security/users/${enc(name.value.trim())}`, { body: JSON.stringify({ password: pass.value, grantedAuthorities: grants() }), headers: JSONH });
        toast(`Added ${name.value.trim()}`);
        render();
      } catch (err) { toast(err.message, true); }
    } },
    h("div", { class: "cols", style: "gap:14px" },
      h("label", { class: "field" }, "Username", name),
      h("label", { class: "field" }, "Password", pass),
      h("label", { class: "field" }, "Role", role)),
    h("div", { class: "cols", style: "gap:14px" },
      h("label", { class: "field" }, "May read repositories", read),
      h("label", { class: "field" }, "May write repositories", write, h("span", { class: "hint" }, "Writing includes reading."))),
    h("div", { class: "row end" }, h("button", { class: "primary", type: "submit" }, "Add user")))),
    panel(null, null, h("div", { class: "panel-body flush" }, h("div", { class: "table-wrap" }, h("table", {},
      h("thead", {}, h("tr", {}, h("th", {}, "User"), h("th", {}, "Permissions"), h("th", {}, ""))),
      h("tbody", {}, users.map((u) => h("tr", {},
        h("td", {}, h("b", {}, u.username)),
        h("td", {}, h("div", { class: "row" }, (u.grantedAuthorities || []).map((a) => h("span", { class: a.startsWith("ROLE_ADMIN") ? "chip accent" : "chip" }, a)))),
        h("td", { class: "actions" }, h("div", { class: "row" },
          h("button", { class: "sm", onclick: async () => {
            const pw = await dialog({ title: `New password for ${u.username}`, input: { label: "Password" }, confirm: "Set password" });
            if (!pw) return;
            try { await http("PUT", `/rest/security/users/${enc(u.username)}`, { body: JSON.stringify({ password: pw }), headers: JSONH }); toast("Password changed"); }
            catch (e) { toast(e.message, true); }
          } }, "Set password"),
          h("button", { class: "sm danger", onclick: async () => {
            if (!(await dialog({ title: `Remove ${u.username}?`, text: "They will be signed out and can no longer sign in.", confirm: "Remove user", danger: true }))) return;
            try { await http("DELETE", `/rest/security/users/${enc(u.username)}`); toast(`Removed ${u.username}`); render(); }
            catch (e) { toast(e.message, true); }
          } }, "Remove")))))))))));
};

// ------------------------------------------------------------------ cluster

routes.cluster = async () => {
  const load = async () => {
    const here = await getJSON("/rest/cluster/status");
    // the leader knows how far every member has replicated
    let lead = here;
    if (here.role !== "leader" && here.leader) {
      try { lead = await (await http("GET", "/rest/cluster/status", { accept: "application/json", headers: { "X-GraphRD-Consistency": "leader" } })).json(); } catch { lead = here; }
    }
    return { here, lead };
  };
  const mapOf = ({ here, lead }) => {
    const leaderId = lead.role === "leader" ? lead.nodeId : here.leader;
    const nodes = [{ id: lead.nodeId, voter: lead.voter !== false, leader: lead.nodeId === leaderId }, ...(lead.peers || []).map((p) => ({ ...p, leader: p.id === leaderId }))];
    const w = 520, hgt = 300, cx = w / 2, cy = hgt / 2, r = 110;
    const centre = nodes.find((n) => n.leader) || nodes[0];
    const others = nodes.filter((n) => n !== centre);
    const pos = new Map([[centre.id, [cx, cy]]]);
    others.forEach((n, i) => {
      const a = (2 * Math.PI * i) / Math.max(others.length, 1) - Math.PI / 2;
      pos.set(n.id, [cx + r * 1.6 * Math.cos(a), cy + r * Math.sin(a)]);
    });
    const esc = (t) => String(t).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
    let svg = `<svg viewBox="0 0 ${w} ${hgt}" role="img" aria-label="Cluster map">`;
    for (const n of others) {
      const [x1, y1] = pos.get(centre.id), [x2, y2] = pos.get(n.id);
      const known = lead.role === "leader" && n.lastContactMs != null;
      const healthy = known && n.lastContactMs < 1500;
      const lag = known && lead.lastIndex != null && n.matchIndex != null ? Math.max(0, lead.lastIndex - n.matchIndex) : null;
      svg += `<line x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}" class="link ${known ? (healthy ? "ok" : "down") : ""}" ${n.voter === false ? 'stroke-dasharray="6 5"' : ""}/>`;
      // the label sits nearer the leader, beside the line
      if (lag != null) svg += `<text x="${x1 + (x2 - x1) * 0.45 + (Math.abs(x2 - x1) < 20 ? 34 : 0)}" y="${y1 + (y2 - y1) * 0.45 - 6}" class="lag">${healthy ? (lag === 0 ? "in sync" : esc(lag) + " behind") : "unreachable"}</text>`;
    }
    for (const n of nodes) {
      const [x, y] = pos.get(n.id);
      svg += `<g class="node ${n.leader ? "leader" : ""} ${n.voter === false ? "learner" : ""} ${n.id === here.nodeId ? "self" : ""}"><circle cx="${x}" cy="${y}" r="${n.leader ? 30 : 24}"/><text x="${x}" y="${y + 4}">${esc(n.id)}</text>`;
      svg += `<text x="${x}" y="${y + (n.leader ? 46 : 40)}" class="sub">${n.leader ? "leader" : n.voter === false ? "learner" : "follower"}${n.id === here.nodeId ? " · this node" : ""}</text></g>`;
    }
    const div = h("div", { class: "cluster-map" });
    div.innerHTML = svg + "</svg>";
    return div;
  };
  let data = await load();
  const stats = h("section", { class: "panel stats" });
  const map = h("div", { class: "panel-body" });
  const table = h("div", { class: "panel-body flush" });
  const draw = () => {
    const { here, lead } = data;
    stats.replaceChildren(
      stat(here.role, "this node", true),
      stat(fmt(here.term), "term"),
      stat(here.leader || "none", "leader", true),
      stat(`${fmt(here.commitIndex)} / ${fmt(here.appliedIndex)}`, "commit / applied"),
      stat(fmt(here.snapshotIndex || 0), "compacted to"));
    map.replaceChildren(mapOf(data));
    const peers = lead.peers || [];
    table.replaceChildren(h("div", { class: "table-wrap" }, h("table", {},
      h("thead", {}, h("tr", {}, h("th", {}, "Node"), h("th", {}, "Address"), h("th", {}, "Role"), h("th", {}, "Replicated to"), h("th", {}, "Last contact"))),
      h("tbody", {}, [{ id: lead.nodeId, address: "", voter: lead.voter !== false, matchIndex: lead.lastIndex, lastContactMs: 0 }, ...peers].map((m) => h("tr", {},
        h("td", {}, h("div", { class: "row" }, h("b", {}, m.id),
          m.id === here.leader ? h("span", { class: "chip accent" }, "leader") : null,
          m.id === here.nodeId ? h("span", { class: "chip" }, "this node") : null)),
        h("td", {}, h("span", { class: "t muted" }, m.address || "—")),
        h("td", {}, m.voter === false ? "learner (read replica)" : "voter"),
        h("td", {}, m.matchIndex != null ? fmt(m.matchIndex) : "—"),
        h("td", {}, m.lastContactMs != null ? `${fmt(m.lastContactMs)} ms ago` : "—")))))));
  };
  draw();
  const page = h("div", { class: "page" },
    pageHead("Cluster", "Raft members, who leads, and how far each has replicated (refreshed every two seconds)."),
    stats, panel("Map", null, map), panel("Members", null, table));
  const timer = setInterval(async () => {
    if (!page.isConnected) { clearInterval(timer); return; }
    try { data = await load(); draw(); } catch { /* the node may be restarting */ }
  }, 2000);
  return page;
};

// ------------------------------------------------------------------ monitoring

// Prometheus text format -> { "name{labels}": value }
function parseMetrics(text) {
  const m = {};
  for (const line of text.split("\n")) {
    if (!line || line[0] === "#") continue;
    const i = line.lastIndexOf(" ");
    m[line.slice(0, i)] = Number(line.slice(i + 1));
  }
  return m;
}

// quantile q of the histogram `name` between two scrapes (bucket upper bound, in ms)
function histQuantile(cur, prev, name, q) {
  const bs = Object.keys(cur).filter((k) => k.startsWith(name + "_bucket{")).map((k) => {
    const le = k.match(/le="([^"]+)"/)[1];
    return { le: le === "+Inf" ? Infinity : Number(le), n: cur[k] - (prev?.[k] || 0) };
  }).sort((a, b) => a.le - b.le);
  const total = bs.length ? bs[bs.length - 1].n : 0;
  if (!total) return null;
  const b = bs.find((x) => x.n >= q * total);
  return b && isFinite(b.le) ? b.le * 1000 : 10000;
}

function sparkline(values, unit) {
  const w = 280, hgt = 56, pts = values.filter((v) => v != null);
  const max = Math.max(1e-9, ...pts);
  const step = w / Math.max(1, values.length - 1);
  const path = values.map((v, i) => v == null ? null : `${(i * step).toFixed(1)},${(hgt - 4 - (v / max) * (hgt - 10)).toFixed(1)}`).filter(Boolean);
  const div = h("div", { class: "spark" });
  div.innerHTML = `<svg viewBox="0 0 ${w} ${hgt}" preserveAspectRatio="none" role="img" aria-label="max ${max.toFixed(2)} ${unit}">` +
    (path.length > 1 ? `<polyline points="${path.join(" ")}" fill="none" stroke="currentColor" stroke-width="1.6" vector-effect="non-scaling-stroke"/>` : "") + `</svg>`;
  return div;
}

routes.monitoring = async () => {
  const fetchMetrics = async () => parseMetrics(await (await http("GET", "/metrics", { accept: "text/plain" })).text());
  const series = { qps: [], p50: [], p95: [], hit: [], mem: [] };
  const N = 60;
  let prev = await fetchMetrics(), prevAt = performance.now();
  const stats = h("section", { class: "panel stats" });
  const charts = h("div", { class: "grid-2" });
  const slow = h("div", { class: "panel-body flush" });
  const chart = (title, key, unit, last) => panel(title, h("span", { class: "t muted" }, last == null ? "—" : `${last.toFixed(unit === "%" ? 1 : 2)} ${unit}`), h("div", { class: "panel-body" }, sparkline(series[key], unit)));
  const tick = async () => {
    const cur = await fetchMetrics(), now = performance.now();
    const dt = Math.max(0.001, (now - prevAt) / 1000);
    const d = (k) => (cur[k] || 0) - (prev[k] || 0);
    const push = (k, v) => { series[k].push(v); if (series[k].length > N) series[k].shift(); };
    push("qps", d("graphrd_query_duration_seconds_count") / dt);
    push("p50", histQuantile(cur, prev, "graphrd_query_duration_seconds", 0.5));
    push("p95", histQuantile(cur, prev, "graphrd_query_duration_seconds", 0.95));
    const hits = d("graphrd_block_cache_hits_total"), misses = d("graphrd_block_cache_misses_total");
    push("hit", hits + misses ? (100 * hits) / (hits + misses) : null);
    push("mem", cur.graphrd_resident_memory_bytes ? cur.graphrd_resident_memory_bytes / 1048576 : null);
    prev = cur; prevAt = now;
    const last = (k) => series[k][series[k].length - 1];
    stats.replaceChildren(
      stat(fmt(cur.graphrd_query_duration_seconds_count), "queries"),
      stat(fmt(cur.graphrd_update_duration_seconds_count), "updates"),
      stat(fmt((cur.graphrd_query_errors_total || 0) + (cur.graphrd_update_errors_total || 0)), "errors"),
      stat(fmt(cur.graphrd_slow_queries_total), "slow"),
      stat(`${Math.round(cur.graphrd_uptime_seconds || 0)} s`, "uptime", true));
    charts.replaceChildren(
      chart("Queries per second", "qps", "q/s", last("qps")),
      chart("Query latency p50", "p50", "ms", last("p50")),
      chart("Query latency p95", "p95", "ms", last("p95")),
      chart("Block cache hit rate", "hit", "%", last("hit")),
      ...(cur.graphrd_resident_memory_bytes ? [chart("Resident memory", "mem", "MiB", last("mem"))] : []));
    const log = await getJSON("/rest/monitor/slow-queries");
    slow.replaceChildren(log.length ? h("div", { class: "table-wrap" }, h("table", {},
      h("thead", {}, h("tr", {}, h("th", {}, "When"), h("th", {}, "Repository"), h("th", {}, "ms"), h("th", {}, "Text"))),
      h("tbody", {}, log.slice(0, 20).map((e) => h("tr", {},
        h("td", {}, new Date(e.time).toLocaleTimeString()), h("td", {}, e.repository), h("td", {}, fmt(e.ms)),
        h("td", {}, h("code", { class: "t" }, e.text.length > 200 ? e.text.slice(0, 200) + "…" : e.text)))))))
      : h("div", { class: "empty" }, "No slow queries. Start the server with --slow-query <ms> to record them."));
  };
  await tick();
  const page = h("div", { class: "page" },
    pageHead("Monitoring", "Live numbers from /metrics (Prometheus format), refreshed every two seconds."),
    stats, charts, panel("Slow queries", null, slow));
  const timer = setInterval(() => {
    if (!page.isConnected) { clearInterval(timer); return; }
    tick().catch(() => {});
  }, 2000);
  return page;
};

// ------------------------------------------------------------------ sign-in

// The backdrop of the sign-in page: landmarks of Türkiye and marks of Turkish history,
// drawn as line art in a 200 × 200 box (ground at the bottom) with nodes along their lines,
// so each reads as a graph; shown without captions (the names are for the code). Each
// side of the card has one set of nodes that turns from one drawing into the next: the
// nodes glide to their new places, the old lines fade and the new ones draw; the two sides
// take turns. With reduced motion each side shows one drawing and stays.
const MOTIFS = (() => {
  // main lines carry nodes; fine lines are details; waves are water
  const M = (d) => ({ d, kind: "main" });
  const F = (d) => ({ d, kind: "fine" });
  const Wv = (d) => ({ d, kind: "wave" });
  const arches = (y, xs, w = 8, h = 12) => xs.map((x) => F(`M${x - w / 2},${y} L${x - w / 2},${y - h + w / 2} Q${x},${y - h - w / 2} ${x + w / 2},${y - h + w / 2} L${x + w / 2},${y}`));
  // the finial of a dome: a rod with a crescent on it
  const alem = (x, y, h = 14) => F(`M${x},${y} L${x},${y - h + 6} M${x - 3.5},${y - h + 2} A4,4 0 1 0 ${x + 3.5},${y - h + 2} A3.2,3.2 0 1 1 ${x - 3.5},${y - h + 2}`);
  // a minaret: its shaft, a pointed cap, and balconies as narrow bands
  const minaret = (x, top, base = 190) => [
    M(`M${x - 3},${base} L${x - 3},${top + 22} L${x},${top} L${x + 3},${top + 22} L${x + 3},${base}`),
    F(`M${x - 5},${top + 48} L${x + 5},${top + 48} L${x + 5},${top + 51} L${x - 5},${top + 51} Z`), F(`M${x - 5},${top + 82} L${x + 5},${top + 82} L${x + 5},${top + 85} L${x - 5},${top + 85} Z`),
  ];
  // Old Turkic letters, roughly, for the Orkhon stele (each in a 10 × 14 cell)
  const runes = ["M0,0 L5,7 L10,0 M5,7 L5,14", "M2,0 L2,14 M2,4 L9,0 M2,9 L9,5", "M0,14 L5,0 L10,14", "M0,0 L10,7 L0,14", "M5,0 L5,14 M0,4 L10,10", "M8,0 L2,7 L8,14", "M0,3 L5,0 L10,3 M5,0 L5,14", "M0,0 L10,14 M10,0 L0,14"];
  const rune = (x, y, k) => F(runes[k % runes.length].replace(/(-?\d+(\.\d+)?),(-?\d+(\.\d+)?)/g, (_, a, _b, c) => `${x + +a},${y + +c}`));
  const starPath = (cx, cy, R, rot = 0) => {
    const pts = [];
    for (let k = 0; k < 16; k++) {
      const r = k % 2 ? R * Math.cos(Math.PI / 4) / Math.cos(Math.PI / 8) : R;
      const a = (k * 22.5 + rot - 90) * Math.PI / 180;
      pts.push(`${(cx + r * Math.cos(a)).toFixed(1)},${(cy + r * Math.sin(a)).toFixed(1)}`);
    }
    return "M" + pts.join(" L") + " Z";
  };
  // shapes the drawings are made of
  const rect = (x, y, w, h) => `M${x},${y} L${x + w},${y} L${x + w},${y + h} L${x},${y + h} Z`;
  const circle = (cx, cy, r) => `M${cx - r},${cy} A${r},${r} 0 1 1 ${cx + r},${cy} A${r},${r} 0 1 1 ${cx - r},${cy} Z`;
  const dome = (cx, y, r, h) => `M${cx - r},${y} A${r},${h} 0 0 1 ${cx + r},${y}`;
  // battlements along a wall top, from x1 to x2 at height y
  const crenel = (x1, x2, y, w = 6) => {
    let d = `M${x1},${y}`;
    for (let x = x1; x < x2 - 0.1; x += 2 * w) {
      const e = Math.min(x + w, x2), f = Math.min(x + 2 * w, x2);
      d += ` L${x},${y - w} L${e},${y - w} L${e},${y} L${f},${y}`;
    }
    return d;
  };
  const fluted = (x1, x2, top, base, n) => [...Array(n)].map((_, i) => F(`M${(x1 + (x2 - x1) * (i + 1) / (n + 1)).toFixed(1)},${top} L${(x1 + (x2 - x1) * (i + 1) / (n + 1)).toFixed(1)},${base}`));
  const columns = (xs, top, base) => xs.map((x) => M(`M${x},${top} L${x},${base}`));
  return [
    { name: "Galata Kulesi", paths: [
      M("M100,6 L131,60 L138,64 L132,70 L134,80 L138,186 L146,189 L146,198 L54,198 L54,189 L62,186 L66,80 L68,70 L62,64 L69,60 Z"),
      F("M100,6 L100,0"), F("M64,70 L136,70"), F("M67,80 L133,80"), F("M66,122 L134,122"), F("M64,156 L136,156"),
      ...arches(108, [82, 100, 118]), ...arches(146, [86, 100, 114], 7, 11), ...arches(182, [100], 12, 18),
    ] },
    { name: "Boğaziçi Köprüsü", paths: [
      M("M0,138 L200,138"), M("M50,172 L50,38"), M("M150,172 L150,38"),
      M("M50,38 Q100,152 150,38"), M("M50,38 Q28,96 0,126"), M("M150,38 Q172,96 200,126"),
      F("M44,56 L56,56"), F("M44,96 L56,96"), F("M144,56 L156,56"), F("M144,96 L156,96"),
      ...[60, 70, 80, 90, 100, 110, 120, 130, 140].map((x) => { const t = (x - 50) / 100; const y = (1 - t) ** 2 * 38 + 2 * (1 - t) * t * 152 + t * t * 38; return F(`M${x},${y.toFixed(1)} L${x},138`); }),
      Wv("M0,184 Q25,176 50,184 T100,184 T150,184 T200,184"), Wv("M20,196 Q45,190 70,196 T120,196 T170,196"),
    ] },
    { name: "Orhun Yazıtları", paths: [
      M("M68,178 L68,44 Q100,14 132,44 L132,178"), M("M58,178 L142,178 L142,186 L58,186 Z"),
      M("M36,198 Q100,166 164,198"), F("M164,196 Q176,186 184,196"), F("M44,198 L40,192"), F("M150,190 L156,198"),
      ...[0, 1, 2, 3].flatMap((c) => [0, 1, 2, 3, 4, 5, 6].map((r) => rune(74 + c * 14, 48 + r * 18, c * 7 + r * 3))),
    ] },
    { name: "Kız Kulesi", paths: [
      Wv("M0,190 Q25,183 50,190 T100,190 T150,190 T200,190"),
      M("M14,182 Q100,170 186,182"),
      M("M28,176 L28,140 L106,140 L106,176"), M("M22,140 L64,118 L112,140"),
      M("M106,176 L106,74 L146,74 L146,176"), M("M102,74 L150,74"),
      M("M112,74 L112,52 L140,52 L140,74"), M("M110,52 Q126,26 142,52"), F("M126,36 L126,12"), F("M126,16 L136,19 L126,22"),
      ...arches(102, [118, 134], 7, 12), ...arches(140, [118, 134], 7, 12), ...arches(170, [48, 67, 86], 8, 14),
    ] },
    { name: "Selçuklu yıldızı", paths: [
      M(starPath(100, 100, 92)), M(starPath(100, 100, 60, 22.5)),
      F("M100,8 L100,192 M8,100 L192,100 M35,35 L165,165 M165,35 L35,165"),
      M("M100,82 A18,18 0 1 1 99.9,82 Z"),
    ] },
    { name: "Göbeklitepe", paths: [
      M("M72,184 L72,96 L50,96 L50,72 L112,72 L112,96 L90,96 L90,184"),
      M("M118,184 L118,104 L98,104 L98,82 L156,82 L156,104 L134,104 L134,184"),
      F("M74,120 Q82,138 88,140"), F("M88,140 L84,148"), F("M120,128 Q128,146 132,148"),
      F("M124,112 L130,108 L136,112 L132,118"),
      M("M8,186 Q100,158 192,186"), F("M8,186 Q100,204 192,186"),
      F("M22,182 L22,168 L16,168 L16,162 L30,162 L30,168 L24,168"), F("M178,182 L178,168 L172,168 L172,162 L186,162 L186,168 L180,168"),
    ] },
    { name: "Sultanahmet Camii", paths: [
      M("M58,112 Q100,50 142,112"), alem(100, 81, 20),
      M("M38,124 Q58,98 78,124"), M("M122,124 Q142,98 162,124"),
      M("M34,124 L166,124 L166,168 L34,168 Z"), M("M22,168 L178,168 L178,186 L22,186"),
      F("M50,112 Q58,104 66,112"), F("M134,112 Q142,104 150,112"),
      ...arches(160, [56, 78, 100, 122, 144], 8, 14),
      ...minaret(10, 26, 186), ...minaret(28, 40, 186), ...minaret(172, 40, 186), ...minaret(190, 26, 186),
      M("M0,192 L200,192"),
    ] },
    { name: "Celsus Kütüphanesi", paths: [
      M("M12,196 L188,196"), M("M18,188 L182,188"),
      ...[30, 44, 72, 86, 114, 128, 156, 170].map((x) => M(`M${x},188 L${x},124`)),
      M("M22,124 L178,124"), M("M24,116 L176,116"),
      ...[44, 58, 92, 108, 142, 156].map((x) => M(`M${x},116 L${x},66`)),
      M("M36,66 L164,66"),
      M("M38,62 L51,46 L64,62"), M("M86,62 Q100,40 114,62"), M("M136,62 L149,46 L162,62"),
      F("M52,188 L52,152 Q58,142 64,152 L64,188"), F("M92,188 L92,146 Q100,134 108,146 L108,188"), F("M136,188 L136,152 Q142,142 148,152 L148,188"),
    ] },
    { name: "Kapadokya", paths: [
      M("M0,196 L200,196"),
      M("M14,196 Q32,150 38,108 Q44,150 62,196"), M("M28,110 Q38,92 48,110 Z"),
      M("M56,196 Q78,130 84,66 Q90,130 112,196"), M("M72,68 Q84,44 96,68 Z"),
      M("M104,196 Q116,160 122,134 Q128,160 140,196"), M("M114,136 Q122,120 130,136 Z"),
      M("M136,54 A24,26 0 1 1 184,54 Q180,74 170,86 L150,86 Q140,74 136,54 Z"),
      F("M160,28 L160,86 M148,32 Q146,60 154,86 M172,32 Q174,60 166,86"),
      F("M150,86 L154,96 M170,86 L166,96"), M("M152,96 L168,96 L168,106 L152,106 Z"),
      F("M48,30 A8,9 0 1 1 64,30 Q62,38 58,42 L54,42 Q50,38 48,30 Z"), F("M54,42 L54,47 L58,47 L58,42"),
    ] },
    { name: "Ayasofya", paths: [
      M(dome(100, 92, 40, 32)), alem(100, 60), M(rect(60, 92, 80, 12)),
      ...arches(102, [70, 80, 90, 100, 110, 120, 130], 5, 7),
      M(dome(44, 112, 18, 14)), M(dome(156, 112, 18, 14)),
      F("M52,112 L60,94 L60,112"), F("M148,112 L140,94 L140,112"),
      M(rect(24, 112, 152, 56)), M("M12,186 L12,150 L24,150 M176,150 L188,150 L188,186"),
      ...arches(160, [46, 70, 130, 154], 8, 16), F("M86,168 L86,140 Q100,124 114,140 L114,168"),
      ...minaret(6, 34, 186), ...minaret(22, 56, 186), ...minaret(178, 56, 186), ...minaret(194, 34, 186),
      M("M0,190 L200,190"),
    ] },
    { name: "Hattuşa Aslanlı Kapı", paths: [
      M("M44,196 L44,62 L84,62 L84,104 Q100,64 116,104 L116,62 L156,62 L156,196"),
      M("M84,196 L84,104 M116,104 L116,196"),
      F("M0,196 L44,196 M156,196 L200,196"), F("M0,120 L44,116 M0,160 L44,156 M156,116 L200,120 M156,156 L200,160 M22,120 L20,160 M178,118 L180,158"),
      M(circle(64, 150, 13)), F(circle(64, 150, 6)), F("M58,147 L60,146 M68,146 L70,147 M60,156 Q64,159 68,156"),
      M(circle(136, 150, 13)), F(circle(136, 150, 6)), F("M130,147 L132,146 M140,146 L142,147 M132,156 Q136,159 140,156"),
      F("M48,70 L80,70 M120,70 L152,70"),
    ] },
    { name: "Dolmabahçe Saat Kulesi", paths: [
      M(rect(66, 152, 68, 44)), M(rect(74, 106, 52, 46)), M(rect(82, 66, 36, 40)),
      M(circle(100, 86, 12)), F("M100,86 L100,78 M100,86 L106,89"),
      M(rect(88, 46, 24, 20)), M(dome(100, 46, 12, 12)), F("M100,34 L100,18"), F(circle(100, 16, 2)),
      F("M62,152 L138,152 M70,106 L130,106 M78,66 L122,66"),
      ...arches(196, [100], 18, 30), ...arches(144, [88, 112], 8, 22),
      F("M70,160 L70,192 M130,160 L130,192 M78,112 L78,148 M122,112 L122,148"),
      M("M40,196 L160,196"),
    ] },
    { name: "Nemrut Dağı", paths: [
      M("M0,140 L60,90 L100,48 L140,90 L200,140"), F("M70,80 L100,48 L130,80"),
      ...[[44, 160], [100, 154], [156, 160]].flatMap(([x, y]) => [
        // the Commagene tiara: a tall cone over a band
        M(`M${x - 11},${y - 12} Q${x - 6},${y - 40} ${x + 2},${y - 46} Q${x + 4},${y - 30} ${x + 11},${y - 12}`), M(rect(x - 13, y - 12, 26, 6)),
        // the face beneath, with the stone beard of the gods
        M(`M${x - 12},${y - 6} L${x - 12},${y + 10} Q${x - 10},${y + 26} ${x},${y + 28} Q${x + 10},${y + 26} ${x + 12},${y + 10} L${x + 12},${y - 6}`),
        F(`M${x - 7},${y + 2} L${x - 3},${y + 2} M${x + 3},${y + 2} L${x + 7},${y + 2} M${x},${y + 4} L${x - 2},${y + 12} L${x + 1},${y + 12} M${x - 4},${y + 17} L${x + 4},${y + 17}`),
      ]),
      M("M0,196 L200,196"), F("M18,190 L182,190"),
    ] },
    { name: "Süleymaniye Camii", paths: [
      M(dome(100, 94, 38, 34)), alem(100, 60, 16), M(rect(62, 94, 76, 10)),
      M(dome(56, 108, 20, 16)), M(dome(144, 108, 20, 16)),
      ...[38, 58, 78, 122, 142, 162].map((x) => F(dome(x, 124, 9, 8))),
      M(rect(26, 108, 148, 66)), ...arches(166, [44, 66, 88, 112, 134, 156], 8, 18),
      ...minaret(8, 14, 186), F("M3,44 L13,44 L13,47 L3,47 Z"), ...minaret(24, 40, 186), ...minaret(176, 40, 186), ...minaret(192, 14, 186), F("M187,44 L197,44 L197,47 L187,47 Z"),
      M("M0,190 L200,190"),
    ] },
    { name: "Otağ", paths: [
      M("M28,186 L28,128 Q100,58 172,128 L172,186"), M(dome(100, 80, 14, 8)),
      ...[40, 60, 80, 120, 140, 160].map((x) => F(`M100,${76},L${x},${x < 100 ? 132 - (x - 28) * 0.1 : 132 - (172 - x) * 0.1}`.replace(",L", " L"))),
      F("M28,132 L172,132"),
      ...[0, 1, 2, 3, 4, 5, 6].flatMap((i) => [F(`M${28 + i * 20},132 L${48 + i * 20},186`), F(`M${48 + i * 20},132 L${28 + i * 20},186`)]).slice(0, 14),
      M("M86,186 L86,146 L114,146 L114,186"), F("M100,146 L100,186"),
      M("M10,186 L190,186"), F("M100,72 L100,58 L112,62 L100,66"),
    ] },
    { name: "Pamukkale", paths: [
      // a white hillside of travertine stepped into terraces of uneven length, each a row of
      // pool rims of uneven width with water on top and short curtains of lime below; the
      // sides follow the ends of the terraces
      M("M0,56 Q46,36 96,44 Q146,32 200,52"),
      M("M70,62 Q58,78 52,92 Q66,104 60,114 Q36,124 28,136 Q44,148 40,158 Q20,168 14,180 L12,192"),
      M("M132,62 Q152,80 150,92 Q178,102 176,114 Q164,126 168,136 Q194,146 190,158 Q174,168 178,180 L182,192"),
      ...(() => {
        const tiers = [[70, 70, 132], [92, 52, 150], [114, 60, 176], [136, 28, 168], [158, 40, 190], [180, 14, 178]];
        const widths = [1, 1.5, 0.8, 1.25, 0.95, 1.4, 0.85, 1.1, 1.3, 0.9];
        return tiers.flatMap(([y, l, r], t) => {
          const n = 3 + Math.round((r - l) / 34);
          const ws = [...Array(n)].map((_, i) => widths[(t * 3 + i) % widths.length]);
          const sum = ws.reduce((a, b) => a + b, 0);
          let x = l, rim = `M${l},${y}`;
          const out = [];
          ws.forEach((w, i) => {
            const x2 = x + (r - l) * w / sum, mid = (x + x2) / 2;
            rim += ` Q${mid.toFixed(1)},${y + 9} ${x2.toFixed(1)},${y}`;
            for (const f of [-0.2, 0.08]) {
              const cx = mid + f * (x2 - x);
              out.push(F(`M${cx.toFixed(1)},${y + 6} L${cx.toFixed(1)},${(y + 11 + ((i + t) % 3) * 3).toFixed(1)}`));
            }
            x = x2;
          });
          out.unshift(M(rim));
          out.push(Wv(`M${l + 5},${y - 3} Q${(l + r) / 2},${y - 6} ${r - 5},${y - 3}`));
          return out;
        });
      })(),
      M("M0,196 L200,196"),
    ] },
    { name: "Selimiye Camii", paths: [
      M(dome(100, 96, 44, 44)), alem(100, 52, 16), M(rect(54, 96, 92, 12)),
      F("M52,108 L52,84 L48,84 M148,108 L148,84 L152,84"),
      M(rect(36, 108, 128, 62)), ...arches(162, [52, 76, 100, 124, 148], 9, 20),
      ...minaret(26, 6, 186), F("M21,70 L31,70 L31,73 L21,73 Z"), ...minaret(44, 10, 186), F("M39,72 L49,72 L49,75 L39,75 Z"),
      ...minaret(156, 10, 186), F("M151,72 L161,72 L161,75 L151,75 Z"), ...minaret(174, 6, 186), F("M169,70 L179,70 L179,73 L169,73 Z"),
      M("M0,190 L200,190"),
    ] },
    { name: "Truva Atı", paths: [
      M("M48,104 L146,104 L150,138 L46,138 Z"), M("M138,104 L154,54 L172,44 L186,56 L176,66 L166,66 L156,108"),
      F("M172,44 L168,34 L176,40"), F(circle(174, 52, 2)),
      M("M58,138 L56,170 M74,138 L76,170 M122,138 L120,170 M138,138 L140,170"),
      M("M48,112 Q30,126 34,150"), F("M70,104 L70,138 M94,104 L94,138 M118,104 L118,138"), F(rect(84, 112, 20, 14)),
      M(rect(36, 170, 128, 8)), M(circle(54, 186, 8)), M(circle(146, 186, 8)), F(circle(54, 186, 2)), F(circle(146, 186, 2)),
      F("M0,196 L200,196"),
    ] },
    { name: "Mevlana Türbesi", paths: [
      M("M82,74 L100,16 L118,74"), ...[86, 90, 94, 106, 110, 114].map((x) => F(`M${x},74 L100,16`)), alem(100, 16, 14),
      M(rect(80, 74, 40, 56)), ...arches(110, [92, 108], 6, 14),
      M(dome(56, 140, 20, 18)), M(dome(144, 140, 20, 18)),
      M(rect(28, 130, 144, 56)), ...arches(178, [48, 72, 128, 152], 9, 22), F("M88,186 L88,150 Q100,138 112,150 L112,186"),
      M("M0,190 L200,190"),
    ] },
    { name: "Çanakkale Şehitler Abidesi", paths: [
      M(rect(18, 40, 164, 30)), F("M24,48 L176,48 M24,62 L176,62"),
      // two of the four square pillars in front, the space between them left open
      M(rect(30, 70, 34, 116)), M(rect(136, 70, 34, 116)), F("M38,70 L38,186 M162,70 L162,186"),
      M("M10,186 L190,186"), M("M4,192 L196,192"), F("M0,198 L200,198"),
    ] },
    { name: "Rumeli Hisarı", paths: [
      M("M0,196 Q60,176 100,182 Q150,176 200,160"),
      M("M18,190 L18,92 L56,92 L56,186"), M("M14,92 L37,58 L60,92"),
      M("M88,182 L88,108 L118,108 L118,180"), M("M84,108 L103,80 L122,108"),
      M("M146,174 L146,70 L190,70 L190,166"), M("M142,70 L168,32 L194,70"),
      F(crenel(56, 88, 134)), M("M56,134 L56,184 M88,134 L88,182"), F(crenel(118, 146, 126)),
      F("M118,126 L118,180 M146,126 L146,174"),
      ...arches(176, [37], 10, 18), ...arches(140, [168], 8, 16), F("M30,120 L44,120 M96,130 L110,130"),
      Wv("M0,198 Q20,194 40,198 T80,198"),
    ] },
    { name: "Türk yayı", paths: [
      M("M110,14 Q100,20 96,30 Q62,64 72,100 Q62,136 96,170 Q100,180 110,186"),
      F("M100,24 L100,176"),
      M("M30,100 L176,100"), M("M176,100 L166,94 L170,100 L166,106 Z"),
      F("M30,100 L22,92 M30,100 L22,108 M38,100 L30,92 M38,100 L30,108 M46,100 L38,92 M46,100 L38,108"),
      F("M66,96 Q72,100 66,104"),
    ] },
    { name: "Safranbolu evi", paths: [
      M(rect(52, 140, 96, 56)), M(rect(40, 100, 120, 40)), M(rect(30, 62, 140, 38)),
      M("M22,62 L100,30 L178,62"), M("M140,46 L140,26 L150,26 L150,50"),
      F("M52,140 L40,128 M148,140 L160,128 M40,100 L30,90 M160,100 L170,90"),
      ...[50, 72, 94, 116, 138].map((x) => F(rect(x, 108, 12, 22))), ...[42, 64, 86, 108, 130, 152].map((x) => F(rect(x, 70, 10, 20))),
      F("M86,196 L86,160 L114,160 L114,196"), F(rect(62, 154, 12, 16)), F(rect(126, 154, 12, 16)),
      M("M0,196 L200,196"),
    ] },
    { name: "Aspendos Tiyatrosu", paths: [
      F("M0,70 Q100,4 200,70"), F("M10,70 Q100,22 190,70"), F("M22,70 Q100,40 178,70"),
      M(rect(12, 58, 176, 132)), M("M12,58 L12,44 L34,44 L34,58 M166,58 L166,44 L188,44 L188,58"),
      F("M12,104 L188,104 M12,148 L188,148"),
      ...arches(96, [40, 60, 80, 100, 120, 140, 160], 8, 16), ...arches(140, [50, 150], 10, 20),
      M("M76,190 L76,158 Q86,146 96,158 L96,190"), M("M104,190 L104,158 Q114,146 124,158 L124,190"),
      M("M0,194 L200,194"),
    ] },
    { name: "Divriği kapısı", paths: [
      M(rect(30, 18, 140, 178)), F(rect(40, 28, 120, 168)),
      M("M62,196 L62,92 Q62,62 100,40 Q138,62 138,92 L138,196"),
      ...[0, 1, 2].flatMap((r) => [72, 86, 100, 114, 128].slice(r, 5 - r).map((x) => F(dome(x, 84 + r * 14, 6, 8)))),
      M(rect(80, 126, 40, 70)), F("M84,130 L116,130 M100,126 L100,196"),
      F(circle(50, 60, 6)), F(circle(150, 60, 6)), F("M46,110 L54,118 M154,110 L146,118"),
      M("M0,196 L200,196"),
    ] },
    { name: "Kalyon", paths: [
      M("M18,140 L182,140 L166,172 L38,172 Z"), M("M150,140 L150,118 L186,116 L182,140"), M("M18,140 L14,130 L40,130"),
      M("M60,140 L60,32"), M("M100,140 L100,18"), M("M140,118 L140,38"),
      M("M44,50 Q60,58 76,50 L78,86 Q60,94 42,86 Z"), M("M82,36 Q100,46 118,36 L120,80 Q100,90 80,80 Z"), M("M126,58 Q140,64 154,58 L156,94 Q140,100 124,94 Z"),
      F("M18,140 L60,32 L100,18 L140,38 L186,116"), F("M60,32 L72,28 L60,24 M100,18 L112,14 L100,10"),
      ...[50, 80, 110, 140].map((x) => F(circle(x, 156, 3))),
      Wv("M0,180 Q25,174 50,180 T100,180 T150,180 T200,180"), Wv("M10,192 Q35,186 60,192 T110,192 T160,192"),
    ] },
    { name: "Ahlat kümbeti", paths: [
      M("M58,82 L100,20 L142,82"), M("M58,82 L58,180 M142,82 L142,180"), M("M58,180 Q100,196 142,180"), F("M58,82 Q100,98 142,82"),
      ...[72, 86, 100, 114, 128].map((x) => F(`M${x},${84 + Math.abs(x - 100) * -0.1 + 8} L${x},176`)),
      ...arches(140, [80, 120], 8, 22), F("M88,182 L88,150 Q100,138 112,150 L112,184"),
      F("M58,108 Q100,124 142,108"),
      M("M40,186 L160,186 M30,194 L170,194"), F("M0,198 L200,198"),
    ] },
    { name: "Anıtkabir", paths: [
      M(rect(36, 72, 128, 8)), M(rect(30, 80, 140, 12)),
      ...columns([40, 52, 64, 76, 88, 100, 112, 124, 136, 148, 160], 92, 150),
      M(rect(26, 150, 148, 22)), M("M14,172 L186,172 M8,180 L192,180 M2,188 L198,188"),
      F("M0,196 L200,196"), F("M90,150 L90,128 L110,128 L110,150"),
    ] },
    { name: "Yivli Minare", paths: [
      M("M86,186 L86,52 M114,52 L114,186"), ...fluted(86, 114, 56, 160, 5),
      M("M80,52 L120,52 L116,44 L84,44 Z"), M("M88,44 L100,14 L112,44"), F("M100,14 L100,4"),
      M(rect(74, 160, 52, 36)), F("M74,168 L126,168"),
      M(dome(46, 170, 22, 20)), M(dome(154, 170, 22, 20)), M("M20,170 L74,170 M126,170 L180,170"), F("M24,196 L24,170 M68,196 L68,170 M132,196 L132,170 M176,196 L176,170"),
      M("M0,196 L200,196"),
    ] },
    { name: "Tuğra", paths: [
      // the three tuğs, their flags (zülfe) blowing to the right
      M("M92,140 L92,16"), M("M106,140 L106,26"), M("M120,140 L120,36"),
      F("M92,16 Q104,22 98,36 M106,26 Q118,32 112,46 M120,36 Q132,42 126,56"),
      // the two loops (beyze) to the left, one inside the other
      M("M92,138 C60,150 10,140 10,112 C10,84 60,80 92,106"), M("M92,128 C66,136 30,130 30,112 C30,96 64,94 92,114"),
      // the arms (kol) reaching to the right
      M("M120,110 Q160,96 196,104"), M("M120,126 Q164,114 196,124"),
      // the name (sere) at the bottom
      M("M40,156 Q70,172 110,160 Q140,150 168,164"), F("M62,150 Q74,140 86,150 M118,148 Q126,140 134,150"), F(circle(150, 176, 3)),
    ] },
    { name: "Bodrum Kalesi", paths: [
      M("M0,176 Q40,160 80,164 Q130,156 200,170"),
      M("M120,160 L120,44 L160,44 L160,154"), M(crenel(116, 164, 44, 6)),
      M("M62,164 L62,80 L92,80 L92,162"), M(crenel(58, 96, 80, 5)),
      M("M18,170 L18,112 L40,112 L40,166"), F(crenel(40, 62, 124, 5)), F("M40,124 L40,166 M62,124 L62,164 M92,118 L120,118"), F(crenel(92, 120, 118, 5)),
      ...arches(70, [140], 8, 18), ...arches(110, [77], 6, 14), F("M136,96 L144,96 M136,124 L144,124"),
      Wv("M0,186 Q25,180 50,186 T100,186 T150,186 T200,186"), Wv("M14,196 Q39,190 64,196 T114,196 T164,196"),
    ] },
    { name: "İnce belli çay bardağı", paths: [
      M("M68,58 C70,86 86,104 84,120 C82,138 70,148 72,164 C73,172 78,176 84,176 L116,176 C122,176 127,172 128,164 C130,148 118,138 116,120 C114,104 130,86 132,58 Z"),
      F("M72,76 C76,92 88,106 87,120 M128,76 C124,92 112,106 113,120"),
      M("M71,70 Q100,76 129,70"),
      M("M34,182 Q100,198 166,182 Q100,170 34,182 Z"), F("M62,180 Q100,188 138,180"),
      F("M92,48 Q86,38 94,28 Q102,18 96,8 M108,48 Q102,38 110,28 Q118,18 112,8"),
    ] },
    { name: "Sultanhanı Kervansarayı", paths: [
      M("M4,186 L4,100 L196,100 L196,186"), F(crenel(4, 196, 100, 5)),
      ...[20, 52, 148, 180].map((x) => M(`M${x - 7},186 L${x - 7},96 L${x + 7},96 L${x + 7},186`)),
      M("M74,186 L74,56 L126,56 L126,186"), F(rect(80, 62, 40, 124)),
      M("M86,186 L86,120 Q86,96 100,84 Q114,96 114,120 L114,186"),
      ...[90, 100, 110].map((x) => F(dome(x, 112, 4, 6))), ...[95, 105].map((x) => F(dome(x, 102, 4, 6))),
      M("M0,190 L200,190"),
    ] },
    { name: "Nazar boncuğu", paths: [
      M(circle(100, 108, 76)), M(circle(100, 108, 54)), M(circle(100, 108, 32)), F(circle(100, 108, 14)),
      F("M100,32 L100,14 M94,20 L100,14 L106,20"), F(circle(100, 10, 4)),
      F("M86,100 Q90,94 96,96"),
    ] },
    { name: "İshak Paşa Sarayı", paths: [
      M("M0,196 L30,170 Q70,150 110,156 Q160,150 200,170"),
      M("M26,172 L26,110 L132,110 L132,156"), F(crenel(26, 132, 110, 4)),
      M(dome(110, 110, 18, 16)), alem(110, 94, 12),
      M("M150,156 L150,46 M158,46 L158,154"), M("M146,46 L162,46 L154,26 Z"), F("M146,70 L162,70 L162,73 L146,73 Z"),
      M("M60,110 L60,74 L92,74 L92,110"), M(dome(76, 74, 16, 14)),
      ...arches(140, [44, 70, 96], 8, 18), ...arches(104, [76], 6, 14),
      F("M166,164 L180,140 L196,168"),
    ] },
    { name: "Lale", paths: [
      M("M100,24 Q122,64 110,108 Q100,116 90,108 Q78,64 100,24 Z"),
      M("M96,108 Q58,100 60,44 Q80,70 92,100"), M("M104,108 Q142,100 140,44 Q120,70 108,100"),
      M("M100,114 Q106,150 96,192"),
      M("M98,170 Q60,160 44,120 Q76,140 98,158"), M("M100,184 Q140,170 150,132 Q128,156 101,172"),
      F("M100,40 L100,100"),
    ] },
    { name: "Yerebatan Sarnıcı", paths: [
      ...[16, 52, 88, 124, 160, 196].map((x) => M(`M${x - 8},176 L${x - 8},92`)),
      ...[16, 52, 88, 124, 160].map((x) => M(`M${x - 8},92 Q${x + 10},62 ${x + 28},92`)),
      ...[34, 70, 106, 142, 178].map((x) => F(`M${x - 8},150 L${x - 8},108`)),
      ...[34, 70, 106, 142].map((x) => F(`M${x - 8},108 Q${x + 10},88 ${x + 28},108`)),
      ...[8, 44, 80, 116, 152, 188].map((x) => F(`M${x - 4},96 L${x + 4},96`)),
      Wv("M0,182 Q25,176 50,182 T100,182 T150,182 T200,182"), Wv("M10,194 Q35,188 60,194 T110,194 T160,194"),
    ] },
    { name: "Ağrı Dağı", paths: [
      M("M0,186 L44,140 L74,96 L100,40 L124,84 L142,96 L156,82 L176,112 L200,138"),
      M("M84,74 L100,40 L118,70 L108,66 L100,76 L92,66 Z"), F("M150,90 L156,82 L162,92"),
      F("M30,60 Q40,50 54,56 Q64,46 76,56 M126,40 Q136,32 148,38 Q156,30 168,38"),
      F("M44,140 L60,150 L74,140 M124,84 L132,110"),
      M("M0,196 L200,196"),
    ] },
    { name: "Traianus Tapınağı", paths: [
      M("M30,62 L100,28 L170,62 Z"), F("M48,58 L100,34 L152,58"),
      M(rect(26, 62, 148, 10)), ...columns([38, 62, 86, 114, 138, 162], 72, 160),
      ...[38, 62, 86, 114, 138, 162].map((x) => F(`M${x - 5},72 L${x + 5},72 M${x - 5},160 L${x + 5},160`)),
      M(rect(20, 160, 160, 14)), M("M12,174 L188,174 M6,182 L194,182"), F("M0,190 L200,190"),
    ] },
  ];
})();

function graphBackdrop() {
  const NS = "http://www.w3.org/2000/svg";
  const el = (name, attrs, ...kids) => {
    const e = document.createElementNS(NS, name);
    for (const [k, v] of Object.entries(attrs || {})) e.setAttribute(k, v);
    for (const k of kids) if (k) e.append(k);
    return e;
  };
  const W = 1600, H = 900, SCALE = 2, N = 110;
  const still = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const svg = el("svg", { class: "signin-backdrop", viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: "xMidYMid slice", "aria-hidden": "true", focusable: "false" });

  // a drawing's fine line art (shown once the nodes are in place), and its graph: points
  // along every line and inside its closed shapes, the edges that follow its lines from
  // point to point, and links from each point to its nearest neighbour on another line
  const lay = (layer, m) => {
    const art = el("g", { class: "gb-art" });
    layer.append(art);
    const points = []; // [x, y, line, sequence]
    const step = { main: 15, fine: 18, wave: 20 };
    m.paths.forEach(({ d, kind }, k) => {
      const p = el("path", { d, class: `gb-line ${kind}` });
      art.append(p);
      const len = p.getTotalLength();
      const closed = d.trim().endsWith("Z");
      // small details (letters, little arches) stay line art only
      const n = kind === "fine" && len < 40 ? -1 : Math.max(1, Math.round(len / step[kind]));
      for (let i = 0; i <= n; i++) {
        if (i === n && closed) break;
        const pt = p.getPointAtLength(len * i / n);
        points.push([pt.x, pt.y, k, i]);
      }
      if (closed && kind === "main" && p.isPointInFill) {
        const box = p.getBBox();
        for (let y = box.y + 14; y < box.y + box.height; y += 28) {
          for (let x = box.x + 14; x < box.x + box.width; x += 28) {
            // a fixed scatter, so the same drawing looks the same each time
            const jx = x + (((x * 7 + y * 13) % 11) - 5), jy = y + (((x * 17 + y * 5) % 11) - 5);
            if (p.isPointInFill(new DOMPoint(jx, jy))) points.push([jx, jy, -1 - k, 0]);
          }
        }
      }
    });
    // exactly N points: evenly chosen when there are more, repeated when there are fewer
    const out = [];
    for (let i = 0; i < N; i++) out.push(points[Math.floor(i * points.length / N)]);
    // edges along the lines: consecutive points of the same line
    const edges = [];
    for (let i = 1; i < N; i++) {
      const [x, y, l, q] = out[i], [x0, y0, l0, q0] = out[i - 1];
      if (l >= 0 && l === l0 && q !== q0 && Math.hypot(x - x0, y - y0) < 45) edges.push([i - 1, i]);
    }
    const links = [];
    const seen = new Set();
    out.forEach(([x, y, line], i) => {
      let best = -1, bestD = 26;
      out.forEach(([x2, y2, l2], j) => {
        const dd = Math.hypot(x2 - x, y2 - y);
        if (j !== i && l2 !== line && dd > 0.5 && dd < bestD) { best = j; bestD = dd; }
      });
      const key = i < best ? `${i}-${best}` : `${best}-${i}`;
      if (best >= 0 && !seen.has(key)) { seen.add(key); links.push([i, best]); }
    });
    return { art, points: out, edges, links };
  };
  // where a point falls on a Z-order curve: nearby points get nearby keys, so matching
  // two drawings' points in this order moves every node only a short way
  const zkey = ([x, y]) => {
    let k = 0;
    const xi = Math.max(0, Math.min(255, Math.round(x + 28))), yi = Math.max(0, Math.min(255, Math.round(y + 28)));
    for (let b = 7; b >= 0; b--) k = k * 4 + (((yi >> b) & 1) << 1) + ((xi >> b) & 1);
    return k;
  };
  const ease = (t) => (t < 0.5 ? 4 * t * t * t : 1 - (-2 * t + 2) ** 3 / 2);

  // one side: a fixed set of nodes, with the edges between them, that turns from one
  // drawing into the next; the edges move with the nodes
  const side = ([x, y], first) => {
    const slot = el("g", { transform: `translate(${x - 100 * SCALE},${y - 100 * SCALE}) scale(${SCALE})` });
    const artLayer = el("g", {});
    const oldEdges = el("path", { class: "gb-skel" }), oldLinks = el("path", { class: "gb-link" });
    const newEdges = el("path", { class: "gb-skel" }), newLinks = el("path", { class: "gb-link" });
    const nodeLayer = el("g", {});
    slot.append(artLayer, oldLinks, newLinks, oldEdges, newEdges, nodeLayer);
    svg.append(slot);
    const nodes = [...Array(N)].map((_, i) => {
      // most nodes plain; some in the colors of subject, predicate and object, for good
      const role = i % 5 === 0 ? ["s", "p", "o"][(i / 5) % 3] : "";
      const c = el("circle", { r: role ? 1.7 : 1.2, class: `gb-node ${role}` });
      nodeLayer.append(c);
      return c;
    });
    // they start gathered loosely in the middle, unseen
    let pos = nodes.map((_, i) => [100 + ((i * 37) % 60) - 30, 100 + ((i * 53) % 60) - 30]);
    let current = { edges: [], links: [] };
    let opacity = 0;
    const pathOf = (pairs, at) => pairs.map(([a, b]) => `M${at[a][0].toFixed(1)},${at[a][1].toFixed(1)}L${at[b][0].toFixed(1)},${at[b][1].toFixed(1)}`).join("");
    const paint = (at, fade) => {
      nodes.forEach((c, i) => { c.setAttribute("cx", at[i][0].toFixed(1)); c.setAttribute("cy", at[i][1].toFixed(1)); });
      nodeLayer.style.opacity = opacity;
      oldEdges.setAttribute("d", pathOf(current.edges, at));
      oldLinks.setAttribute("d", pathOf(current.links, at));
      oldEdges.style.opacity = oldLinks.style.opacity = 1 - fade;
    };
    let k = first;
    const become = () => {
      if (!svg.isConnected) return;
      const m = MOTIFS[k % MOTIFS.length];
      k += 2;
      for (const old of [...artLayer.children]) {
        old.classList.remove("on");
        setTimeout(() => old.remove(), 1200);
      }
      const next = lay(artLayer, m);
      // each node goes to the target its rank on the Z curve gives it
      const from = nodes.map((_, i) => i).sort((a, b) => zkey(pos[a]) - zkey(pos[b]));
      const to = next.points.map((_, i) => i).sort((a, b) => zkey(next.points[a]) - zkey(next.points[b]));
      const node = new Array(N); // target index → node
      from.forEach((ni, r) => { node[to[r]] = ni; });
      const target = new Array(N);
      next.points.forEach((p, ti) => { target[node[ti]] = [p[0], p[1]]; });
      const edges = next.edges.map(([a, b]) => [node[a], node[b]]);
      const links = next.links.map(([a, b]) => [node[a], node[b]]);
      const start = pos.map((p) => p.slice());
      const DUR = still ? 0 : 2600, t0 = performance.now();
      const frame = (now) => {
        if (!svg.isConnected) return;
        const t = DUR ? Math.min(1, (now - t0) / DUR) : 1;
        // nodes leave a little one after another, and all arrive together
        const at = start.map(([sx, sy], i) => {
          const lag = (i % 17) / 17 * 0.25;
          const e = ease(Math.max(0, Math.min(1, (t - lag) / (1 - lag))));
          return [sx + (target[i][0] - sx) * e, sy + (target[i][1] - sy) * e];
        });
        opacity = Math.max(opacity, Math.min(1, t * 2));
        paint(at, Math.min(1, t * 1.6));
        // the new drawing's edges come up as its shape forms
        newEdges.setAttribute("d", pathOf(edges, at));
        newLinks.setAttribute("d", pathOf(links, at));
        newEdges.style.opacity = newLinks.style.opacity = ease(Math.max(0, (t - 0.35) / 0.65));
        if (t < 1) return requestAnimationFrame(frame);
        pos = target;
        current = { edges, links };
        paint(pos, 0);
        newEdges.setAttribute("d", "");
        newLinks.setAttribute("d", "");
        // the line art settles in last
        next.art.classList.add("on");
        if (!still) setTimeout(become, 7400);
      };
      requestAnimationFrame(frame);
    };
    requestAnimationFrame(become);
  };
  // drawn once the page is on screen (lengths need a laid-out drawing); the two sides
  // change half a cycle apart
  requestAnimationFrame(() => {
    side([300, 440], 0);
    if (still) side([1300, 440], 1);
    else setTimeout(() => side([1300, 440], 1), 5000);
  });
  return svg;
}

function showLogin() {
  if ($("#login")) return;
  const user = h("input", { type: "text", id: "login-user", autocomplete: "username", required: true });
  const pass = h("input", { type: "password", id: "login-pass", autocomplete: "current-password", required: true });
  const msg = h("div", { class: "status err", role: "alert" });
  const dlg = h("div", { id: "login", class: "signin" },
    graphBackdrop(),
    h("main", { class: "signin-main" },
      h("form", { class: "dialog signin-card", onsubmit: async (e) => {
        e.preventDefault();
        msg.textContent = "";
        try {
          const res = await fetch("/rest/login", { method: "POST", headers: { ...JSONH, "X-GraphRD-UI": "1" }, body: JSON.stringify({ username: user.value, password: pass.value }) });
          if (!res.ok) throw new Error("That username and password don't match.");
          auth.token = res.headers.get("Authorization");
          dlg.remove();
          updateAuthUi();
          await loadRepos();
          render();
        } catch (err) { msg.textContent = err.message; }
      } },
      h("div", { class: "signin-head" }, $(".mark").cloneNode(true), h("h2", {}, "Sign in to GraphRD"), h("p", {}, "RDF graph database"), h("span", { class: "signin-host" }, location.host)),
      h("label", { class: "field" }, "Username", user),
      h("label", { class: "field" }, "Password", pass),
      msg,
      h("button", { class: "primary signin-go", type: "submit" }, "Sign in")),
      h("div", { class: "signin-foot" }, "GraphRD · SPARQL 1.1 and 1.2 · RDF 1.2")));
  document.body.append(dlg);
  setTimeout(() => user.focus(), 0);
}

async function updateAuthUi() {
  $("#logout").hidden = !auth.token;
  let admin = false, open = false;
  try {
    const sec = await (await fetch("/rest/security/all", { headers: { "X-GraphRD-UI": "1" } })).json();
    open = !sec.enabled;
    if (sec.enabled && auth.token) {
      const me = await getJSON("/rest/security/authenticated-user");
      admin = (me.grantedAuthorities || me.authorities || []).includes("ROLE_ADMIN");
    }
  } catch { /* not signed in */ }
  $("#nav-users").hidden = !admin;
  // the AI assistant's settings are for administrators (everyone, when sign-in is off)
  $("#nav-llm").hidden = !(admin || open);
}

// ------------------------------------------------------------------ boot

function applyTheme(t) {
  if (t) document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
}
applyTheme(store.get("graphrd.theme", null));
$("#theme-toggle").addEventListener("click", () => {
  const current = document.documentElement.dataset.theme || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  const next = current === "dark" ? "light" : "dark";
  store.set("graphrd.theme", next);
  applyTheme(next);
});
$("#repo-select").addEventListener("change", async (e) => { setRepo(e.target.value); await loadNamespaces(); render(); });
$("#logout").addEventListener("click", async () => {
  try { await fetch("/rest/logout", { method: "POST", headers: { Authorization: auth.token || "", "X-GraphRD-UI": "1" } }); } catch { /* ignore */ }
  auth.token = null;
  updateAuthUi();
  showLogin();
});
window.addEventListener("hashchange", render);
updateAuthUi();

// The cluster page exists only when this server runs in cluster mode.
fetch("/rest/cluster/status", { headers: { "X-GraphRD-UI": "1" } }).then((res) => { if (res.status === 200) $("#nav-cluster").hidden = false; }).catch(() => {});

loadRepos().then(render).catch((e) => {
  if (e instanceof AuthRequired) return;
  view.replaceChildren(h("div", { class: "page" }, panel("GraphRD is not reachable", null, h("div", { class: "panel-body" }, h("pre", { class: "error-box" }, e.message)))));
});

// ------------------------------------------------------------------ visual graph
// A node-link view of the data: start from one resource, a named graph, or the schema
// (classes linked by the predicates between their instances); expand nodes to explore.
// GraphRD's own engine, no third-party code like the rest of the UI: a canvas renderer,
// a Barnes–Hut force layout plus radial and hierarchy layouts, a minimap, search, and
// level-of-detail labels.

const LABEL_PREDICATES = [
  "http://www.w3.org/2000/01/rdf-schema#label", "http://www.w3.org/2004/02/skos/core#prefLabel",
  "http://xmlns.com/foaf/0.1/name", "http://schema.org/name", "https://schema.org/name", "http://purl.org/dc/terms/title",
];
const VG_MAX_NODES = 3000;
const SVG_NS = "http://www.w3.org/2000/svg";
const svgEl = (tag, attrs = {}) => {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
  return el;
};

routes.visual = async (params) => {
  let mode = params.mode || (params.graph ? "graph" : params.iri ? "node" : "schema");
  const vg = createVisualGraph();
  const ico = (d, extra = "") => {
    const s = svgEl("svg", { viewBox: "0 0 20 20", "aria-hidden": "true", class: "vg-i" + extra });
    for (const path of [].concat(d)) s.append(svgEl("path", { d: path }));
    return s;
  };
  const I = {
    graph: ["M6 6l8 2M6 6l2 8M14 8l-6 6", "M6 6m-2.2 0a2.2 2.2 0 1 0 4.4 0a2.2 2.2 0 1 0-4.4 0", "M14 8m-2.2 0a2.2 2.2 0 1 0 4.4 0a2.2 2.2 0 1 0-4.4 0", "M8 14m-2.2 0a2.2 2.2 0 1 0 4.4 0a2.2 2.2 0 1 0-4.4 0"],
    node: ["M10 10m-3 0a3 3 0 1 0 6 0a3 3 0 1 0-6 0", "M10 3v2M10 15v2M3 10h2M15 10h2"],
    named: ["M3.5 5.5h13v9h-13z", "M3.5 8.5h13"],
    schema: ["M4 4h5v4H4zM11 12h5v4h-5z", "M6.5 8v6h4.5"],
    search: ["M9 9m-5 0a5 5 0 1 0 10 0a5 5 0 1 0-10 0", "M12.6 12.6L16.5 16.5"],
    arrow: "M5 10h10M11 6l4 4-4 4",
    force: ["M5 6m-1.8 0a1.8 1.8 0 1 0 3.6 0a1.8 1.8 0 1 0-3.6 0", "M15 7m-1.8 0a1.8 1.8 0 1 0 3.6 0a1.8 1.8 0 1 0-3.6 0", "M9 15m-1.8 0a1.8 1.8 0 1 0 3.6 0a1.8 1.8 0 1 0-3.6 0", "M6.7 6.4l6.5.5M14 8.6l-4 5M6 7.6l2.2 5.6"],
    radial: ["M10 10m-1.6 0a1.6 1.6 0 1 0 3.2 0a1.6 1.6 0 1 0-3.2 0", "M10 10m-6 0a6 6 0 1 0 12 0a6 6 0 1 0-12 0"],
    tree: ["M10 3.5v4M10 7.5H5v4M10 7.5h5v4", "M5 13.5m-1.6 0a1.6 1.6 0 1 0 3.2 0a1.6 1.6 0 1 0-3.2 0", "M15 13.5m-1.6 0a1.6 1.6 0 1 0 3.2 0a1.6 1.6 0 1 0-3.2 0"],
    download: "M10 3.5v9M6 9l4 4 4-4M4 16.5h12",
    clear: ["M4 6h12M8 6V4h4v2M6 6l1 10h6l1-10"],
    help: ["M10 10m-7 0a7 7 0 1 0 14 0a7 7 0 1 0-14 0", "M8 8a2 2 0 1 1 2.8 1.8c-.6.3-.8.7-.8 1.4M10 13.6v.1"],
  };

  // ---- source: where the graph starts
  const target = h("input", { type: "text", class: "vg-source-input", value: params.iri || "", placeholder: "Search a resource, or paste an IRI / prefix:name", list: "vg-suggest", "aria-label": "Resource to start from" });
  const suggest = h("datalist", { id: "vg-suggest" });
  const graphs = await http("GET", repoUrl("/contexts"), { accept: "application/sparql-results+json" })
    .then((r) => r.json()).then((j) => j.results.bindings.map((b) => b.contextID.value)).catch(() => []);
  const graphSel = h("select", { class: "vg-source-input", "aria-label": "Named graph" }, graphs.length
    ? graphs.map((g) => h("option", { value: g, selected: g === params.graph }, abbreviate(g).replace(/^<(.*)>$/, "$1")))
    : h("option", { value: "" }, "This repository has no named graphs"));
  const schemaNote = h("span", { class: "vg-source-note", title: "Click to search a resource instead" }, "Schema: classes and the links between them · click to search a resource");
  const limit = h("select", { class: "vg-limit", "aria-label": "Neighbours per expansion", title: "Neighbours added per expansion" },
    [25, 50, 100, 250, 500].map((n) => h("option", { value: n, selected: n === 50 }, `${n} / expand`)));

  const segmented = (label, items, value, onPick) => {
    const box = h("div", { class: "vg-seg", role: "radiogroup", "aria-label": label });
    const sync = (v) => { for (const b of box.children) { const on = b.dataset.value === v; b.classList.toggle("on", on); b.setAttribute("aria-checked", on); } };
    for (const [v, text, icon, title] of items) {
      box.append(h("button", { type: "button", role: "radio", "data-value": v, title: title || text, onclick: () => { sync(v); onPick(v); } }, ico(icon), text ? h("span", {}, text) : null));
    }
    sync(value);
    box.select = (v) => { sync(v); onPick(v); };
    return box;
  };
  const syncSource = () => {
    target.hidden = mode !== "node";
    graphSel.hidden = mode !== "graph";
    schemaNote.hidden = mode !== "schema";
    limit.hidden = mode === "schema";
  };
  const modeSeg = segmented("Start from", [["node", "Resource", I.node], ["graph", "Named graph", I.named], ["schema", "Schema", I.schema]], mode,
    (v) => { mode = v; syncSource(); if (v === "node") target.focus(); });
  syncSource();

  let timer = null;
  target.addEventListener("input", () => {
    clearTimeout(timer);
    const v = target.value.trim();
    if (v.length < 2) return;
    timer = setTimeout(async () => {
      const r = await getJSON(`/rest/autocomplete/query?repository=${enc(state.repo)}&q=${enc(v.split(":").pop())}`).catch(() => ({ suggestions: [] }));
      suggest.replaceChildren(...r.suggestions.slice(0, 20).map((s) => h("option", { value: s.value })));
    }, 150);
  });
  const resolve = (v) => {
    v = v.trim().replace(/^<|>$/g, "");
    const pn = v.match(/^([\w-]*):(\S*)$/);
    const ns = pn && state.namespaces.find(([p]) => p === pn[1]);
    return ns ? ns[1] + pn[2] : v;
  };

  // ---- view toggles
  const flags = { literals: false, edges: true, infer: true };
  const toggle = (k, text, title, onChange) => {
    const b = h("button", { type: "button", class: "vg-toggle", "aria-pressed": String(flags[k]), title, onclick: () => {
      flags[k] = !flags[k];
      b.setAttribute("aria-pressed", String(flags[k]));
      onChange?.(flags[k]);
    } }, h("i"), text);
    return b;
  };

  const start = async () => {
    vg.opts.limit = Number(limit.value);
    vg.opts.literals = flags.literals;
    vg.opts.infer = flags.infer;
    vg.clear();
    try {
      if (mode === "node") {
        const iri = resolve(target.value);
        if (!iri) { target.focus(); return toast("Enter a resource to start from", true); }
        await vg.expand({ type: "uri", value: iri }, true);
      } else if (mode === "graph") {
        if (!graphSel.value) return toast("This repository has no named graphs", true);
        await vg.loadGraph(graphSel.value);
      } else {
        await vg.loadSchema();
      }
    } catch (e) {
      toast(e.message || String(e), true);
    }
  };
  target.addEventListener("keydown", (e) => { if (e.key === "Enter") start(); });

  const help = h("div", { class: "vg-help", hidden: true, role: "dialog", "aria-label": "Shortcuts" },
    h("div", { class: "eyebrow" }, "Mouse"),
    h("dl", {},
      [["Hover", "light up a neighbourhood"], ["Click", "inspect a node"], ["Double-click", "expand a node"], ["Right-click", "more actions"], ["Drag", "move and pin a node, or pan"], ["Scroll / pinch", "zoom"]]
        .map(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
    h("div", { class: "eyebrow" }, "Keyboard"),
    h("dl", {},
      [["/", "find in the view"], ["F", "fit to the view"], ["E or Enter", "expand the selection"], ["P", "pin or unpin"], ["Del", "hide"], ["Tab", "next node by importance"], ["+ −", "zoom"], ["Esc", "clear the selection"]]
        .map(([k, v]) => [h("dt", {}, h("kbd", {}, k)), h("dd", {}, v)])));
  const helpBtn = h("button", { type: "button", class: "vg-tool", title: "Shortcuts", "aria-label": "Shortcuts", onclick: (e) => { e.stopPropagation(); help.hidden = !help.hidden; } }, ico(I.help));
  document.addEventListener("pointerdown", (e) => { if (!help.hidden && !help.contains(e.target) && e.target !== helpBtn && !helpBtn.contains(e.target)) help.hidden = true; });

  // the whole field is a search box: a click anywhere in it (icon, padding, or the schema
  // note) puts the cursor in the resource search, switching to it from the schema view
  const source = h("div", { class: "vg-source", onpointerdown: (e) => {
    if (e.target.closest("button, select") || e.target === target) return;
    e.preventDefault();
    if (mode === "schema") modeSeg.select("node");
    if (mode === "node") target.focus();
  } },
    ico(I.search, " vg-source-icon"),
    target, suggest, graphSel, schemaNote,
    limit,
    h("button", { type: "button", class: "vg-go", onclick: start }, h("span", {}, "Explore"), ico(I.arrow)));
  const toolbar = h("header", { class: "vg-toolbar" },
    h("div", { class: "vg-brand" }, h("span", { class: "vg-mark" }, ico(I.graph)), h("h1", {}, "Visual Graph")),
    modeSeg,
    source,
    h("div", { class: "vg-view" },
      segmented("Layout", [["force", "", I.force, "Force layout"], ["radial", "", I.radial, "Radial layout"], ["hierarchy", "", I.tree, "Hierarchy layout"]], "force", (v) => vg.setLayout(v)),
      h("div", { class: "vg-toggles" },
        toggle("literals", "Literals", "Show literal values as nodes", (on) => { vg.opts.literals = on; vg.applyLiteralFilter(); }),
        toggle("edges", "Labels", "Show link labels", (on) => vg.showEdgeLabels(on)),
        toggle("infer", "Inferred", "Include inferred statements (applies to the next load or expansion)", (on) => { vg.opts.infer = on; })),
      h("div", { class: "vg-tools" },
        h("button", { type: "button", class: "vg-tool", title: "Download the view as PNG", "aria-label": "Download PNG", onclick: () => vg.exportPng() }, ico(I.download)),
        h("button", { type: "button", class: "vg-tool", title: "Clear the canvas", "aria-label": "Clear", onclick: () => vg.clear() }, ico(I.clear)),
        helpBtn, help)));
  // full-bleed: the graph takes the whole workspace instead of a boxed panel
  const page = h("div", { class: "vg-page" }, toolbar, h("div", { class: "vg-body" }, vg.canvas, vg.side));
  setTimeout(() => { if (mode === "schema" || params.iri || params.graph) start(); }, 0);
  return page;
};

function createVisualGraph() {
  const nodes = new Map(); // key -> node
  const edges = new Map(); // key -> edge
  const opts = { limit: 50, literals: false, infer: true };
  const typeColor = new Map();
  const PALETTE = ["--vg-1", "--vg-2", "--vg-3", "--vg-4", "--vg-5", "--vg-6", "--vg-7", "--vg-8"];

  // ------------------------------------------------------------ DOM
  const cv = h("canvas", { class: "vg-surface", tabindex: "0", role: "img", "aria-label": "Graph view" });
  const ctx = cv.getContext("2d");
  const mini = h("canvas", { class: "vg-minimap", "aria-label": "Overview" });
  const mctx = mini.getContext("2d");
  const legend = h("div", { class: "vg-legend" });
  const empty = h("div", { class: "vg-empty" }, "Nothing to show yet.");
  const tip = h("div", { class: "vg-tip", hidden: true });
  const menu = h("div", { class: "vg-menu", hidden: true, role: "menu" });
  const find = h("input", { type: "search", class: "vg-find", placeholder: "Find in view…  /", "aria-label": "Find a node in the view" });
  const stats = h("div", { class: "vg-stats" });
  const icon = (d) => {
    const s = svgEl("svg", { viewBox: "0 0 20 20", "aria-hidden": "true" });
    s.append(svgEl("path", { d }));
    return s;
  };
  const ctl = (title, d, fn) => h("button", { class: "vg-ctl", title, "aria-label": title, onclick: fn }, icon(d));
  const controls = h("div", { class: "vg-controls" },
    ctl("Zoom in (+)", "M10 4v12M4 10h12", () => zoomBy(1.4)),
    ctl("Zoom out (−)", "M4 10h12", () => zoomBy(1 / 1.4)),
    ctl("Fit to view (F)", "M3 7V3h4M13 3h4v4M17 13v4h-4M7 17H3v-4", () => fit(true)),
    ctl("Re-run layout", "M16 10a6 6 0 1 1-2-4.5M16 3v4h-4", () => relayout()),
    ctl("Full screen", "M7 3H3v4M13 3h4v4M3 13v4h4M17 13v4h-4", () => toggleFullscreen()));
  const canvas = h("div", { class: "vg-canvas" }, cv, h("div", { class: "vg-hud" }, find, stats), controls, mini, legend, empty, tip, menu);
  const side = h("aside", { class: "vg-side" }, h("p", { class: "muted small" }, "Select a node to see its details."));

  // ------------------------------------------------------------ state
  let view = { x: 0, y: 0, k: 1 };
  let width = 0, height = 0, dpr = 1;
  let selected = null, hovered = null, dragging = null;
  let alpha = 0, frame = null, layout = "force";
  let edgeLabels = true, fitWhenSettled = false, query = "";
  let tween = null; // camera animation
  let moves = null; // node position animation (radial / hierarchy)
  let colors = null;

  const key = (t) => termKey(t);
  const short = (t) => t.type === "uri" ? abbreviate(t.value).replace(/^<(.*)>$/, (_, i) => i.split(/[/#]/).filter(Boolean).pop() || i)
    : t.type === "bnode" ? `_:${t.value}` : t.value.length > 40 ? t.value.slice(0, 38) + "…" : t.value;
  const labelOf = (n) => n.label || short(n.term);
  const now = () => performance.now();
  const ease = (t) => (t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);
  const easeBack = (t) => { const c = 1.7; return 1 + (c + 1) * Math.pow(t - 1, 3) + c * Math.pow(t - 1, 2); };

  // theme colours, re-read when the theme changes
  function palette() {
    if (colors) return colors;
    const css = getComputedStyle(document.documentElement);
    const v = (n) => css.getPropertyValue(n).trim() || "#888";
    colors = { bg: v("--code-bg"), surface: v("--surface"), fg: v("--fg"), muted: v("--muted"), faint: v("--faint"), line: v("--line"),
      strong: v("--line-strong"), accent: v("--accent"), lit: v("--role-lit"), bnode: v("--role-b"), cls: v("--accent"), sans: v("--sans"), mono: v("--mono"),
      cat: PALETTE.map(v) };
    return colors;
  }
  new MutationObserver(() => { colors = null; invalidate(); }).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => { colors = null; invalidate(); });

  const colorIndex = (t) => {
    if (!typeColor.has(t)) typeColor.set(t, typeColor.size % PALETTE.length);
    return typeColor.get(t);
  };
  function colorFor(n) {
    const c = palette();
    if (n.term.type === "literal") return c.lit;
    if (n.term.type === "bnode") return c.bnode;
    // classes take their own colour, instances their most specific type's
    const t = n.cls ? n.term.value : n.primary;
    return t ? c.cat[colorIndex(t)] : c.faint;
  }
  // An instance's colour comes from its least common type among the shown nodes: with
  // inference on everything is also an Agent or a Thing, which tells nothing apart.
  function assignPrimaryTypes() {
    const freq = new Map();
    for (const n of nodes.values()) for (const t of n.types) freq.set(t, (freq.get(t) || 0) + 1);
    for (const n of nodes.values()) n.primary = n.types.slice().sort((a, b) => freq.get(a) - freq.get(b) || n.types.indexOf(a) - n.types.indexOf(b))[0];
  }
  const radius = (n) => n.cls ? Math.min(30, 11 + Math.sqrt(n.count || 1) * 0.9) : n.term.type === "literal" ? 5 : 7 + Math.min(9, Math.sqrt(n.degree) * 1.7);

  // ------------------------------------------------------------ model
  function addNode(term, extra = {}) {
    const k = key(term);
    let n = nodes.get(k);
    if (n) return n;
    if (nodes.size >= VG_MAX_NODES) return null;
    const angle = Math.random() * Math.PI * 2;
    const r = 30 + Math.random() * 50;
    const c = extra.near || { x: 0, y: 0 };
    n = { key: k, term, types: [], label: null, x: c.x + Math.cos(angle) * r, y: c.y + Math.sin(angle) * r, vx: 0, vy: 0,
      fixed: false, expanded: false, degree: 0, links: new Set(), born: now(), ...extra };
    delete n.near;
    nodes.set(k, n);
    return n;
  }

  function addEdge(s, p, o, extra = {}) {
    if (!s || !o) return;
    const k = s.key + "§" + p.value + "§" + o.key;
    if (edges.has(k)) return;
    const e = { key: k, s, p, o, label: extra.label || short(p), width: extra.width || 1, inferred: extra.inferred, born: now() };
    edges.set(k, e);
    s.links.add(e); o.links.add(e);
    s.degree++; o.degree++;
    // parallel edges between the same pair are drawn as a fan of curves
    const pair = [...s.links].filter((x) => (x.s === s && x.o === o) || (x.s === o && x.o === s));
    pair.forEach((x, i) => { x.lane = i; x.lanes = pair.length; });
  }

  function neighbours(n) {
    const out = new Set([n]);
    for (const e of n.links) { out.add(e.s); out.add(e.o); }
    return out;
  }

  async function describe(list) {
    // types and labels for the nodes in one query per 200
    const iris = list.filter((n) => n.term.type === "uri" && !n.described).map((n) => `<${n.term.value}>`);
    for (let i = 0; i < iris.length; i += 200) {
      const chunk = iris.slice(i, i + 200).join(" ");
      const rows = await select(`SELECT ?n ?t ?l WHERE { VALUES ?n { ${chunk} } { ?n a ?t } UNION { VALUES ?lp { ${LABEL_PREDICATES.map((x) => `<${x}>`).join(" ")} } ?n ?lp ?l } }`, { infer: opts.infer }).catch(() => []);
      for (const [n, t, l] of rows) {
        const node = nodes.get(key(n));
        if (!node) continue;
        if (t && !node.types.includes(t.value)) node.types.push(t.value);
        if (l && (!node.label || l["xml:lang"] === (navigator.language || "en").slice(0, 2))) node.label = l.value;
      }
    }
    for (const n of list) n.described = true;
    assignPrimaryTypes();
    renderLegend();
    invalidate();
  }

  async function expand(term, first = false) {
    const center = nodes.get(key(term)) || addNode(term);
    if (!center) return toast(`Showing at most ${fmt(VG_MAX_NODES)} nodes`, true);
    if (first) { center.fixed = true; center.x = 0; center.y = 0; }
    const node = sparqlTerm(term);
    const [out, inc] = await Promise.all([
      select(`SELECT ?p ?o WHERE { ${node} ?p ?o } LIMIT ${opts.limit * 4}`, { infer: opts.infer }),
      select(`SELECT ?s ?p WHERE { ?s ?p ${node} } LIMIT ${opts.limit}`, { infer: opts.infer }),
    ]);
    const added = [center];
    let shown = 0;
    for (const [p, o] of out) {
      if (p.value === RDF_TYPE) {
        if (!center.types.includes(o.value)) center.types.push(o.value);
        continue;
      }
      if (o.type === "literal" && !opts.literals) continue;
      if (shown++ >= opts.limit) break;
      const n = addNode(o, { near: center });
      if (n) { addEdge(center, p, n); added.push(n); }
    }
    for (const [s, p] of inc) {
      const n = addNode(s, { near: center });
      if (n) { addEdge(n, p, center); added.push(n); }
    }
    center.expanded = true;
    details.delete(center.key);
    empty.hidden = nodes.size > 0;
    await describe(added);
    if (first) { pick(center); fitWhenSettled = true; }
    else if (selected) pick(selected); // refresh the side panel's link counts
    // keep the newcomers in sight: refit once they settle if any landed outside the view
    if (!first && added.some((n) => { const sx = n.x * view.k + view.x, sy = n.y * view.k + view.y; return sx < 40 || sy < 40 || sx > width - 40 || sy > height - 40; })) fitWhenSettled = true;
    relayout(layout === "force" ? 0.8 : 1);
  }

  async function loadGraph(g) {
    const rows = await select(`SELECT ?s ?p ?o WHERE { GRAPH <${g}> { ?s ?p ?o } } LIMIT ${opts.limit * 10}`, { infer: false });
    for (const [s, p, o] of rows) {
      if (o.type === "literal" && !opts.literals) continue;
      if (p.value === RDF_TYPE) {
        const n = addNode(s);
        if (n && !n.types.includes(o.value)) n.types.push(o.value);
        continue;
      }
      const a = addNode(s), b = addNode(o, { near: a || undefined });
      if (a && b) addEdge(a, p, b);
    }
    empty.hidden = nodes.size > 0;
    if (!nodes.size) empty.textContent = "This graph has no statements between resources (try showing literals).";
    await describe([...nodes.values()]);
    fitWhenSettled = true;
    relayout(1);
  }

  async function loadSchema() {
    const [links, counts] = await Promise.all([
      select(`SELECT ?c1 ?p ?c2 (COUNT(*) AS ?n) WHERE { { SELECT ?s ?c1 WHERE { ?s a ?c1 } LIMIT 200000 } ?s ?p ?o . ?o a ?c2 FILTER(?p != <${RDF_TYPE}>) } GROUP BY ?c1 ?p ?c2 ORDER BY DESC(?n) LIMIT ${opts.limit * 4}`, { infer: false }),
      select("SELECT ?c (COUNT(?s) AS ?n) WHERE { ?s a ?c } GROUP BY ?c ORDER BY DESC(?n) LIMIT 300", { infer: false }),
    ]);
    const size = new Map(counts.map(([c, n]) => [c.value, Number(n.value)]));
    for (const [c, n] of counts.slice(0, opts.limit)) addNode(c, { cls: true, count: Number(n.value) });
    for (const [c1, p, c2, n] of links) {
      const a = addNode(c1, { cls: true, count: size.get(c1.value) || 1 });
      const b = addNode(c2, { cls: true, count: size.get(c2.value) || 1 });
      addEdge(a, p, b, { width: 1 + Math.log10(Number(n.value)), label: `${short(p)} · ${fmt(n.value)}` });
    }
    empty.hidden = nodes.size > 0;
    if (!nodes.size) empty.textContent = "No typed resources: the schema view needs rdf:type statements.";
    await describe([...nodes.values()]);
    fitWhenSettled = true;
    relayout(1);
  }

  // ------------------------------------------------------------ details panel
  const SUBCLASS = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
  const DESC_PREDICATES = new Set(["http://www.w3.org/2000/01/rdf-schema#comment", "http://www.w3.org/2004/02/skos/core#definition",
    "http://purl.org/dc/terms/description", "http://purl.org/dc/elements/1.1/description", "http://schema.org/description", "https://schema.org/description"]);
  const details = new Map(); // node key -> loaded details
  const pname = (iri) => abbreviate(iri).replace(/^<(.*)>$/, (_, i) => i.split(/[/#]/).filter(Boolean).pop() || i);
  const lang = () => (navigator.language || "en").slice(0, 2);

  async function loadDetails(n) {
    if (details.has(n.key)) return details.get(n.key);
    const node = sparqlTerm(n.term);
    const d = { props: new Map(), links: new Map(), incoming: [], graphs: [], types: [], supers: [], subs: [], samples: [], labels: new Map(), outCount: 0, inCount: 0, truncated: false };
    const q = (text, infer = opts.infer) => select(text, { infer }).catch(() => []);
    if (n.term.type === "literal") {
      const users = await q(`SELECT ?s ?p WHERE { ?s ?p ${node} } LIMIT 50`);
      for (const [s, p] of users) {
        if (!d.links.has(p.value)) d.links.set(p.value, []);
        d.links.get(p.value).push({ term: s, incoming: true });
      }
      d.inCount = users.length;
    } else {
      const LIMIT = 2000;
      const [out, explicit, inc, graphs, subs, samples] = await Promise.all([
        q(`SELECT ?p ?o WHERE { ${node} ?p ?o } LIMIT ${LIMIT}`),
        opts.infer ? q(`SELECT ?p ?o WHERE { ${node} ?p ?o } LIMIT ${LIMIT}`, false) : Promise.resolve(null),
        q(`SELECT ?p (COUNT(?s) AS ?c) WHERE { ?s ?p ${node} } GROUP BY ?p ORDER BY DESC(?c) LIMIT 60`),
        q(`SELECT DISTINCT ?g WHERE { GRAPH ?g { { ${node} ?p ?o } UNION { ?s ?p ${node} } } } LIMIT 30`, false),
        n.cls || n.types.includes("http://www.w3.org/2002/07/owl#Class") || n.types.includes("http://www.w3.org/2000/01/rdf-schema#Class")
          ? q(`SELECT DISTINCT ?c WHERE { ?c <${SUBCLASS}> ${node} FILTER(?c != ${node}) } LIMIT 40`) : Promise.resolve([]),
        n.cls ? q(`SELECT ?s WHERE { ?s a ${node} } LIMIT 12`) : Promise.resolve([]),
      ]);
      const stated = explicit && new Set(explicit.map(([p, o]) => p.value + "\u0000" + termKey(o)));
      d.outCount = out.length;
      d.truncated = out.length >= LIMIT;
      for (const [p, o] of out) {
        const inferred = !!stated && !stated.has(p.value + "\u0000" + termKey(o));
        if (p.value === RDF_TYPE && o.type === "uri") { d.types.push({ iri: o.value, inferred }); continue; }
        if (p.value === SUBCLASS && o.type === "uri" && o.value !== n.term.value) d.supers.push({ iri: o.value, inferred });
        const bucket = o.type === "literal" ? d.props : d.links;
        if (!bucket.has(p.value)) bucket.set(p.value, []);
        bucket.get(p.value).push({ term: o, inferred });
      }
      d.incoming = inc.map(([p, c]) => ({ p: p.value, count: Number(c.value) }));
      d.inCount = d.incoming.reduce((s, x) => s + x.count, 0);
      d.graphs = graphs.map(([g]) => g.value);
      d.subs = subs.map(([c]) => c.value);
      d.samples = samples.map(([s]) => s);
      // labels for everything the panel will name
      const iris = new Set();
      for (const list of d.links.values()) for (const x of list.slice(0, 8)) if (x.term.type === "uri") iris.add(x.term.value);
      for (const x of [...d.supers.map((s) => s.iri), ...d.subs, ...d.samples.map((s) => s.value)]) iris.add(x);
      const chunk = [...iris].slice(0, 300).map((i) => `<${i}>`).join(" ");
      if (chunk) {
        for (const [x, l] of await q(`SELECT ?n ?l WHERE { VALUES ?n { ${chunk} } VALUES ?lp { ${LABEL_PREDICATES.map((x) => `<${x}>`).join(" ")} } ?n ?lp ?l }`)) {
          if (!d.labels.has(x.value) || l["xml:lang"] === lang()) d.labels.set(x.value, l.value);
        }
      }
    }
    details.set(n.key, d);
    return d;
  }

  function nameOf(term, d) {
    const on = nodes.get(key(term));
    if (on) return labelOf(on);
    if (term.type === "uri" && d && d.labels.has(term.value)) return d.labels.get(term.value);
    return short(term);
  }

  function dotFor(term) {
    const on = nodes.get(key(term));
    return h("i", { class: "vg-dot", style: on ? `background:${colorFor(on)}` : "" });
  }

  // put a neighbour on the canvas (and keep the current selection)
  function addLinked(n, p, term, incoming) {
    const m = addNode(term, { near: n });
    if (!m) return toast(`Showing at most ${fmt(VG_MAX_NODES)} nodes`, true);
    if (incoming) addEdge(m, { value: p }, n); else addEdge(n, { value: p }, m);
    empty.hidden = true;
    describe([m]);
    relayout(0.5);
    return m;
  }

  async function addAll(n, p, incoming) {
    const node = sparqlTerm(n.term);
    const rows = await select(incoming ? `SELECT ?x WHERE { ?x <${p}> ${node} } LIMIT ${opts.limit}` : `SELECT ?x WHERE { ${node} <${p}> ?x FILTER(!isLiteral(?x)) } LIMIT ${opts.limit}`, { infer: opts.infer });
    const added = [];
    for (const [x] of rows) {
      const m = addNode(x, { near: n });
      if (!m) { toast(`Showing at most ${fmt(VG_MAX_NODES)} nodes`, true); break; }
      if (incoming) addEdge(m, { value: p }, n); else addEdge(n, { value: p }, m);
      added.push(m);
    }
    empty.hidden = nodes.size > 0;
    await describe(added);
    relayout(0.6);
    pick(n);
  }

  function pick(n) {
    selected = n;
    invalidate();
    if (!n) { side.replaceChildren(h("p", { class: "muted small" }, "Select a node to see its details.")); return; }
    renderDetails(n, details.get(n.key));
    if (!details.has(n.key)) loadDetails(n).then((d) => { if (selected === n) renderDetails(n, d); });
  }

  function renderDetails(n, d) {
    const kind = n.cls ? "Class" : n.term.type === "literal" ? "Literal" : n.term.type === "bnode" ? "Blank node" : "Resource";
    const copy = (text) => navigator.clipboard?.writeText(text).then(() => toast("Copied"), () => {});
    const section = (title, count, ...body) => h("details", { class: "vg-sec", open: true },
      h("summary", {}, h("span", {}, title), count != null ? h("b", {}, count) : null), ...body);
    const types = d ? d.types : n.types.map((iri) => ({ iri }));
    const head = [
      h("div", { class: "vg-head" },
        h("i", { class: "vg-swatch", style: `background:${colorFor(n)}` }),
        h("div", { class: "vg-head-text" }, h("div", { class: "eyebrow" }, kind), h("h2", { class: "vg-title" }, labelOf(n)))),
      n.term.type === "uri" ? h("div", { class: "vg-iri" }, h("code", {}, n.term.value), h("button", { class: "vg-icon", title: "Copy IRI", onclick: () => copy(n.term.value) }, "⧉")) : null,
      types.length ? h("div", { class: "types" }, types.map((t) => h("button", {
        class: "chip vg-type" + (t.inferred ? " inferred" : ""), title: t.iri + (t.inferred ? " (inferred)" : "") + "\nClick to highlight",
        style: `--chip:var(${PALETTE[colorIndex(t.iri)]})`, onclick: () => { highlightType = highlightType === t.iri ? null : t.iri; renderLegend(); invalidate(); },
      }, h("i", { class: "vg-dot", style: `background:var(${PALETTE[colorIndex(t.iri)]})` }), pname(t.iri)))) : null,
      h("div", { class: "row vg-actions" },
        !n.cls && n.term.type !== "literal" ? h("button", { class: "sm primary", onclick: () => expand(n.term) }, n.expanded ? "Load more" : "Expand") : null,
        n.term.type === "uri" ? h("button", { class: "sm", onclick: () => go("explore", { iri: n.term.value }) }, "Open") : null,
        n.term.type !== "bnode" ? h("button", { class: "sm", onclick: () => go("sparql", { query: n.cls ? `SELECT ?s WHERE { ?s a <${n.term.value}> } LIMIT 100` : n.term.type === "literal" ? `SELECT ?s ?p WHERE { ?s ?p ${sparqlTerm(n.term)} } LIMIT 100` : `DESCRIBE <${n.term.value}>`, run: 1 }) }, "SPARQL") : null,
        h("button", { class: "sm ghost", onclick: () => { n.fixed = !n.fixed; renderDetails(n, details.get(n.key)); relayout(0.3); } }, n.fixed ? "Unpin" : "Pin"),
        h("button", { class: "sm ghost", onclick: () => removeNode(n) }, "Hide")),
    ];
    if (!d) {
      side.replaceChildren(...head.filter(Boolean), h("div", { class: "vg-loading" }, h("span"), h("span"), h("span")));
      return;
    }
    const stats = h("dl", { class: "vg-stats-grid" },
      n.term.type !== "literal" ? [h("dt", {}, "Statements"), h("dd", {}, fmt(d.outCount) + (d.truncated ? "+" : ""))] : null,
      h("dt", {}, n.term.type === "literal" ? "Used by" : "Referenced by"), h("dd", {}, fmt(d.inCount)),
      n.cls ? [h("dt", {}, "Instances"), h("dd", {}, fmt(n.count))] : null,
      h("dt", {}, "On canvas"), h("dd", {}, `${fmt(n.links.size)} links`),
      d.graphs.length ? [h("dt", {}, "Graphs"), h("dd", {}, fmt(d.graphs.length))] : null);
    // a description, if the resource has one
    let desc = null;
    for (const [p, vals] of d.props) if (DESC_PREDICATES.has(p)) for (const v of vals) if (!desc || v.term["xml:lang"] === lang()) desc = v.term.value;
    const literalView = (v) => h("div", { class: "vg-val" + (v.inferred ? " inferred" : "") },
      h("span", { class: "vg-text" }, v.term.value),
      v.term["xml:lang"] ? h("span", { class: "vg-tag" }, v.term["xml:lang"].replace("--", " ")) : v.term.datatype && !v.term.datatype.endsWith("#string") ? h("span", { class: "vg-tag" }, pname(v.term.datatype)) : null,
      v.inferred ? h("span", { class: "vg-tag inferred", title: "Derived by inference" }, "inferred") : null);
    const props = [...d.props].filter(([p]) => !DESC_PREDICATES.has(p) || d.props.get(p).length > 1 || !desc).sort((a, b) => pname(a[0]).localeCompare(pname(b[0])));
    const linkRow = (p, x, incoming) => {
      const on = nodes.get(key(x.term));
      const linked = on && [...on.links].some((e) => e.p.value === p && (incoming ? e.o === n && e.s === on : e.s === n && e.o === on));
      return h("li", { class: x.inferred ? "inferred" : "" },
        dotFor(x.term),
        h("button", { class: "vg-link", title: x.term.value || "", onclick: () => { const m = on || addLinked(n, p, x.term, incoming); if (m) { pick(m); focusOn(m); } } }, nameOf(x.term, d)),
        x.inferred ? h("span", { class: "vg-tag inferred" }, "inferred") : null,
        linked ? h("span", { class: "vg-on", title: "On the canvas" }, "✓")
          : h("button", { class: "vg-icon", title: "Add to the canvas", onclick: () => { addLinked(n, p, x.term, incoming); renderDetails(n, d); } }, "+"));
    };
    const links = [...d.links].sort((a, b) => b[1].length - a[1].length);
    const outSection = links.length && n.term.type !== "literal" ? section("Links", fmt([...d.links.values()].reduce((s, l) => s + l.length, 0)),
      links.map(([p, list]) => h("div", { class: "vg-pred" },
        h("div", { class: "vg-pred-head" }, h("span", { title: p }, "→ " + pname(p)), h("b", {}, fmt(list.length)),
          list.length > 1 ? h("button", { class: "vg-mini", onclick: () => addAll(n, p, false) }, "add all") : null),
        h("ul", { class: "vg-list" }, list.slice(0, 6).map((x) => linkRow(p, x, false))),
        list.length > 6 ? h("button", { class: "vg-mini more", onclick: () => addAll(n, p, false) }, `+ ${fmt(list.length - 6)} more on the canvas`) : null))) : null;
    const usedBy = n.term.type === "literal" && links.length ? section("Used by", fmt(d.inCount),
      links.map(([p, list]) => h("div", { class: "vg-pred" },
        h("div", { class: "vg-pred-head" }, h("span", { title: p }, "← " + pname(p)), h("b", {}, fmt(list.length))),
        h("ul", { class: "vg-list" }, list.slice(0, 8).map((x) => linkRow(p, x, true)))))) : null;
    const inSection = d.incoming.length ? section("Referenced by", fmt(d.inCount),
      h("ul", { class: "vg-list" }, d.incoming.map(({ p, count }) => h("li", {},
        h("span", { class: "vg-link plain", title: p }, "← " + pname(p)), h("b", { class: "vg-count" }, fmt(count)),
        h("button", { class: "vg-mini", title: `Add up to ${opts.limit} of them`, onclick: () => addAll(n, p, true) }, "add"))))) : null;
    const chipsOf = (iris, title) => iris.length ? section(title, fmt(iris.length), h("div", { class: "types" }, iris.map((iri) => h("button", {
      class: "chip vg-type", title: iri, onclick: () => { const t = { type: "uri", value: iri }; const m = nodes.get(key(t)) || addNode(t, { near: n, cls: n.cls, count: 0 }); if (m) { describe([m]); relayout(0.5); pick(m); focusOn(m); } },
    }, nameOf({ type: "uri", value: iri }, d))))) : null;
    side.replaceChildren(...[
      ...head,
      stats,
      desc ? h("p", { class: "vg-desc" }, desc) : null,
      n.term.type === "literal" ? section("Value", null, literalView({ term: n.term }),
        h("dl", { class: "vg-stats-grid" }, h("dt", {}, "Datatype"), h("dd", {}, n.term["xml:lang"] ? "rdf:langString" : pname(n.term.datatype || "http://www.w3.org/2001/XMLSchema#string")),
          n.term["xml:lang"] ? [h("dt", {}, "Language"), h("dd", {}, n.term["xml:lang"])] : null,
          h("dt", {}, "Length"), h("dd", {}, fmt([...n.term.value].length)))) : null,
      props.length ? section("Properties", fmt(props.reduce((s, [, v]) => s + v.length, 0)),
        h("dl", { class: "vg-props" }, props.map(([p, vals]) => [h("dt", { title: p }, pname(p)), h("dd", {}, vals.slice(0, 12).map(literalView), vals.length > 12 ? h("span", { class: "muted small" }, `+ ${vals.length - 12} more`) : null)]))) : null,
      chipsOf(d.supers.map((s) => s.iri), "Superclasses"),
      chipsOf(d.subs, "Subclasses"),
      d.samples.length ? section("Instances", fmt(n.count || d.samples.length), h("ul", { class: "vg-list" }, d.samples.map((s) => h("li", {}, dotFor(s),
        h("button", { class: "vg-link", title: s.value, onclick: () => go("visual", { mode: "node", iri: s.value }) }, nameOf(s, d)))))) : null,
      outSection,
      usedBy,
      inSection,
      d.graphs.length ? section("Named graphs", fmt(d.graphs.length), h("div", { class: "types" }, d.graphs.map((g) => h("button", { class: "chip", title: g, onclick: () => go("visual", { mode: "graph", graph: g }) }, abbreviate(g).replace(/^<(.*)>$/, "$1"))))) : null,
    ].filter(Boolean));
  }

  function removeNode(n) {
    for (const e of [...n.links]) {
      edges.delete(e.key);
      e.s.links.delete(e); e.o.links.delete(e);
      e.s.degree--; e.o.degree--;
    }
    nodes.delete(n.key);
    if (selected === n) pick(null);
    if (hovered === n) hovered = null;
    empty.hidden = nodes.size > 0;
    renderLegend();
    relayout(0.3);
  }

  function isolate(n) {
    const keep = neighbours(n);
    for (const m of [...nodes.values()]) if (!keep.has(m)) removeNode(m);
    fitWhenSettled = true;
  }

  function applyLiteralFilter() {
    if (opts.literals) return;
    for (const n of [...nodes.values()]) if (n.term.type === "literal") removeNode(n);
  }

  function renderLegend() {
    const used = new Map();
    for (const n of nodes.values()) if (!n.cls && n.primary) used.set(n.primary, (used.get(n.primary) || 0) + 1);
    const ranked = [...used.entries()].sort((a, b) => b[1] - a[1]);
    const local = (t) => short({ type: "uri", value: t });
    legend.replaceChildren(...ranked.slice(0, 6).map(([t, c]) =>
      h("button", { class: "vg-key" + (highlightType === t ? " on" : ""), title: `${t}\nClick to highlight`, onclick: () => { find.value = ""; query = ""; highlightType = highlightType === t ? null : t; renderLegend(); invalidate(); } },
        h("i", { style: `background:var(${PALETTE[colorIndex(t)]})` }), local(t), h("b", {}, fmt(c)))),
      ...(ranked.length > 6 ? [h("span", { class: "vg-more" }, `+${ranked.length - 6} more`)] : []));
    legend.hidden = used.size === 0;
  }
  let highlightType = null;

  // ------------------------------------------------------------ layouts
  // Force: Barnes–Hut repulsion (O(n log n)), springs along edges, weak gravity and
  // collision. Radial and hierarchy place nodes by breadth-first depth from a root and
  // animate them there.
  function relayout(a = 1) {
    if (layout === "force") { moves = null; kick(a); return; }
    placeStructured();
  }

  function kick(a) {
    alpha = Math.max(alpha, a);
    invalidate();
  }

  function quadtree(list) {
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const n of list) { x0 = Math.min(x0, n.x); y0 = Math.min(y0, n.y); x1 = Math.max(x1, n.x); y1 = Math.max(y1, n.y); }
    const size = Math.max(x1 - x0, y1 - y0, 1) + 1;
    const root = { x: x0, y: y0, size, mass: 0, cx: 0, cy: 0, body: null, kids: null };
    const insert = (q, n, depth) => {
      q.cx = (q.cx * q.mass + n.x) / (q.mass + 1);
      q.cy = (q.cy * q.mass + n.y) / (q.mass + 1);
      q.mass++;
      if (!q.kids && !q.body) { q.body = n; return; }
      if (depth > 24) return; // coincident points: keep the mass, drop the split
      if (!q.kids) {
        const half = q.size / 2;
        q.kids = [0, 1, 2, 3].map((i) => ({ x: q.x + (i & 1) * half, y: q.y + (i >> 1) * half, size: half, mass: 0, cx: 0, cy: 0, body: null, kids: null }));
        const old = q.body;
        q.body = null;
        insertKid(q, old, depth);
      }
      insertKid(q, n, depth);
    };
    const insertKid = (q, n, depth) => {
      const half = q.size / 2;
      const i = (n.x >= q.x + half ? 1 : 0) + (n.y >= q.y + half ? 2 : 0);
      insert(q.kids[i], n, depth + 1);
    };
    for (const n of list) insert(root, n, 0);
    return root;
  }

  function simulate() {
    const list = [...nodes.values()];
    if (!list.length) { alpha = 0; return; }
    const tree = quadtree(list);
    const strength = 2600 * alpha;
    const theta2 = 0.81;
    for (const a of list) {
      const ra = radius(a);
      const stack = [tree];
      while (stack.length) {
        const q = stack.pop();
        if (!q.mass) continue;
        let dx = a.x - q.cx, dy = a.y - q.cy;
        let d2 = dx * dx + dy * dy;
        if (q.body === a && !q.kids) continue;
        if (q.kids && (q.size * q.size) / Math.max(d2, 1) > theta2) { for (const k of q.kids) stack.push(k); continue; }
        if (d2 < 1) { dx = Math.random() - 0.5; dy = Math.random() - 0.5; d2 = 1; }
        if (d2 > 640000) continue;
        const f = (strength * q.mass * (a.cls ? 3.2 : 1)) / d2;
        a.vx += dx * f / Math.sqrt(d2) * 0.08; a.vy += dy * f / Math.sqrt(d2) * 0.08;
        // collision with a single nearby body
        if (q.body && q.body !== a) {
          const d = Math.sqrt(d2), min = ra + radius(q.body) + 6;
          if (d < min) { const push = (min - d) / d * 0.25; a.vx += dx * push; a.vy += dy * push; }
        }
      }
    }
    for (const e of edges.values()) {
      if (e.s === e.o) continue;
      const dx = e.o.x - e.s.x, dy = e.o.y - e.s.y;
      const d = Math.sqrt(dx * dx + dy * dy) || 1;
      const want = (e.s.cls || e.o.cls ? 230 : 90) + (radius(e.s) + radius(e.o));
      const f = (d - want) / d * 0.06 * alpha / Math.sqrt(Math.min(e.s.degree, e.o.degree) || 1);
      e.s.vx += dx * f; e.s.vy += dy * f; e.o.vx -= dx * f; e.o.vy -= dy * f;
    }
    for (const p of list) {
      p.vx -= p.x * 0.003 * alpha; p.vy -= p.y * 0.003 * alpha;
      if (p.fixed || p === dragging) { p.vx = 0; p.vy = 0; continue; }
      p.vx *= 0.62; p.vy *= 0.62;
      p.x += Math.max(-40, Math.min(40, p.vx)); p.y += Math.max(-40, Math.min(40, p.vy));
    }
    alpha += (0 - alpha) * 0.022;
    if (alpha < 0.012) {
      alpha = 0;
      if (fitWhenSettled) { fitWhenSettled = false; fit(true); }
    }
  }

  function bfs() {
    const list = [...nodes.values()];
    const root = selected || list.find((n) => n.fixed) || list.slice().sort((a, b) => b.degree - a.degree)[0];
    const depth = new Map(), parent = new Map(), order = [];
    const visit = (start) => {
      depth.set(start, depth.has(start) ? depth.get(start) : 0);
      const queue = [start];
      while (queue.length) {
        const n = queue.shift();
        order.push(n);
        for (const e of n.links) {
          const m = e.s === n ? e.o : e.s;
          if (!depth.has(m)) { depth.set(m, depth.get(n) + 1); parent.set(m, n); queue.push(m); }
        }
      }
    };
    if (root) visit(root);
    // other components hang below the deepest level
    for (const n of list) if (!depth.has(n)) { depth.set(n, Math.max(0, ...depth.values()) + 1); visit(n); }
    return { root, depth, parent, order };
  }

  function placeStructured() {
    const { root, depth, parent, order } = bfs();
    if (!root) return;
    const kids = new Map();
    for (const n of order) if (parent.has(n)) { const p = parent.get(n); if (!kids.has(p)) kids.set(p, []); kids.get(p).push(n); }
    const leaves = new Map();
    const count = (n) => { const c = (kids.get(n) || []).reduce((s, k) => s + count(k), 0) || 1; leaves.set(n, c); return c; };
    const roots = order.filter((n) => !parent.has(n));
    roots.forEach(count);
    const target = new Map();
    if (layout === "radial") {
      const total = roots.reduce((s, r) => s + leaves.get(r), 0);
      let a0 = 0;
      const place = (n, from, to) => {
        const d = depth.get(n), mid = (from + to) / 2;
        target.set(n, d === 0 && n === root ? { x: 0, y: 0 } : { x: Math.cos(mid) * d * 150, y: Math.sin(mid) * d * 150 });
        let a = from;
        for (const k of kids.get(n) || []) { const span = (to - from) * leaves.get(k) / leaves.get(n); place(k, a, a + span); a += span; }
      };
      for (const r of roots) { const span = Math.PI * 2 * leaves.get(r) / total; place(r, a0, a0 + span); a0 += span; }
    } else {
      // hierarchy: one band per depth, ordered by the parents' positions (fewer crossings);
      // a wide band wraps onto several rows so the picture stays compact
      const layers = [];
      for (const n of order) (layers[depth.get(n)] ||= []).push(n);
      const perRow = Math.max(8, Math.ceil(Math.sqrt(order.length) * 1.6));
      let y = 0;
      for (const layer of layers.filter(Boolean)) {
        const px = (n) => (parent.has(n) && target.has(parent.get(n)) ? target.get(parent.get(n)).x : 0);
        layer.sort((a, b) => px(a) - px(b));
        for (let i = 0; i < layer.length; i += perRow) {
          const row = layer.slice(i, i + perRow);
          row.forEach((n, j) => target.set(n, { x: (j - (row.length - 1) / 2) * 96, y }));
          y += 70;
        }
        y += 60;
      }
    }
    const t0 = now();
    moves = { t0, ms: 650, from: new Map([...nodes.values()].map((n) => [n, { x: n.x, y: n.y }])), to: target };
    alpha = 0;
    fitWhenSettled = true;
    invalidate();
  }

  function stepMoves() {
    if (!moves) return false;
    const t = Math.min(1, (now() - moves.t0) / moves.ms), e = ease(t);
    for (const [n, to] of moves.to) {
      if (n.fixed && n !== selected) continue;
      const from = moves.from.get(n) || to;
      n.x = from.x + (to.x - from.x) * e; n.y = from.y + (to.y - from.y) * e;
    }
    if (t >= 1) { moves = null; if (fitWhenSettled) { fitWhenSettled = false; fit(true); } }
    return true;
  }

  function setLayout(name) {
    layout = name;
    relayout(1);
  }

  // ------------------------------------------------------------ drawing
  function invalidate() {
    if (!frame) frame = requestAnimationFrame(loop);
  }

  function loop() {
    frame = null;
    let busy = false;
    if (alpha > 0) { simulate(); busy = true; }
    if (stepMoves()) busy = true;
    if (stepTween()) busy = true;
    if (stepInertia()) busy = true;
    const t = now();
    for (const n of nodes.values()) if (t - n.born < 400) { busy = true; break; }
    draw();
    if (busy) invalidate();
  }

  function resize() {
    rect = null;
    const r = canvas.getBoundingClientRect();
    dpr = Math.min(2, window.devicePixelRatio || 1);
    width = r.width; height = r.height;
    cv.width = Math.max(1, Math.round(width * dpr)); cv.height = Math.max(1, Math.round(height * dpr));
    cv.style.width = width + "px"; cv.style.height = height + "px";
    mini.width = Math.round(176 * dpr); mini.height = Math.round(116 * dpr);
    invalidate();
  }
  new ResizeObserver(() => { const blank = !width; resize(); if (blank || !nodes.size) fit(); }).observe(canvas);

  // the focus set: hovered or selected node and its neighbours, a found label, or a type
  function focusSet() {
    if (hovered) return neighbours(hovered);
    if (query) {
      const q = query.toLowerCase();
      return new Set([...nodes.values()].filter((n) => labelOf(n).toLowerCase().includes(q) || (n.term.value || "").toLowerCase().includes(q)));
    }
    if (highlightType) return new Set([...nodes.values()].filter((n) => n.primary === highlightType));
    if (selected) return neighbours(selected);
    return null;
  }

  function curve(e) {
    // control point of the edge's quadratic curve: straight for a single edge, fanned for
    // several between the same pair (consistently oriented, so a↔b pairs don't overlap)
    const { s, o } = e;
    const dx = o.x - s.x, dy = o.y - s.y, d = Math.sqrt(dx * dx + dy * dy) || 1;
    const flip = s.key < o.key ? 1 : -1;
    const lanes = e.lanes || 1;
    const off = lanes > 1 ? (e.lane - (lanes - 1) / 2) * 34 * flip : 0;
    return { cx: (s.x + o.x) / 2 - dy / d * off, cy: (s.y + o.y) / 2 + dx / d * off };
  }

  function draw() {
    const c = palette();
    const t = now();
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = c.bg;
    ctx.fillRect(0, 0, width, height);
    // dot grid that moves with the view: one fill with a cached pattern tile, whatever the
    // canvas size or zoom (drawing the dots one by one cost thousands of calls per frame)
    const step = 26 * view.k;
    if (step > 9) {
      const tile = gridTile(step, c.line);
      const ox = ((view.x % step) + step) % step, oy = ((view.y % step) + step) % step;
      tile.pattern.setTransform(new DOMMatrix([1 / tile.scale, 0, 0, 1 / tile.scale, ox - step / 2, oy - step / 2]));
      ctx.fillStyle = tile.pattern;
      ctx.fillRect(0, 0, width, height);
    }
    const focus = focusSet();
    const inFocus = (n) => !focus || focus.has(n);
    ctx.setTransform(dpr * view.k, 0, 0, dpr * view.k, dpr * view.x, dpr * view.y);

    // edges: dimmed ones first, then the focused ones on top
    const lw = 1 / Math.max(0.5, Math.min(view.k, 2));
    const drawEdge = (e, hot) => {
      const { s, o } = e;
      const fade = Math.min(1, (t - e.born) / 300);
      ctx.globalAlpha = (hot ? 0.95 : focus ? 0.08 : 0.55) * fade;
      ctx.strokeStyle = hot ? c.accent : c.strong;
      ctx.fillStyle = ctx.strokeStyle;
      ctx.lineWidth = Math.min(6, e.width) * lw * (hot ? 1.6 : 1);
      ctx.setLineDash(e.inferred ? [4 * lw, 3 * lw] : []);
      if (s === o) {
        // a self-link: a loop leaving the rim up and to the right and coming back into it,
        // with its arrowhead on the rim like any other link
        const r = radius(s), [a1, a2] = [-Math.PI / 2 + 0.45, -Math.PI / 2 - 0.45];
        const p1 = { x: s.x + Math.cos(a1) * r, y: s.y + Math.sin(a1) * r }, p2 = { x: s.x + Math.cos(a2) * (r + 2), y: s.y + Math.sin(a2) * (r + 2) };
        const reach = r + 30 + r * 0.6;
        const c1 = { x: s.x + Math.cos(a1 + 0.35) * reach, y: s.y + Math.sin(a1 + 0.35) * reach }, c2 = { x: s.x + Math.cos(a2 - 0.35) * reach, y: s.y + Math.sin(a2 - 0.35) * reach };
        ctx.beginPath(); ctx.moveTo(p1.x, p1.y); ctx.bezierCurveTo(c1.x, c1.y, c2.x, c2.y, p2.x, p2.y); ctx.stroke();
        const ux0 = p2.x - c2.x, uy0 = p2.y - c2.y, ul = Math.hypot(ux0, uy0) || 1, ux = ux0 / ul, uy = uy0 / ul;
        const ah = 7 * lw * (hot ? 1.3 : 1);
        ctx.setLineDash([]);
        ctx.beginPath(); ctx.moveTo(p2.x, p2.y);
        ctx.lineTo(p2.x - ux * ah - uy * ah * 0.55, p2.y - uy * ah + ux * ah * 0.55);
        ctx.lineTo(p2.x - ux * ah + uy * ah * 0.55, p2.y - uy * ah - ux * ah * 0.55);
        ctx.closePath(); ctx.fill();
        return;
      }
      const { cx, cy } = curve(e);
      // stop at the target's rim, along the curve's final tangent
      const tx = o.x - cx, ty = o.y - cy, tl = Math.sqrt(tx * tx + ty * ty) || 1;
      const ro = radius(o) + 2;
      const ex = o.x - tx / tl * ro, ey = o.y - ty / tl * ro;
      ctx.beginPath(); ctx.moveTo(s.x, s.y); ctx.quadraticCurveTo(cx, cy, ex, ey); ctx.stroke();
      // a hub's arrowheads would merge into a solid ring on its rim: draw them only when the
      // rim has room for each (they return as you zoom in)
      if (crowded(o)) return;
      const ah = 7 * lw * (hot ? 1.3 : 1), ux = tx / tl, uy = ty / tl;
      ctx.setLineDash([]);
      ctx.beginPath();
      ctx.moveTo(ex, ey);
      ctx.lineTo(ex - ux * ah - uy * ah * 0.55, ey - uy * ah + ux * ah * 0.55);
      ctx.lineTo(ex - ux * ah + uy * ah * 0.55, ey - uy * ah - ux * ah * 0.55);
      ctx.closePath(); ctx.fill();
    };
    const incoming = new Map();
    for (const e of edges.values()) incoming.set(e.o, (incoming.get(e.o) || 0) + 1);
    // screen pixels of rim per incoming arrow; below ~9 px the heads overlap
    const crowded = (o) => { const k = incoming.get(o) || 0; return k > 8 && (2 * Math.PI * (radius(o) + 2) * view.k) / k < 9; };
    const hotEdges = [];
    for (const e of edges.values()) {
      const hot = focus && focus.has(e.s) && focus.has(e.o) && (hovered || selected ? (e.s === (hovered || selected) || e.o === (hovered || selected)) : true);
      if (hot) hotEdges.push(e); else drawEdge(e, false);
    }
    for (const e of hotEdges) drawEdge(e, true);
    ctx.setLineDash([]);

    // nodes
    for (const n of nodes.values()) {
      const grow = easeBack(Math.min(1, (t - n.born) / 380));
      const r = radius(n) * grow;
      const on = inFocus(n);
      ctx.globalAlpha = on ? 1 : 0.18;
      const col = colorFor(n);
      if (n === selected || n === hovered) {
        // glow as two soft rings (a blurred shadow is costly to rasterize on large canvases)
        ctx.fillStyle = col;
        ctx.globalAlpha = 0.12; ctx.beginPath(); ctx.arc(n.x, n.y, r + 11, 0, Math.PI * 2); ctx.fill();
        ctx.globalAlpha = 0.22; ctx.beginPath(); ctx.arc(n.x, n.y, r + 6, 0, Math.PI * 2); ctx.fill();
        ctx.globalAlpha = 1;
      }
      ctx.fillStyle = col;
      ctx.strokeStyle = c.surface;
      ctx.lineWidth = 2 * lw;
      ctx.beginPath();
      if (n.term.type === "literal") ctx.roundRect(n.x - r * 1.4, n.y - r, r * 2.8, r * 2, r * 0.6);
      else ctx.arc(n.x, n.y, r, 0, Math.PI * 2);
      ctx.fill(); ctx.stroke();
      if (n.expanded && !n.cls) {
        ctx.strokeStyle = col; ctx.globalAlpha *= 0.45; ctx.lineWidth = 1.4 * lw;
        ctx.beginPath(); ctx.arc(n.x, n.y, r + 3.5, 0, Math.PI * 2); ctx.stroke();
        ctx.globalAlpha = on ? 1 : 0.18;
      }
      if (n === selected) {
        ctx.strokeStyle = c.accent; ctx.lineWidth = 2.5 * lw;
        ctx.beginPath(); ctx.arc(n.x, n.y, r + 4, 0, Math.PI * 2); ctx.stroke();
      }
      if (n.fixed) {
        // pinned: a small badge on the rim (the centre may carry a count)
        const br = Math.max(2.6, Math.min(5, r * 0.3)), bx = n.x + r * 0.72, by = n.y - r * 0.72;
        ctx.fillStyle = c.fg; ctx.strokeStyle = c.bg; ctx.lineWidth = 1.5 * lw;
        ctx.beginPath(); ctx.arc(bx, by, br, 0, Math.PI * 2); ctx.fill(); ctx.stroke();
      }
      if (n.cls && r * view.k > 16) {
        ctx.fillStyle = c.surface; ctx.font = `600 ${Math.max(8, r * 0.55)}px ${c.sans}`;
        ctx.textAlign = "center"; ctx.textBaseline = "middle";
        ctx.fillText(compact(n.count), n.x, n.y + 0.5);
      }
    }
    ctx.globalAlpha = 1;

    // labels in screen space, so they stay crisp and readable at every zoom; placed by
    // priority and skipped where they would collide
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.textAlign = "left"; ctx.textBaseline = "middle";
    // label boxes and node discs go into a coarse grid, so placing a label checks only its
    // neighbourhood instead of every placed box
    const CELL = 64, cells = new Map();
    const each = (b, f) => {
      for (let gx = Math.floor(b[0] / CELL); gx <= Math.floor(b[2] / CELL); gx++)
        for (let gy = Math.floor(b[1] / CELL); gy <= Math.floor(b[3] / CELL); gy++) if (f(gx * 65536 + gy)) return true;
      return false;
    };
    const free = (x0, y0, x1, y1) => !each([x0, y0, x1, y1], (k) => (cells.get(k) || []).some((b) => x0 < b[2] && x1 > b[0] && y0 < b[3] && y1 > b[1]));
    const occupy = (b) => each(b, (k) => { let l = cells.get(k); if (!l) cells.set(k, (l = [])); l.push(b); return false; });
    const placed = { push: occupy };
    // the nodes themselves are obstacles: a label must not sit on another node
    let mx0 = 0, my0 = 0;
    for (const n of nodes.values()) {
      const sx = n.x * view.k + view.x, sy = n.y * view.k + view.y, rr = radius(n) * view.k * 0.92;
      mx0 += sx; my0 += sy;
      if (sx > -60 && sy > -60 && sx < width + 60 && sy < height + 60) occupy([sx - rr, sy - rr, sx + rr, sy + rr]);
    }
    const midX = nodes.size ? mx0 / nodes.size : width / 2;
    const prio = (n) => (n === selected || n === hovered ? 1e9 : 0) + (focus && focus.has(n) ? 1e6 : 0) + (n.cls ? n.count || 0 : n.degree * 10);
    const ordered = [...nodes.values()].sort((a, b) => prio(b) - prio(a));
    const always = nodes.size <= 40 || view.k > 1.4;
    // while the view moves (drag, pan, zoom, layout), draw only the labels that matter: text
    // is the most expensive thing to rasterize
    const moving = !!press?.moved || !!tween || !!inertia || !!moves || t - lastWheel < 160;
    let budget = moving ? 40 : Infinity;
    for (const n of ordered) {
      const sx = n.x * view.k + view.x, sy = n.y * view.k + view.y;
      if (sx < -200 || sy < -20 || sx > width + 20 || sy > height + 20) continue;
      const important = n === selected || n === hovered || (focus && focus.has(n));
      if (!always && !important && radius(n) * view.k < 7) continue;
      if (!important && budget-- <= 0) continue;
      const text = labelOf(n);
      const bold = n === selected || n === hovered;
      ctx.font = `${bold ? 600 : 500} ${n.cls ? 13 : 12}px ${n.term.type === "literal" ? c.mono : c.sans}`;
      const w = Math.min(ctx.measureText(text).width, 260);
      // clear of the node, its selection ring and its glow; try right, left, below, above
      // (nodes left of the picture's centre try left first), and skip only if all collide
      const rr = (radius(n) + (n === selected || n === hovered ? 6 : 1)) * view.k + 6;
      const side = [[sx + rr, sy], [sx - rr - w, sy]];
      if (sx < midX) side.reverse();
      const spots = [...side, [sx - w / 2, sy + rr + 4], [sx - w / 2, sy - rr - 4]];
      let x = null, y = null;
      for (const [cx, cy] of spots) {
        if (free(cx - 3, cy - 9, cx + w + 3, cy + 9)) { x = cx; y = cy; break; }
      }
      if (x === null) continue;
      placed.push([x - 3, y - 9, x + w + 3, y + 9]);
      ctx.globalAlpha = focus && !focus.has(n) ? 0.25 : 1;
      if (!moving || important) {
        ctx.lineWidth = 4; ctx.strokeStyle = c.bg; ctx.lineJoin = "round";
        ctx.strokeText(text, x, y, 260);
      }
      ctx.fillStyle = n.term.type === "literal" ? c.lit : c.fg;
      ctx.fillText(text, x, y, 260);
    }
    // edge labels: focused edges always, the rest when zoomed in
    if (edgeLabels) {
      ctx.font = `500 10.5px ${c.mono}`;
      ctx.textAlign = "center";
      const hotSet = new Set(hotEdges);
      const seenLabel = new Set();
      for (const e of edges.values()) {
        const hot = hotSet.has(e);
        // around a hub, each distinct predicate is labelled once instead of on every edge
        if (hot && hotEdges.length > 24) { if (seenLabel.has(e.label)) continue; seenLabel.add(e.label); }
        // unfocused edges get labels only when zoomed in (and the view is not crowded)
        if (!hot && (focus || view.k < (edges.size > 120 ? 2.2 : 1.6))) continue;
        let mx, my;
        if (e.s === e.o) { const r = radius(e.s); mx = e.s.x; my = e.s.y - r - 22 - r * 0.45; }
        else { const { cx, cy } = curve(e); mx = 0.25 * e.s.x + 0.5 * cx + 0.25 * e.o.x; my = 0.25 * e.s.y + 0.5 * cy + 0.25 * e.o.y; }
        const sx = mx * view.k + view.x, sy = my * view.k + view.y;
        if (sx < 0 || sy < 0 || sx > width || sy > height) continue;
        const w = ctx.measureText(e.label).width;
        if (!free(sx - w / 2 - 4, sy - 8, sx + w / 2 + 4, sy + 8)) continue;
        placed.push([sx - w / 2 - 4, sy - 8, sx + w / 2 + 4, sy + 8]);
        ctx.globalAlpha = 1;
        ctx.fillStyle = c.surface;
        ctx.beginPath(); ctx.roundRect(sx - w / 2 - 5, sy - 8, w + 10, 16, 8); ctx.fill();
        ctx.strokeStyle = hot ? c.accent : c.line; ctx.lineWidth = 1; ctx.stroke();
        ctx.fillStyle = hot ? c.accent : c.muted;
        ctx.fillText(e.label, sx, sy + 0.5);
      }
    }
    ctx.globalAlpha = 1;
    drawMinimap();
    // touch the DOM only on change: a write per frame forces a layout on the next pointer
    // event (which reads the canvas position) and makes dragging stutter
    const text = nodes.size ? `${fmt(nodes.size)} nodes · ${fmt(edges.size)} links · ${Math.round(view.k * 100)}%` : "";
    if (text !== lastStats) { lastStats = text; stats.textContent = text; }
  }

  let grid = null;
  function gridTile(step, color) {
    // a tile is rendered at device resolution, rounded so zooming does not rebuild it constantly
    const px = Math.max(4, Math.round(step * dpr));
    if (grid && grid.px === px && grid.color === color) return grid;
    const tc = document.createElement("canvas");
    tc.width = tc.height = px;
    const g = tc.getContext("2d");
    const dot = Math.min(1.6, Math.max(0.8, view.k)) * dpr;
    g.fillStyle = color;
    g.fillRect(px / 2 - dot / 2, px / 2 - dot / 2, dot, dot);
    grid = { px, color, scale: px / step, pattern: ctx.createPattern(tc, "repeat") };
    return grid;
  }

  function compact(n) {
    n = Number(n) || 0;
    return n >= 1e6 ? (n / 1e6).toFixed(n >= 1e7 ? 0 : 1) + "M" : n >= 1e3 ? (n / 1e3).toFixed(n >= 1e4 ? 0 : 1) + "k" : String(n);
  }

  function bounds() {
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const n of nodes.values()) { const r = radius(n); x0 = Math.min(x0, n.x - r); y0 = Math.min(y0, n.y - r); x1 = Math.max(x1, n.x + r); y1 = Math.max(y1, n.y + r); }
    return { x0, y0, x1, y1 };
  }

  let miniMap = null;
  let lastStats = null;
  let lastWheel = 0, wheelEnd = null;
  function drawMinimap() {
    if (mini.hidden !== nodes.size < 2) mini.hidden = nodes.size < 2;
    if (mini.hidden) return;
    const c = palette();
    const W = 176, H = 116, pad = 8;
    const b = bounds();
    // include the viewport, so the frame is always on the map
    const v = { x0: -view.x / view.k, y0: -view.y / view.k, x1: (width - view.x) / view.k, y1: (height - view.y) / view.k };
    const x0 = Math.min(b.x0, v.x0), y0 = Math.min(b.y0, v.y0), x1 = Math.max(b.x1, v.x1), y1 = Math.max(b.y1, v.y1);
    const s = Math.min((W - 2 * pad) / Math.max(1, x1 - x0), (H - 2 * pad) / Math.max(1, y1 - y0));
    const ox = pad + ((W - 2 * pad) - (x1 - x0) * s) / 2 - x0 * s, oy = pad + ((H - 2 * pad) - (y1 - y0) * s) / 2 - y0 * s;
    miniMap = { s, ox, oy };
    mctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    mctx.clearRect(0, 0, W, H);
    mctx.globalAlpha = 0.35; mctx.strokeStyle = c.strong; mctx.lineWidth = 0.6;
    mctx.beginPath();
    for (const e of edges.values()) { mctx.moveTo(e.s.x * s + ox, e.s.y * s + oy); mctx.lineTo(e.o.x * s + ox, e.o.y * s + oy); }
    mctx.stroke();
    mctx.globalAlpha = 1;
    for (const n of nodes.values()) {
      mctx.fillStyle = colorFor(n);
      mctx.fillRect(n.x * s + ox - 1.2, n.y * s + oy - 1.2, 2.4, 2.4);
    }
    mctx.strokeStyle = c.accent; mctx.lineWidth = 1.5;
    mctx.fillStyle = c.accent; mctx.globalAlpha = 0.08;
    mctx.fillRect(v.x0 * s + ox, v.y0 * s + oy, (v.x1 - v.x0) * s, (v.y1 - v.y0) * s);
    mctx.globalAlpha = 1;
    mctx.strokeRect(v.x0 * s + ox, v.y0 * s + oy, (v.x1 - v.x0) * s, (v.y1 - v.y0) * s);
  }

  // ------------------------------------------------------------ camera
  function animateView(to, ms = 450) {
    tween = { from: { ...view }, to, t0: now(), ms };
    invalidate();
  }
  function stepTween() {
    if (!tween) return false;
    const t = Math.min(1, (now() - tween.t0) / tween.ms), e = ease(t);
    view = { x: tween.from.x + (tween.to.x - tween.from.x) * e, y: tween.from.y + (tween.to.y - tween.from.y) * e, k: tween.from.k + (tween.to.k - tween.from.k) * e };
    if (t >= 1) tween = null;
    return true;
  }
  function zoomAt(k, mx, my) {
    k = Math.min(5, Math.max(0.08, k));
    view = { k, x: mx - (mx - view.x) * (k / view.k), y: my - (my - view.y) * (k / view.k) };
    invalidate();
  }
  function zoomBy(f) {
    const k = Math.min(5, Math.max(0.08, view.k * f));
    animateView({ k, x: width / 2 - (width / 2 - view.x) * (k / view.k), y: height / 2 - (height / 2 - view.y) * (k / view.k) }, 260);
  }
  function fit(animate = false) {
    if (!width || !height) { resize(); if (!width) return; }
    if (!nodes.size) { view = { x: width / 2, y: height / 2, k: 1 }; invalidate(); return; }
    const b = bounds();
    const k = Math.max(0.08, Math.min(1.6, 0.86 * Math.min(width / Math.max(120, b.x1 - b.x0), height / Math.max(120, b.y1 - b.y0))));
    const to = { k, x: width / 2 - k * (b.x0 + b.x1) / 2, y: height / 2 - k * (b.y0 + b.y1) / 2 };
    if (animate) animateView(to); else { view = to; invalidate(); }
  }
  function focusOn(n) {
    const k = Math.max(view.k, 1.2);
    animateView({ k, x: width / 2 - n.x * k, y: height / 2 - n.y * k });
  }
  let inertia = null;
  function stepInertia() {
    if (!inertia) return false;
    view.x += inertia.vx; view.y += inertia.vy;
    inertia.vx *= 0.9; inertia.vy *= 0.9;
    if (Math.abs(inertia.vx) + Math.abs(inertia.vy) < 0.2) inertia = null;
    return true;
  }
  function toggleFullscreen() {
    if (document.fullscreenElement) document.exitFullscreen();
    else canvas.requestFullscreen?.().catch(() => toast("Full screen is not available here", true));
  }

  // ------------------------------------------------------------ interaction
  // the canvas position, read once per gesture rather than on every pointer event
  let rect = null;
  const canvasRect = (fresh) => (rect = fresh || !rect ? cv.getBoundingClientRect() : rect);
  addEventListener("scroll", () => { rect = null; }, { passive: true, capture: true });
  addEventListener("resize", () => { rect = null; });
  const toWorld = (e) => { const r = canvasRect(); return { x: (e.clientX - r.left - view.x) / view.k, y: (e.clientY - r.top - view.y) / view.k, sx: e.clientX - r.left, sy: e.clientY - r.top }; };
  function hit(p) {
    let best = null, bestD = Infinity;
    for (const n of nodes.values()) {
      const r = radius(n) + 4 / view.k;
      const dx = n.x - p.x, dy = n.y - p.y, d = dx * dx + dy * dy;
      if (d <= r * r * (n.term.type === "literal" ? 2.2 : 1) && d < bestD) { best = n; bestD = d; }
    }
    return best;
  }
  let press = null;
  cv.addEventListener("pointerdown", (e) => {
    canvasRect(true);
    if (e.button === 2) return;
    hideMenu();
    cv.focus({ preventScroll: true });
    const p = toWorld(e);
    const n = hit(p);
    cv.setPointerCapture(e.pointerId);
    tween = null; inertia = null;
    press = { n, x0: e.clientX, y0: e.clientY, vx0: view.x, vy0: view.y, moved: false, last: { x: e.clientX, y: e.clientY, t: now() }, v: { x: 0, y: 0 } };
  });
  cv.addEventListener("pointermove", (e) => {
    const p = toWorld(e);
    if (!press) {
      const n = hit(p);
      if (n !== hovered) { hovered = n; cv.style.cursor = n ? "pointer" : "grab"; invalidate(); }
      showTip(n, p);
      return;
    }
    const dx = e.clientX - press.x0, dy = e.clientY - press.y0;
    if (!press.moved && dx * dx + dy * dy < 16) return;
    press.moved = true;
    hideTip();
    if (press.n) {
      dragging = press.n;
      press.n.x = p.x; press.n.y = p.y;
      if (layout === "force") kick(0.25);
      invalidate();
    } else {
      cv.style.cursor = "grabbing";
      view.x = press.vx0 + dx; view.y = press.vy0 + dy;
      const t = now(), dt = Math.max(1, t - press.last.t);
      press.v = { x: (e.clientX - press.last.x) / dt * 16, y: (e.clientY - press.last.y) / dt * 16 };
      press.last = { x: e.clientX, y: e.clientY, t };
      invalidate();
    }
  });
  cv.addEventListener("pointerup", () => {
    if (!press) return;
    if (!press.moved) pick(press.n);
    else if (press.n) { press.n.fixed = true; if (selected === press.n) pick(press.n); }
    else if (Math.abs(press.v.x) + Math.abs(press.v.y) > 2 && now() - press.last.t < 80) {
      // glide on, but never fling the graph away: at most ~300 px in all
      const sp = Math.hypot(press.v.x, press.v.y), cap = 30 / Math.max(sp, 30);
      inertia = { vx: press.v.x * cap, vy: press.v.y * cap };
    }
    dragging = null; press = null;
    cv.style.cursor = hovered ? "pointer" : "grab";
    invalidate();
    setTimeout(invalidate, 180); // full labels once the view settles
  });
  cv.addEventListener("pointerleave", () => { if (!press) { hovered = null; hideTip(); invalidate(); } });
  cv.addEventListener("dblclick", (e) => {
    const n = hit(toWorld(e));
    if (n && !n.cls && n.term.type !== "literal") expand(n.term);
    else if (!n) zoomBy(1.6);
  });
  cv.addEventListener("wheel", (e) => {
    e.preventDefault();
    lastWheel = now();
    clearTimeout(wheelEnd);
    wheelEnd = setTimeout(invalidate, 180);
    tween = null;
    const p = toWorld(e);
    if (e.ctrlKey || !e.deltaX || Math.abs(e.deltaY) > 40) zoomAt(view.k * Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.0018)), p.sx, p.sy);
    else { view.x -= e.deltaX; view.y -= e.deltaY; invalidate(); } // two-finger pan on a trackpad
  }, { passive: false });
  cv.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    const p = toWorld(e);
    const n = hit(p);
    if (!n) return hideMenu();
    pick(n);
    const item = (label, fn, hint = "") => h("button", { role: "menuitem", onclick: () => { hideMenu(); fn(); } }, h("span", {}, label), hint ? h("kbd", {}, hint) : null);
    menu.replaceChildren(...[
      h("div", { class: "vg-menu-head" }, labelOf(n)),
      !n.cls && n.term.type !== "literal" ? item(n.expanded ? "Load more neighbours" : "Expand neighbours", () => expand(n.term), "E") : null,
      item("Focus", () => focusOn(n)),
      item("Isolate with neighbours", () => isolate(n)),
      item(n.fixed ? "Unpin" : "Pin in place", () => { n.fixed = !n.fixed; pick(n); relayout(0.3); }, "P"),
      n.term.type === "uri" ? item("Open in explorer", () => go("explore", { iri: n.term.value })) : null,
      n.term.type === "uri" ? item("Copy IRI", () => navigator.clipboard?.writeText(n.term.value).then(() => toast("IRI copied"))) : null,
      h("hr"),
      item("Hide", () => removeNode(n), "Del"),
    ].filter(Boolean));
    menu.hidden = false;
    const r = canvas.getBoundingClientRect();
    menu.style.left = Math.min(p.sx + 6, r.width - 220) + "px";
    menu.style.top = Math.min(p.sy + 6, r.height - menu.offsetHeight - 8) + "px";
  });
  const hideMenu = () => { menu.hidden = true; };
  document.addEventListener("pointerdown", (e) => { if (!menu.contains(e.target) && e.target !== cv) hideMenu(); });

  // the tooltip is rebuilt and measured only when the hovered node changes; moving the
  // pointer just moves it (no layout reads per event)
  let tipFor = null, tipSize = { w: 0, h: 0 };
  function showTip(n, p) {
    if (!n) return hideTip();
    if (tipFor !== n || tip.hidden) {
      const types = [n.primary, ...n.types.filter((t) => t !== n.primary)].filter(Boolean).slice(0, 2).map((t) => abbreviate(t).replace(/^<(.*)>$/, "$1")).join(", ");
      tip.replaceChildren(h("b", {}, labelOf(n)),
        h("span", {}, n.cls ? `class · ${fmt(n.count)} instances` : n.term.type === "literal" ? "literal" : types || (n.term.type === "bnode" ? "blank node" : "resource")),
        h("span", { class: "muted" }, `${n.links.size} links${n.expanded || n.cls || n.term.type === "literal" ? "" : " · double-click to expand"}`));
      tip.hidden = false;
      tipFor = n;
      tipSize = { w: tip.offsetWidth, h: tip.offsetHeight };
    }
    const x = Math.min(p.sx + 14, width - tipSize.w - 8), y = Math.min(p.sy + 14, height - tipSize.h - 8);
    tip.style.transform = `translate(${x}px, ${y}px)`;
  }
  const hideTip = () => { if (!tip.hidden) tip.hidden = true; tipFor = null; };

  cv.addEventListener("keydown", (e) => {
    const n = selected;
    if (e.key === "f" || e.key === "F") fit(true);
    else if (e.key === "+" || e.key === "=") zoomBy(1.4);
    else if (e.key === "-") zoomBy(1 / 1.4);
    else if (e.key === "/") { e.preventDefault(); find.focus(); }
    else if (e.key === "Escape") { pick(null); highlightType = null; hideMenu(); }
    else if (n && (e.key === "e" || e.key === "Enter") && !n.cls && n.term.type !== "literal") expand(n.term);
    else if (n && (e.key === "p" || e.key === "P")) { n.fixed = !n.fixed; pick(n); relayout(0.3); }
    else if (n && (e.key === "Delete" || e.key === "Backspace")) removeNode(n);
    else if (e.key === "Tab" && nodes.size) {
      // walk the nodes by importance
      e.preventDefault();
      const list = [...nodes.values()].sort((a, b) => b.degree - a.degree);
      const next = list[(list.indexOf(n) + (e.shiftKey ? list.length - 1 : 1)) % list.length];
      pick(next); focusOn(next);
    } else return;
    invalidate();
  });
  find.addEventListener("input", () => { query = find.value.trim(); highlightType = null; invalidate(); });
  find.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { find.value = ""; query = ""; cv.focus(); invalidate(); }
    if (e.key === "Enter" && query) {
      const hits = [...(focusSet() || [])];
      if (!hits.length) return toast("No node in the view matches", true);
      const n = hits.sort((a, b) => b.degree - a.degree)[0];
      pick(n); focusOn(n);
    }
  });
  mini.addEventListener("pointerdown", (e) => {
    const go = (ev) => {
      if (!miniMap) return;
      const r = mini.getBoundingClientRect();
      const wx = (ev.clientX - r.left - miniMap.ox) / miniMap.s, wy = (ev.clientY - r.top - miniMap.oy) / miniMap.s;
      view.x = width / 2 - wx * view.k; view.y = height / 2 - wy * view.k;
      invalidate();
    };
    mini.setPointerCapture(e.pointerId);
    go(e);
    const up = () => { mini.removeEventListener("pointermove", go); mini.removeEventListener("pointerup", up); };
    mini.addEventListener("pointermove", go);
    mini.addEventListener("pointerup", up);
  });

  // ------------------------------------------------------------ misc
  function clear() {
    nodes.clear(); edges.clear(); details.clear(); selected = null; hovered = null; moves = null; alpha = 0;
    typeColor.clear();
    pick(null);
    empty.hidden = false; empty.textContent = "Nothing to show yet.";
    renderLegend();
    fit();
  }

  function exportPng() {
    const out = h("canvas");
    out.width = cv.width; out.height = cv.height;
    out.getContext("2d").drawImage(cv, 0, 0);
    out.toBlob((blob) => {
      const url = URL.createObjectURL(blob);
      const a = h("a", { href: url, download: "graph.png" });
      document.body.append(a); a.click(); a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    }, "image/png");
  }

  function showEdgeLabels(on) {
    edgeLabels = on;
    invalidate();
  }

  return { canvas, side, opts, expand, loadGraph, loadSchema, clear, fit: () => fit(true), exportPng, showEdgeLabels, applyLiteralFilter, setLayout };
}
