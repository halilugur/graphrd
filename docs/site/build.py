#!/usr/bin/env python3
"""Build GraphRD's documentation as one HTML file in the app's own UI style.

Reads the repo's docs/*.md and writes /tmp/grd-docs/index.html — the same
shell, sidebar, panels, tables and code editor look as ui/style.css.
"""
import html
import re
from pathlib import Path

DOCS = Path(__file__).resolve().parent.parent
OUT = Path(__file__).with_name("index.html")

# page order + (icon, group) — icons reuse the app sidebar's stroke style
PAGES = [
    ("getting-started", "Getting started", "home", "Basics"),
    ("cli", "Command line", "term", "Basics"),
    ("http-api", "HTTP API", "link", "Interfaces"),
    ("sparql", "SPARQL", "code", "Interfaces"),
    ("graphql", "GraphQL", "tri", "Interfaces"),
    ("sql", "SQL", "db", "Interfaces"),
    ("nl-query", "Questions in plain language", "spark", "Interfaces"),
    ("formats", "Formats", "layers", "Interfaces"),
    ("examples", "Examples", "box", "Interfaces"),
    ("inference", "Inference", "branch", "Knowledge"),
    ("fts", "Full-text search", "find", "Knowledge"),
    ("connectors", "Search connectors", "plug", "Knowledge"),
    ("vectors", "Vector search", "nodes", "Knowledge"),
    ("geosparql", "GeoSPARQL", "pin", "Knowledge"),
    ("shacl", "SHACL", "shield", "Knowledge"),
    ("admin", "Administration", "gear", "Operations"),
    ("cluster", "Cluster", "cluster", "Operations"),
    ("storage", "Storage", "disk", "Operations"),
    ("performance", "Performance", "pulse", "Operations"),
    ("security", "Security", "lock", "Operations"),
]
GROUPS = ["Basics", "Interfaces", "Knowledge", "Operations"]

ICONS = {
    "home": 'M3 9l7-6 7 6v8H3z',
    "term": 'M3 5h14M3 10h9M3 15h14M15.5 8.5l2 1.5-2 1.5',
    "link": 'M8.5 11.5a3 3 0 0 0 4.2 0l2.6-2.6a3 3 0 0 0-4.2-4.2l-1 1M11.5 8.5a3 3 0 0 0-4.2 0l-2.6 2.6a3 3 0 0 0 4.2 4.2l1-1',
    "code": 'M7 5L2 10l5 5M13 5l5 5-5 5',
    "tri": 'M10 2.5l6.5 3.75v7.5L10 17.5l-6.5-3.75v-7.5zM10 2.5L3.5 13.75h13z',
    "db": 'M4 5c0-1.4 2.7-2.5 6-2.5s6 1.1 6 2.5-2.7 2.5-6 2.5S4 6.4 4 5zM4 5v10c0 1.4 2.7 2.5 6 2.5s6-1.1 6-2.5V5M4 10c0 1.4 2.7 2.5 6 2.5s6-1.1 6-2.5',
    "spark": 'M8.5 3.5l1.4 3.6 3.6 1.4-3.6 1.4-1.4 3.6-1.4-3.6-3.6-1.4 3.6-1.4zM14.5 11.5l.7 1.8 1.8.7-1.8.7-.7 1.8-.7-1.8-1.8-.7 1.8-.7z',
    "layers": 'M10 3l7 3.5-7 3.5-7-3.5zM3 10l7 3.5 7-3.5M3 13.5l7 3.5 7-3.5',
    "branch": 'M6 3v9m0 0a3 3 0 1 0 0 6 3 3 0 0 0 0-6zm8-6a3 3 0 1 0 0 6 3 3 0 0 0 0-6zm0 6c0 4-8 2-8 6',
    "find": 'M8.5 3.5a5 5 0 1 0 0 10 5 5 0 0 0 0-10zm3.6 8.4L17 16.5',
    "plug": 'M7 3v4m6-4v4M5 7h10v3a5 5 0 0 1-5 5 5 5 0 0 1-5-5zm5 8v2',
    "nodes": 'M5 5a2.3 2.3 0 1 0 0-.01M15 7a2.3 2.3 0 1 0 0-.01M8 15.5a2.3 2.3 0 1 0 0-.01M7.2 5.5l5.6 1.1M13.6 8.8l-4.2 5M6.3 7.1l1.2 6.1',
    "pin": 'M10 17s-5.5-4.6-5.5-9a5.5 5.5 0 0 1 11 0c0 4.4-5.5 9-5.5 9zm0-11.5a2.5 2.5 0 1 0 0 5 2.5 2.5 0 0 0 0-5z',
    "shield": 'M10 3l6 2.2v4.3c0 3.8-2.6 6.4-6 7.5-3.4-1.1-6-3.7-6-7.5V5.2zm-2.8 6.9l2 2 3.6-3.8',
    "gear": 'M10 7.5a2.5 2.5 0 1 0 0 5 2.5 2.5 0 0 0 0-5zM16.5 10c0 .5-.05.98-.14 1.44l1.65 1.3-1.6 2.77-1.97-.66c-.72.6-1.55 1.06-2.45 1.33L11.6 18H8.4l-.49-1.82c-.9-.27-1.73-.73-2.45-1.33l-1.97.66-1.6-2.77 1.65-1.3A6.6 6.6 0 0 1 3.5 10c0-.5.05-.98.14-1.44L2 7.26l1.6-2.77 1.97.66c.72-.6 1.55-1.06 2.45-1.33L8.4 2h3.2l.49 1.82c.9.27 1.73.73 2.45 1.33l1.97-.66 1.6 2.77-1.65 1.3c.09.46.14.94.14 1.44z',
    "cluster": 'M4.5 10a2.5 2.5 0 1 0 0-.01M15 4.5a2.5 2.5 0 1 0 0-.01M15 15.5a2.5 2.5 0 1 0 0-.01M6.5 8.9L13 5.6M6.5 11.1L13 14.4',
    "disk": 'M4 6c0-1.4 2.7-2.5 6-2.5S16 4.6 16 6 13.3 8.5 10 8.5 4 7.4 4 6zM4 6v8c0 1.4 2.7 2.5 6 2.5s6-1.1 6-2.5V6M4 10c0 1.4 2.7 2.5 6 2.5s6-1.1 6-2.5',
    "pulse": 'M2.5 11h3l2-5 3 9 2-6 1.5 2h3.5',
    "lock": 'M5.5 8.5V7a4.5 4.5 0 0 1 9 0v1.5M4.5 8.5h11v8h-11zM10 11.5v2.5',
    "box": 'M3 7l7-4 7 4v6l-7 4-7-4zM3 7l7 4 7-4M10 11v6',
}

