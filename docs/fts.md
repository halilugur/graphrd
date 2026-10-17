# Full-text search

GraphRD can index the words of every literal and find literals by a text query, inside SPARQL.

## Turning it on

Set `enableFtsIndex` when creating the repository, or later (existing literals are indexed then):

```bash
curl -X POST localhost:7200/rest/repositories -H 'Content-Type: application/json' \
  -d '{"id":"docs","params":{"enableFtsIndex":{"value":"true"}}}'
```

The index is kept up to date on every write, including bulk loads, and is stored in its own LSM
index next to the quad indexes. Turning it off drops it.

## Querying

The predicate `g:fts` (`g:` = `http://graphrd.org/fn#`, predefined) relates a literal to a query
string. With the literal unbound it searches the index; with it bound it checks that one literal.

```sparql
PREFIX g: <http://graphrd.org/fn#>
PREFIX ex: <http://example.org/>
SELECT ?doc ?title ?score WHERE {
  ?title g:fts "graph database" .
  ?doc ex:title ?title .
  BIND(g:ftsScore(?title, "graph database") AS ?score)
}
ORDER BY DESC(?score)
LIMIT 10
```

A query on a repository without the index fails with
`Full-text search is not enabled for this repository (set enableFtsIndex)`.

## Query syntax

| Query | Matches literals that contain |
|---|---|
| `graph database` | every word (in any order), in any inflection (`databases`, `kitaplar` → `kitabı`) |
| `"graph database"` | the exact phrase (exact words, not stemmed) |
| `data*` | a word starting with `data` (the prefix needs 3 or more characters) |
| `databse~` | a word within edit distance 1 (up to 5 letters) or 2 (longer); `databse~1` / `~2` set it |
| `graph OR network` | either word |
| `graph -relational` | `graph` and not `relational` |

Words are compared case-insensitively. A word is a run of letters and digits in any script (Unicode
alphanumerics); everything else separates words. There is no stop-word list.

### Stemming

Words match in any inflection: English words through the Porter stemmer (`running` → `run`,
`databases` → `databas`), Turkish words through a light stemmer that strips up to three noun suffixes
(case, possessive, plural, copula) and undoes final consonant softening (`kitaplarımızdan`,
`kitabı` → `kitap`). A literal tagged `@en…` is stemmed only as English, one tagged `@tr…` only as
Turkish, any other literal both ways. Quoted phrases match exact words.

### Typos

`word~` matches words within an edit distance (insertions, deletions, substitutions and swapped
neighbours each count 1). A query that matches nothing at all is run again with each of its words of
3 or more letters allowed one edit, then (if that finds nothing either) the full `word~` allowance,
so `relatonal databses` still finds "Relational databases". Typo-tolerant matches score lower than
exact ones. Two edits on a word of 6 to 8 letters match a large part of the vocabulary's candidates,
so an explicit `word~2` is slower than `word~1`.

## Relevance

`g:ftsScore(?literal, "query")` returns the BM25 score (k1 = 1.2, b = 0.75) of a literal that
matches, and is unbound for one that does not: rare words weigh more than common ones, and a match in
a short literal more than in a long one. Each literal counts as one document; document frequencies
and the average length come from the index.

## Highlighting

`g:ftsHighlight(?literal, "query" [, pre, post])` returns the literal's text (with its language tag)
with every word the query matches wrapped in `pre` and `post` (default `<b>` and `</b>`):

```sparql
PREFIX g: <http://graphrd.org/fn#>
SELECT ?title (g:ftsHighlight(?title, "graph") AS ?snippet) WHERE { ?title g:fts "graph" }
```

## Search endpoint

`GET /rest/repositories/{id}/search` runs a query and returns the matching statements (subject,
predicate, literal), best first, with highlights, plus facet counts: for each `facet` predicate, how
many of the matching subjects have each value.

| Parameter | Default | |
|---|---|---|
| `query` (or `q`) | | the full-text query |
| `limit`, `offset` | 10, 0 | the page of hits (`limit` at most 1000) |
| `facet` | | a predicate IRI; repeat for more facets (up to 100 values each, most frequent first) |
| `pre`, `post` | `<b>`, `</b>` | highlight markers |

```bash
curl -G localhost:7200/rest/repositories/docs/search --data-urlencode 'query=graph' \
  --data-urlencode 'facet=http://www.w3.org/1999/02/22-rdf-syntax-ns#type'
```

```json
{"query": "graph", "total": 2,
 "hits": [{"subject": "<http://example.org/a>", "predicate": "<http://example.org/title>",
           "literal": "\"Graph databases\"@en", "score": 0.89, "highlight": "<b>Graph</b> databases"}],
 "facets": {"http://www.w3.org/1999/02/22-rdf-syntax-ns#type": [{"value": "<http://example.org/Book>", "count": 2}]}}
```

## How it works

Each word of a literal becomes postings `[hash(key), literal id]` for the word itself, its English
and Turkish stems and every prefix of 3 or more characters (for `prefix*`); its character trigrams
padded with `$$` go into a vocabulary (trigram → word): a word within edit distance d shares all but
4·d of its trigrams, so a typo-tolerant word looks up the words sharing enough of them and takes
their literals as candidates. One more posting per literal records its length in words, for BM25. A query collects candidate literals from the postings and
verifies each against its actual text, so hash collisions never produce wrong results and phrases
are checked exactly. Stores whose index predates stems and trigrams are re-indexed once when opened
(200,000 literals: about 8 s).

Measured with `bench/fts.py` on 200,000 literals (4 to 13 words, a fifth Turkish), against the
previous index: loading
over HTTP 3.7 → 4.4 s (the stem postings), two-word and phrase queries within 5%, a typo that needs
the fallback (`databse`, 40,000 hits) about 0.2 s.