SPARQL_KW = {
    "select", "distinct", "reduced", "where", "prefix", "base", "filter", "optional",
    "union", "minus", "graph", "service", "values", "bind", "as", "construct", "describe",
    "ask", "insert", "delete", "data", "with", "using", "load", "clear", "drop", "create",
    "add", "move", "copy", "from", "having", "group", "order", "asc", "desc", "limit",
    "offset", "by", "not", "in", "exists", "strstarts", "strends", "contains", "str",
    "lang", "datatype", "bound", "sameTerm", "true", "false", "a", "isiri", "isblank",
    "isliteral", "istriple", "triple", "subject", "predicate", "object", "reifies",
}
SQL_KW = {
    "select", "from", "where", "create", "table", "view", "drop", "join", "left",
    "inner", "outer", "on", "group", "order", "by", "limit", "offset", "as", "and",
    "or", "not", "null", "is", "in", "insert", "into", "values", "update", "set",
    "delete", "distinct", "having", "asc", "desc", "with", "cast", "case", "when",
    "then", "else", "end", "true", "false",
}


def slug(text: str) -> str:
    return re.sub(r"[^a-z0-9 -]", "", text.lower()).strip().replace(" ", "-")


def hl_generic(code: str) -> str:
    return html.escape(code)


def hl_sparql(code: str) -> str:
    pat = re.compile(
        r"(?P<com>#[^\n]*)"
        r"|(?P<iri>&lt;[^&\n]*&gt;)"
        r'|(?P<str>&quot;(?:[^&\\]|&(?!quot;)|\\.)*?&quot;|\'(?:[^\'\\]|\\.)*?\')'
        r"|(?P<var>[?$][A-Za-z_][\w]*)"
        r"|(?P<num>\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b)"
        r"|(?P<pn>[A-Za-z_][\w-]*:[A-Za-z_][\w.-]*)"
        r"|(?P<word>[A-Za-z_][\w-]*)"
    )

    def rep(m):
        if m.group("com"):
            return f'<span class="tok-com">{m.group()}</span>'
        if m.group("iri"):
            return f'<span class="tok-iri">{m.group()}</span>'
        if m.group("str"):
            return f'<span class="tok-str">{m.group()}</span>'
        if m.group("var"):
            return f'<span class="tok-var">{m.group()}</span>'
        if m.group("num"):
            return f'<span class="tok-num">{m.group()}</span>'
        if m.group("pn"):
            return f'<span class="tok-pn">{m.group()}</span>'
        if m.group("word").lower() in SPARQL_KW:
            return f'<span class="tok-kw">{m.group()}</span>'
        return m.group()

    return pat.sub(rep, html.escape(code))


def hl_bash(code: str) -> str:
    out = []
    for line in html.escape(code).split("\n"):
        m = re.match(r"^(\s*)(\S+)(.*)$", line)
        if m and not line.lstrip().startswith("#"):
            head, cmd, rest = m.groups()
            rest = re.sub(
                r'(&quot;[^&]*?&quot;|&#x27;[^&]*?&#x27;)', r'<span class="tok-str">\1</span>', rest)
            rest = re.sub(r"(\$\{?\w+\}?)", r'<span class="tok-var">\1</span>', rest)
            rest = re.sub(r"(?<!\w)(--?[A-Za-z][\w-]*)", r'<span class="tok-pn">\1</span>', rest)
            line = f'{head}<span class="tok-kw">{cmd}</span>{rest}'
        else:
            line = f'<span class="tok-com">{line}</span>'
        out.append(line)
    return "\n".join(out)


def hl_json(code: str) -> str:
    pat = re.compile(
        r'(?P<key>&quot;[^&\\]*?&quot;)(?=\s*:)'
        r'|(?P<str>&quot;(?:[^&\\]|&(?!quot;)|\\.)*?&quot;)'
        r"|(?P<num>-?\b\d+(?:\.\d+)?\b)"
        r"|(?P<kw>\b(?:true|false|null)\b)"
    )

    def rep(m):
        if m.group("key"):
            return f'<span class="tok-pn">{m.group()}</span>'
        if m.group("str"):
            return f'<span class="tok-str">{m.group()}</span>'
        if m.group("num"):
            return f'<span class="tok-num">{m.group()}</span>'
        return f'<span class="tok-kw">{m.group()}</span>'

    return pat.sub(rep, html.escape(code))


def hl_yaml(code: str) -> str:
    pat = re.compile(
        r"(?P<com>#[^\n]*)"
        r'|(?P<key>^\s*-?\s*[\w.-]+)(?=:)'
        r'|(?P<str>&quot;[^&]*?&quot;|&#x27;[^&]*?&#x27;)'
        r"|(?P<num>\b\d+(?:\.\d+)?\b)"
        r"|(?P<kw>\b(?:true|false|null)\b)",
        re.M,
    )

    def rep(m):
        if m.group("com"):
            return f'<span class="tok-com">{m.group()}</span>'
        if m.group("key"):
            return f'<span class="tok-pn">{m.group()}</span>'
        if m.group("str"):
            return f'<span class="tok-str">{m.group()}</span>'
        if m.group("num"):
            return f'<span class="tok-num">{m.group()}</span>'
        return f'<span class="tok-kw">{m.group()}</span>'

    return pat.sub(rep, html.escape(code))


def hl_sql(code: str) -> str:
    pat = re.compile(
        r"(?P<com>--[^\n]*)"
        r'|(?P<str>&#x27;[^&]*?&#x27;)'
        r"|(?P<num>\b\d+(?:\.\d+)?\b)"
        r"|(?P<word>[A-Za-z_][\w]*)"
    )

    def rep(m):
        if m.group("com"):
            return f'<span class="tok-com">{m.group()}</span>'
        if m.group("str"):
            return f'<span class="tok-str">{m.group()}</span>'
        if m.group("num"):
            return f'<span class="tok-num">{m.group()}</span>'
        if m.group("word").upper() in SQL_KW:
            return f'<span class="tok-kw">{m.group()}</span>'
        return m.group()

    return pat.sub(rep, html.escape(code))


HIGHLIGHT = {
    "sparql": hl_sparql, "turtle": hl_sparql, "bash": hl_bash, "sh": hl_bash,
    "json": hl_json, "yaml": hl_yaml, "sql": hl_sql,
}
LANG_LABEL = {"sparql": "SPARQL", "turtle": "Turtle", "bash": "Shell", "sh": "Shell",
              "json": "JSON", "yaml": "YAML", "sql": "SQL", "": "Text"}


def inline(text: str) -> str:
    """Inline markdown → HTML (escapes first, then patterns on escaped text)."""
    t = html.escape(text)
    t = re.sub(r"`([^`]+)`", r"<code>\1</code>", t)
    t = re.sub(r"\[([^\]]+)\]\(([^)\s]+)\)", link, t)
    t = re.sub(r"&lt;(https?://[^&]+)&gt;", r'<a href="\1" target="_blank" rel="noopener">\1</a>', t)
    t = re.sub(r"\*\*([^*]+)\*\*", r"<b>\1</b>", t)
    t = re.sub(r"(?<!\*)\*([^*\n]+)\*(?!\*)", r"<i>\1</i>", t)
    t = re.sub(r"(?<![\w>])((?:https?://)[^\s<)]+)(?![\w<])",
               r'<a href="\1" target="_blank" rel="noopener">\1</a>', t)
    return t


def link(m) -> str:
    text, href = m.group(1), m.group(2)
    if href.endswith(".md"):
        href = "#/" + href[:-3]
    elif ".md#" in href:
        page, anchor = href.split(".md#", 1)
        href = f"#/{page}/{anchor}"
    else:
        return f'<a href="{href}" target="_blank" rel="noopener">{text}</a>'
    return f'<a href="{href}">{text}</a>'


def parse_md(src: str):
    """Split a page into (title, lede, body-html)."""
    lines = src.split("\n")
    title = ""
    lede = ""
    body: list[str] = []
    i, n = 0, len(lines)
    para: list[str] = []
    list_open = None  # "ul" | "ol"

    def close_para():
        nonlocal para
        if para:
            body.append(f"<p>{inline(' '.join(para))}</p>")
            para = []

    def close_list():
        nonlocal list_open
        if list_open:
            body.append(f"</{list_open}>")
            list_open = None

    while i < n:
        line = lines[i]
        stripped = line.lstrip()
        if stripped.startswith("```"):
            close_para(); close_list()
            # the fence may sit inside a list item; keep its indentation off the code
            indent = line[:len(line) - len(stripped)]
            lang = stripped[3:].strip()
            code = []
            i += 1
            while i < n and not lines[i].lstrip().startswith("```"):
                raw = lines[i]
                if indent and raw.startswith(indent):
                    raw = raw[len(indent):]
                code.append(raw); i += 1
            text = "\n".join(code)
            hl = HIGHLIGHT.get(lang, hl_generic)(text)
            label = LANG_LABEL.get(lang, lang or "Text")
            body.append(
                f'<div class="codebox"><div class="code-head"><span class="eyebrow">{label}</span>'
                f'<button class="copy" type="button">Copy</button></div>'
                f"<pre><code>{hl}</code></pre></div>")
            i += 1
            continue
        m = re.match(r"^(#{1,4})\s+(.*)$", line)
        if m:
            close_para(); close_list()
            level = len(m.group(1)) + 1  # md # is the page title → h2 in-section
            text = m.group(2).strip()
            if not title and level == 2:
                title = text
                i += 1
                continue
            body.append(f'<h{level} id="{slug(text)}">{inline(text)}</h{level}>')
            i += 1
            continue
        if re.match(r"^\|.*\|\s*$", line):
            close_para(); close_list()
            rows = []
            while i < n and re.match(r"^\|.*\|\s*$", lines[i]):
                cells = [c.strip() for c in lines[i].strip().strip("|").split("|")]
                rows.append(cells); i += 1
            if len(rows) >= 2 and re.match(r"^:?-{2,}:?$", rows[1][0].replace("-", "-")):
                head, data = rows[0], rows[2:]
            else:
                head, data = None, rows
            out = ['<div class="table-wrap"><table>']
            if head:
                out.append("<thead><tr>" + "".join(f"<th>{inline(c)}</th>" for c in head) + "</tr></thead>")
            out.append("<tbody>")
            for r in data:
                out.append("<tr>" + "".join(f"<td>{inline(c)}</td>" for c in r) + "</tr>")
            out.append("</tbody></table></div>")
            body.append("".join(out))
            continue
        m = re.match(r"^\s*[-*]\s+(.*)$", line)
        if m and not re.match(r"^\|", line):
            close_para()
            if list_open != "ul":
                close_list(); body.append("<ul>"); list_open = "ul"
            item = m.group(1)
            # folded "- **x**: y" keeps bold; nested lists not used in docs
            body.append(f"<li>{inline(item)}</li>")
            i += 1
            continue
        m = re.match(r"^\s*\d+[.)]\s+(.*)$", line)
        if m:
            close_para()
            if list_open != "ol":
                close_list(); body.append("<ol>"); list_open = "ol"
            body.append(f"<li>{inline(m.group(1))}</li>")
            i += 1
            continue
        if not line.strip():
            close_para(); close_list()
            i += 1
            continue
        close_para()
        para.append(line.strip())
        i += 1
    close_para(); close_list()
    # first paragraph right after the title is the lede (kept as plain text,
    # so callers escape it exactly once)
    if body and body[0].startswith("<p>") and not lede:
        lede = html.unescape(re.sub(r"<[^>]+>", "", body[0]))
        body.pop(0)
    return title, lede, "\n".join(body)


CSS = """
/* GraphRD docs — the app's own design language (ui/style.css), one file. */
:root {
  --bg: #f3f5f8; --surface: #ffffff; --sunken: #eaeef3;
  --line: #d8dee6; --line-strong: #c3ccd7;
  --fg: #17202b; --muted: #5a6777; --faint: #8a96a5;
  --accent: #0e6c88; --accent-fg: #ffffff; --accent-soft: #dcedf2;
  --role-s: #2356c4; --role-p: #a35f08; --role-o: #0d786c; --role-lit: #8b3a5d; --role-b: #6a49c0;
  --ok: #16794a; --warn: #a8670b; --danger: #b4232a; --danger-soft: #fbe6e6;
  --code-bg: #fbfcfd;
  --shadow: 0 1px 0 rgba(23,32,43,.04), 0 6px 20px -8px rgba(23,32,43,.12);
  --display: "Avenir Next", "Segoe UI Variable Display", "Segoe UI", system-ui, sans-serif;
  --sans: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", sans-serif;
  --mono: "SF Mono", "JetBrains Mono", "Cascadia Code", ui-monospace, Menlo, Consolas, monospace;
  --step--1: .8125rem; --step-0: .875rem; --step-1: 1rem; --step-2: 1.25rem; --step-3: 1.75rem;
  --radius: 10px; --radius-sm: 6px; --sidebar: 248px;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --bg: #0d1116; --surface: #141a21; --sunken: #1a2129; --line: #252e39; --line-strong: #33404d;
    --fg: #e1e7ee; --muted: #93a0b0; --faint: #66727f; --accent: #4cb2cf; --accent-fg: #071318; --accent-soft: #15303a;
    --role-s: #82a8ff; --role-p: #e3a552; --role-o: #43c3ae; --role-lit: #e591b6; --role-b: #b39cff;
    --ok: #4cc287; --warn: #e3a552; --danger: #ff7b7f; --danger-soft: #3a1c1f; --code-bg: #10151b;
    --shadow: 0 1px 0 rgba(0,0,0,.3), 0 8px 24px -10px rgba(0,0,0,.6); color-scheme: dark;
  }
}
:root[data-theme="dark"] {
  --bg: #0d1116; --surface: #141a21; --sunken: #1a2129; --line: #252e39; --line-strong: #33404d;
  --fg: #e1e7ee; --muted: #93a0b0; --faint: #66727f; --accent: #4cb2cf; --accent-fg: #071318; --accent-soft: #15303a;
  --role-s: #82a8ff; --role-p: #e3a552; --role-o: #43c3ae; --role-lit: #e591b6; --role-b: #b39cff;
  --ok: #4cc287; --warn: #e3a552; --danger: #ff7b7f; --danger-soft: #3a1c1f; --code-bg: #10151b;
  --shadow: 0 1px 0 rgba(0,0,0,.3), 0 8px 24px -10px rgba(0,0,0,.6); color-scheme: dark;
}
* { box-sizing: border-box; }
html, body { margin: 0; height: 100%; }
body { background: var(--bg); color: var(--fg); font: var(--step-0)/1.6 var(--sans); -webkit-font-smoothing: antialiased; }
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
svg { flex: none; }

.shell { display: grid; grid-template-columns: var(--sidebar) minmax(0, 1fr); min-height: 100%; }
.sidebar { position: sticky; top: 0; height: 100vh; overflow-y: auto; display: flex; flex-direction: column; gap: 18px;
  padding: 18px 14px calc(14px + env(safe-area-inset-bottom, 0px)); background: var(--sunken); border-right: 1px solid var(--line); }
.sidebar-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.brand { display: flex; align-items: center; gap: 10px; padding: 2px 8px; color: var(--fg); font: 600 var(--step-2)/1 var(--display); letter-spacing: -.01em; }
.brand b { font-weight: 800; color: var(--accent); }
.brand:hover { text-decoration: none; }
.brand .doc-tag { font: 600 10.5px var(--sans); letter-spacing: .08em; text-transform: uppercase; color: var(--faint); margin-top: 6px; }
.mark { width: 28px; height: 28px; color: var(--faint); }
.mark .s { fill: var(--role-s); } .mark .p { fill: var(--role-p); } .mark .o { fill: var(--role-o); }
.topbar-btn { display: grid; place-items: center; width: 38px; height: 38px; padding: 0; border: 0; border-radius: 10px; background: transparent; color: var(--fg); cursor: pointer; }
.topbar-btn:hover { background: var(--surface); }
.topbar-btn svg { width: 20px; height: 20px; fill: none; stroke: currentColor; stroke-width: 1.8; stroke-linecap: round; }
#menu-close { display: none; }

.eyebrow { display: block; font-size: 11px; font-weight: 650; letter-spacing: .08em; text-transform: uppercase; color: var(--faint); }
.search-card { display: flex; flex-direction: column; gap: 8px; padding: 12px; background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius); }
input[type=search] { font: inherit; color: var(--fg); background: var(--surface); border: 1px solid var(--line-strong); border-radius: var(--radius-sm); padding: 7px 10px; min-width: 0; }
input::placeholder { color: var(--faint); }
input:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }

#nav { display: flex; flex-direction: column; gap: 2px; }
.nav-group { margin: 10px 10px 4px; }
.nav-group:first-child { margin-top: 0; }
#nav a { display: flex; align-items: center; gap: 10px; padding: 7px 10px; border-radius: var(--radius-sm); color: var(--muted); font-weight: 550; }
#nav a svg { width: 17px; height: 17px; fill: none; stroke: currentColor; stroke-width: 1.6; stroke-linecap: round; stroke-linejoin: round; }
#nav a:hover { background: color-mix(in srgb, var(--surface) 70%, transparent); color: var(--fg); text-decoration: none; }
#nav a.active { background: var(--surface); color: var(--fg); box-shadow: inset 0 0 0 1px var(--line); }
#nav a.active svg { stroke: var(--accent); }
.sidebar-foot { margin-top: auto; display: flex; gap: 6px; flex-wrap: wrap; }
.sidebar-foot svg { width: 16px; height: 16px; fill: none; stroke: currentColor; stroke-width: 1.6; }
.sidebar-foot svg .fill { fill: currentColor; stroke: none; }
button.ghost { display: inline-flex; align-items: center; gap: 7px; font: 600 var(--step-0)/1 var(--sans); cursor: pointer; white-space: nowrap;
  padding: 7px 10px; border-radius: var(--radius-sm); border: 1px solid transparent; background: transparent; color: var(--muted); }
button.ghost:hover { background: var(--surface); color: var(--fg); }

main { min-width: 0; padding-block: 28px 64px; padding-inline: clamp(16px, 4vw, 44px); outline: none; }
.page { max-width: 980px; margin: 0 auto; display: none; flex-direction: column; gap: 22px; }
.page.show { display: flex; }
.page-head { display: flex; align-items: flex-end; justify-content: space-between; gap: 16px; flex-wrap: wrap; }
.page-head > div { min-width: 0; }
h1 { margin: 0; font: 650 var(--step-3)/1.15 var(--display); letter-spacing: -.02em; text-wrap: balance; }
.lede { margin: 6px 0 0; color: var(--muted); max-width: 64ch; }

.panel { background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius); box-shadow: var(--shadow); min-width: 0; }

/* prose */
.doc { display: flex; flex-direction: column; gap: 14px; }
.doc h2 { margin: 18px 0 2px; padding-top: 14px; border-top: 1px solid var(--line); font: 650 var(--step-2)/1.25 var(--display); letter-spacing: -.01em; scroll-margin-top: 16px; }
.doc > .panel + h2, .doc > .codebox + h2 { margin-top: 20px; }
.doc h3 { margin: 10px 0 0; font: 650 var(--step-1)/1.3 var(--display); scroll-margin-top: 16px; }
.doc h4 { margin: 8px 0 0; font: 650 var(--step-0)/1.3 var(--display); }
.doc p { margin: 0; }
.doc ul, .doc ol { margin: 0; padding-left: 22px; display: flex; flex-direction: column; gap: 5px; }
.doc li::marker { color: var(--faint); }
code { font: 500 .92em var(--mono); background: var(--sunken); border: 1px solid var(--line); border-radius: 5px; padding: .08em .35em; overflow-wrap: anywhere; color: var(--role-o); }
.doc a code { color: var(--accent); }

/* code blocks: the editor's colours, in a copyable panel */
.codebox { background: var(--code-bg); border: 1px solid var(--line); border-radius: var(--radius); box-shadow: var(--shadow); overflow: hidden; }
.code-head { display: flex; align-items: center; justify-content: space-between; gap: 10px; padding: 8px 12px 7px; border-bottom: 1px solid var(--line); background: color-mix(in srgb, var(--surface) 55%, var(--code-bg)); }
.codebox pre { margin: 0; padding: 13px 16px; overflow: auto; font: var(--step--1)/1.65 var(--mono); tab-size: 2; }
.codebox code { all: unset; font: inherit; }
button.copy { font: 600 11px var(--sans); cursor: pointer; padding: 3px 10px; border-radius: 5px; border: 1px solid var(--line-strong); background: var(--surface); color: var(--muted); }
button.copy:hover { background: var(--sunken); color: var(--fg); }
.tok-kw { color: var(--accent); font-weight: 650; }
.tok-var { color: var(--role-s); }
.tok-iri { color: var(--role-o); }
.tok-pn { color: var(--role-p); }
.tok-str { color: var(--role-lit); }
.tok-num { color: var(--role-lit); }
.tok-com { color: var(--faint); font-style: italic; }

/* tables, straight from the app */
.table-wrap { overflow: auto; border: 1px solid var(--line); border-radius: var(--radius); box-shadow: var(--shadow); background: var(--surface); }
table { border-collapse: separate; border-spacing: 0; width: 100%; font-size: var(--step--1); }
th, td { text-align: left; padding: 7px 12px; border-bottom: 1px solid var(--line); vertical-align: top; }
th { background: var(--sunken); color: var(--muted); font: 650 11px/1.4 var(--sans); letter-spacing: .06em; text-transform: uppercase; }
tbody tr:hover td { background: color-mix(in srgb, var(--sunken) 60%, transparent); }
tbody tr:last-child td { border-bottom: 0; }

/* overview cards */
.cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(250px, 1fr)); gap: 14px; }
a.card { display: flex; flex-direction: column; gap: 6px; padding: 16px; background: var(--surface); border: 1px solid var(--line);
  border-radius: var(--radius); box-shadow: var(--shadow); color: var(--fg); transition: border-color .12s, transform .12s; }
a.card:hover { text-decoration: none; border-color: var(--accent); transform: translateY(-1px); }
a.card svg { width: 20px; height: 20px; fill: none; stroke: var(--accent); stroke-width: 1.6; stroke-linecap: round; stroke-linejoin: round; }
a.card .t { font: 650 var(--step-1)/1.3 var(--display); }
a.card .d { font-size: var(--step--1); color: var(--muted); }

.pager { display: flex; justify-content: space-between; gap: 12px; }
.pager a { display: flex; flex-direction: column; gap: 2px; padding: 12px 16px; background: var(--surface); border: 1px solid var(--line);
  border-radius: var(--radius); box-shadow: var(--shadow); min-width: 0; }
.pager a:hover { text-decoration: none; border-color: var(--accent); }
.pager .eyebrow { margin-bottom: 2px; }
.pager a.next { text-align: right; }
.pager .t { font: 650 var(--step-0) var(--display); color: var(--accent); }

/* mobile: the app's drawer */
.topbar, .scrim { display: none; }
@media (max-width: 860px) {
  .shell { grid-template-columns: minmax(0, 1fr); }
  .topbar { position: sticky; top: 0; z-index: 30; display: flex; align-items: center; gap: 6px;
    padding: calc(6px + env(safe-area-inset-top, 0px)) 10px 6px; grid-column: 1 / -1;
    background: color-mix(in srgb, var(--sunken) 88%, transparent);
    -webkit-backdrop-filter: blur(12px) saturate(1.4); backdrop-filter: blur(12px) saturate(1.4);
    border-bottom: 1px solid var(--line); }
  .topbar-mark { display: grid; place-items: center; width: 34px; height: 34px; }
  .topbar-mark .mark { width: 24px; height: 24px; }
  .topbar-title { font: 650 var(--step-1) var(--display); }
  .sidebar { position: fixed; z-index: 50; top: 0; bottom: 0; left: 0; height: auto; width: min(300px, 86vw);
    padding-top: calc(14px + env(safe-area-inset-top, 0px)); transform: translateX(-102%); visibility: hidden;
    transition: transform .22s cubic-bezier(.2,.8,.2,1), visibility 0s linear .22s; box-shadow: 0 0 40px -8px rgba(0,0,0,.45); }
  body.drawer-open .sidebar { transform: none; visibility: visible; transition: transform .22s cubic-bezier(.2,.8,.2,1); }
  body.drawer-open { overflow: hidden; }
  #menu-close { display: grid; }
  .scrim { display: block; position: fixed; inset: 0; z-index: 40; background: rgba(5,10,16,.5); -webkit-backdrop-filter: blur(2px); backdrop-filter: blur(2px); }
  .scrim[hidden] { display: none; }
  main { padding-block: 20px 48px; }
}
@media (prefers-reduced-motion: reduce) { .sidebar, body.drawer-open .sidebar { transition: none; } }
"""

JS = """
const pages = Array.from(document.querySelectorAll('.page'));
const navLinks = Array.from(document.querySelectorAll('#nav a'));
function go() {
  let [_, page, anchor] = decodeURIComponent(location.hash).split('/');
  page = page || 'overview';
  let shown = pages.find(p => p.dataset.page === page);
  if (!shown) { page = 'overview'; shown = pages.find(p => p.dataset.page === 'overview'); }
  pages.forEach(p => p.classList.toggle('show', p === shown));
  navLinks.forEach(a => a.classList.toggle('active', a.getAttribute('href') === '#/' + page));
  document.getElementById('topbar-page').textContent = shown.dataset.title;
  document.title = shown.dataset.title + ' — GraphRD docs';
  if (anchor) {
    const el = shown.querySelector('#' + CSS.escape(anchor) + ', a[name="' + anchor + '"]');
    if (el) { el.scrollIntoView(); return; }
  }
  scrollTo({ top: 0 });
}
addEventListener('hashchange', go);
navLinks.forEach(a => a.addEventListener('click', () => document.body.classList.remove('drawer-open')));

// theme: system by default, the button pins light/dark (like the app)
const root = document.documentElement, tbtn = document.getElementById('theme-toggle');
function setTheme(t) { t ? root.dataset.theme = t : delete root.dataset.theme; tbtn.querySelector('span').textContent = t === 'dark' || (!t && matchMedia('(prefers-color-scheme: dark)').matches) ? 'Light' : 'Dark'; try { localStorage.setItem('grd-docs-theme', t || ''); } catch (e) {} }
tbtn.addEventListener('click', () => { const cur = root.dataset.theme || (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'); setTheme(cur === 'dark' ? 'light' : 'dark'); });
let saved = ''; try { saved = localStorage.getItem('grd-docs-theme') || ''; } catch (e) {}
if (saved) root.dataset.theme = saved;

// mobile drawer
const menu = document.getElementById('menu-toggle');
menu.addEventListener('click', () => { document.body.classList.add('drawer-open'); menu.setAttribute('aria-expanded', 'true'); });
document.getElementById('menu-close').addEventListener('click', () => document.body.classList.remove('drawer-open'));
document.getElementById('scrim').addEventListener('click', () => document.body.classList.remove('drawer-open'));

// page filter — hides empty group labels too
const q = document.getElementById('q');
q.addEventListener('input', () => {
  const v = q.value.trim().toLowerCase();
  navLinks.forEach(a => a.style.display = !v || a.textContent.toLowerCase().includes(v) ? '' : 'none');
  document.querySelectorAll('#nav .nav-group').forEach(g => {
    let el = g.nextElementSibling, any = false;
    while (el && el.tagName === 'A') { if (el.style.display !== 'none') any = true; el = el.nextElementSibling; }
    g.style.display = any ? '' : 'none';
  });
});
q.addEventListener('keydown', e => { if (e.key === 'Enter') { const first = navLinks.find(a => a.style.display !== 'none'); if (first && q.value.trim()) first.click(); } });

// copy buttons
document.addEventListener('click', e => {
  const b = e.target.closest('button.copy');
  if (!b) return;
  const code = b.closest('.codebox').querySelector('code');
  navigator.clipboard.writeText(code.textContent).then(() => {
    b.textContent = 'Copied';
    setTimeout(() => b.textContent = 'Copy', 1200);
  }).catch(() => {});
});
go();
"""


def svg_icon(name: str) -> str:
    return f'<svg viewBox="0 0 20 20" aria-hidden="true"><path d="{ICONS[name]}"/></svg>'


def write_llms(titles: dict, ledes: dict) -> None:
    """llms.txt (a map) and llms-full.txt (everything in one file) for agents
    that read a site instead of browsing it. The map also lands in docs/."""
    base = "https://graphrd.org"
    lines = [
        "# GraphRD",
        "",
        "> An RDF database in one binary: SPARQL 1.1 and 1.2, rule-based inference,",
        "> full-text, vector and geospatial search, SHACL, GraphQL, SQL — with a web UI inside.",
        "",
        "Docs:",
        "",
    ]
    for pid, _t, _i, _g in PAGES:
        first = html.unescape(ledes.get(pid, "")).strip().split("\n")[0]
        entry = f"- [{titles[pid]}]({base}/{pid}.md)"
        if first:
            entry += f": {first}"
        lines.append(entry)
    lines += ["", f"- [Full documentation]({base}/llms-full.txt): every page in one file"]
    map_text = "\n".join(lines) + "\n"
    OUT.parent.joinpath("llms.txt").write_text(map_text)
    DOCS.joinpath("llms.txt").write_text(map_text)

    full = ["# GraphRD — full documentation\n"]
    for pid, _t, _i, _g in PAGES:
        src = open_doc(f"{pid}.md")
        full.append(f"\n\n---\n\n{src.strip()}\n")
    OUT.parent.joinpath("llms-full.txt").write_text("".join(full))


def open_doc(name: str) -> str:
    return (DOCS / name).read_text()


def build() -> None:
    OUT.parent.mkdir(parents=True, exist_ok=True)
    sections = []
    cards = []
    nav = []
    titles = {}
    ledes = {}
    for i, (pid, ptitle, icon, group) in enumerate(PAGES):
        src = (DOCS / f"{pid}.md").read_text()
        title, lede, body = parse_md(src)
        titles[pid] = title or ptitle
        ledes[pid] = lede
        prev = PAGES[i - 1] if i else None
        nxt = PAGES[i + 1] if i + 1 < len(PAGES) else None
        pager = '<div class="pager">'
        pager += f'<a href="#/{prev[0]}" class="prev"><span class="eyebrow">Previous</span><span class="t">{titles.get(prev[0], prev[1])}</span></a>' if prev else '<span></span>'
        pager += f'<a href="#/{nxt[0]}" class="next"><span class="eyebrow">Next</span><span class="t">{titles.get(nxt[0], nxt[1])}</span></a>' if nxt else '<span></span>'
        pager += "</div>"
        sections.append(
            f'<section class="page" data-page="{pid}" data-title="{html.escape(titles[pid])}">'
            f'<div class="page-head"><div><h1>{html.escape(titles[pid])}</h1><p class="lede">{inline(lede)}</p></div></div>'
            f'<div class="doc">{body}</div>{pager}</section>')
        cards.append(f'<a class="card" href="#/{pid}">{svg_icon(icon)}<span class="t">{html.escape(titles[pid])}</span>'
                     f'<span class="d">{html.escape(lede[:110])}{"…" if len(lede) > 110 else ""}</span></a>')
    for group in GROUPS:
        nav.append(f'<span class="eyebrow nav-group">{group}</span>')
        for pid, ptitle, icon, g in PAGES:
            if g == group:
                nav.append(f'<a href="#/{pid}" data-page="{pid}">{svg_icon(icon)}{html.escape(titles[pid])}</a>')
    overview = (
        '<section class="page" data-page="overview" data-title="Overview">'
        '<div class="page-head"><div><h1>GraphRD documentation</h1>'
        '<p class="lede">An RDF database in one binary: SPARQL 1.1 and 1.2, rule-based inference, '
        'full-text, vector and geospatial search, SHACL, GraphQL, SQL — with a web UI inside.</p></div></div>'
        f'<div class="cards">{"".join(cards)}</div></section>')
    doc = f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>GraphRD docs</title>
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 64 64%22%3E%3Crect x=%221%22 y=%221%22 width=%2262%22 height=%2262%22 rx=%2214%22 fill=%22%230f172a%22/%3E%3Cpath d=%22M18 19L46 19L32 46Z%22 fill=%22none%22 stroke=%22%2394a3b8%22 stroke-width=%223%22 stroke-linejoin=%22round%22 opacity=%22.55%22/%3E%3Ccircle cx=%2218%22 cy=%2219%22 r=%228.5%22 fill=%22%233b82f6%22/%3E%3Ccircle cx=%2246%22 cy=%2219%22 r=%228.5%22 fill=%22%23f59e0b%22/%3E%3Ccircle cx=%2232%22 cy=%2246%22 r=%228.5%22 fill=%22%2314b8a6%22/%3E%3C/svg%3E">
<style>{CSS}</style>
</head>
<body>
<div class="shell">
  <header class="topbar">
    <button id="menu-toggle" class="topbar-btn" type="button" aria-label="Open the menu" aria-controls="sidebar" aria-expanded="false">
      <svg viewBox="0 0 20 20" aria-hidden="true"><path d="M3.5 6h13M3.5 10h13M3.5 14h13"/></svg>
    </button>
    <a class="topbar-mark" href="#/overview" aria-label="GraphRD docs">
      <svg class="mark" viewBox="0 0 32 32" aria-hidden="true"><path d="M9 9L23 9L16 23Z" fill="none" stroke="currentColor" stroke-width="1.6" opacity=".45"/><circle cx="9" cy="9" r="4.5" class="s"/><circle cx="23" cy="9" r="4.5" class="p"/><circle cx="16" cy="23" r="4.5" class="o"/></svg>
    </a>
    <div class="topbar-title"><b id="topbar-page">Overview</b></div>
  </header>
  <div class="scrim" id="scrim" hidden></div>
  <aside class="sidebar" id="sidebar">
    <div class="sidebar-head">
      <a class="brand" href="#/overview" aria-label="GraphRD docs">
        <svg class="mark" viewBox="0 0 32 32" aria-hidden="true"><path d="M9 9L23 9L16 23Z" fill="none" stroke="currentColor" stroke-width="1.6" opacity=".45"/><circle cx="9" cy="9" r="4.5" class="s"/><circle cx="23" cy="9" r="4.5" class="p"/><circle cx="16" cy="23" r="4.5" class="o"/></svg>
        <span>Graph<b>RD</b> <span class="doc-tag">docs</span></span>
      </a>
      <button id="menu-close" class="topbar-btn" type="button" aria-label="Close the menu">
        <svg viewBox="0 0 20 20" aria-hidden="true"><path d="M5 5l10 10M15 5L5 15"/></svg>
      </button>
    </div>
    <section class="search-card" aria-label="Filter pages">
      <input type="search" id="q" placeholder="Filter pages…" aria-label="Filter pages">
    </section>
    <nav id="nav" aria-label="Documentation">
      <a href="#/overview" data-page="overview"><svg viewBox="0 0 20 20" aria-hidden="true"><path d="M3 9l7-6 7 6v8H3z"/></svg>Overview</a>
      {"".join(nav)}
    </nav>
    <div class="sidebar-foot">
      <button id="theme-toggle" class="ghost" type="button">
        <svg viewBox="0 0 20 20" aria-hidden="true"><circle cx="10" cy="10" r="6"/><path d="M10 4a6 6 0 0 1 0 12z" class="fill"/></svg>
        <span>Dark</span>
      </button>
    </div>
  </aside>
  <main id="view" tabindex="-1">
{overview}
{"".join(sections)}
  </main>
</div>
<script>{JS}</script>
</body>
</html>
"""
    OUT.write_text(doc)
    write_llms(titles, ledes)
    print(f"wrote {OUT} ({len(doc)//1024} KB)")


if __name__ == "__main__":
    build()
