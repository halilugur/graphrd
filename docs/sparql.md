# SPARQL

GraphRD implements SPARQL 1.1 Query and Update completely, SPARQL 1.2 (the W3C draft), and a few
extensions. Requests reach it through the endpoints in [http-api.md](http-api.md) or the
`graphrd query` / `graphrd update` tools ([cli.md](cli.md)).

## Query language

- Forms: `SELECT` (with `DISTINCT`, `REDUCED`, expressions `(expr AS ?v)`), `ASK`, `CONSTRUCT`
  (including `CONSTRUCT WHERE`), `DESCRIBE`.
- Patterns: basic graph patterns, `OPTIONAL`, `UNION`, `MINUS`, `FILTER`, `BIND`, `VALUES` (inline and
  trailing), `GRAPH`, sub-queries, `EXISTS` / `NOT EXISTS`, `SERVICE`.
- Property paths: `/`, `|`, `^`, `*`, `+`, `?`, `!(…)`, grouping.
- Aggregates: `COUNT` (with `DISTINCT` and `*`), `SUM`, `AVG`, `MIN`, `MAX`, `SAMPLE`,
  `GROUP_CONCAT` (with `SEPARATOR`); `GROUP BY` on variables or expressions, `HAVING`.
- Solution modifiers: `ORDER BY` (`ASC`/`DESC`, expressions), `LIMIT`, `OFFSET`.
- Datasets: `FROM`, `FROM NAMED`, or the protocol's `default-graph-uri` / `named-graph-uri`.
- Functions: the whole SPARQL 1.1 library — terms (`STR`, `LANG`, `DATATYPE`, `IRI`, `BNODE`,
  `STRDT`, `STRLANG`, `isIRI`, `isBlank`, `isLiteral`, `isNumeric`, `sameTerm`, `LANGMATCHES`,
  `BOUND`, `IF`, `COALESCE`, `IN`), strings (`STRLEN`, `SUBSTR`, `UCASE`, `LCASE`, `STRSTARTS`,
  `STRENDS`, `CONTAINS`, `STRBEFORE`, `STRAFTER`, `ENCODE_FOR_URI`, `CONCAT`, `REGEX`, `REPLACE`),
  numbers (`ABS`, `ROUND`, `CEIL`, `FLOOR`, `RAND`), dates (`NOW`, `YEAR` … `SECONDS`, `TIMEZONE`,
  `TZ`), hashes (`MD5`, `SHA1`, `SHA256`, `SHA384`, `SHA512`), `UUID`, `STRUUID`, and XSD casts
  (`xsd:integer(…)` and the other numeric, boolean, string and date types).
- Regular expressions are GraphRD's own engine (a Pike VM): matching time is linear in the input,
  so no pattern can stall the server. Flags `i`, `s`, `m`, `x` and `q` are supported.

### SPARQL 1.2 and RDF 1.2

Triple terms `<<( s p o )>>`, reified triples `<< s p o >>` and `<< s p o ~ :r >>`, annotations
`s p o {| :source :x |}`, literals with a base direction (`"…"@ar--rtl`), `VERSION`, and the
functions `TRIPLE`, `SUBJECT`, `PREDICATE`, `OBJECT`, `isTRIPLE`, `LANGDIR`, `hasLANG`,
`hasLANGDIR`, `STRLANGDIR`. As in RDF 1.2, `<< s p o >>` denotes a reifier of the triple (with
`rdf:reifies`), not the triple itself as in the older RDF-star drafts.

```sparql
PREFIX ex: <http://example.org/>
SELECT ?who ?src WHERE {
  << ?who ex:age ?age >> ex:source ?src .
}
```

## Update language

`INSERT DATA`, `DELETE DATA`, `DELETE WHERE`, `DELETE … INSERT … WHERE` with `WITH` and `USING` /
`USING NAMED`, `LOAD <url> [INTO GRAPH <g>]` (HTTP; HTTPS in builds with `tls`), `CLEAR`, `DROP`,
`CREATE`, `ADD`, `MOVE`, `COPY` (each with `SILENT`, `DEFAULT`, `NAMED`, `ALL`). Several operations
separated by `;` form one request, applied completely or not at all: if one fails, the earlier ones
are undone.

## Inference in queries

Repositories with a ruleset store inferred statements next to explicit ones
([inference.md](inference.md)). Queries see both unless `infer=false` is passed; `infer=at-query`
infers at query time instead ([inference.md](inference.md#query-time-inference)). Two pseudo-graphs
select one kind in a query:

```sparql
PREFIX graph: <http://graphrd.org/graph#>
SELECT ?s ?p ?o WHERE { GRAPH graph:implicit { ?s ?p ?o } }    # inferred only
SELECT ?s ?p ?o WHERE { GRAPH graph:explicit { ?s ?p ?o } }    # stated only
```

## The default graph

By default (`unionDefaultGraph` = true) the default graph of a query is the union of every graph,
With `unionDefaultGraph` = false it is only the statements without a graph, as the
SPARQL specification describes and the W3C test suites expect. `FROM` and `default-graph-uri`
override either.

## Federated queries

`SERVICE <endpoint> { … }` sends the block to any SPARQL 1.1 endpoint and joins its results;
`SERVICE ?var` takes the endpoint from a binding made earlier in the query, and `SERVICE SILENT`
turns a failing endpoint into an empty match. Each distinct (endpoint, query) pair is called once
per query.

- **Parallel calls**: when a query starts, every SERVICE block with a fixed endpoint is sent at
  once, each on its own thread, so a query over three endpoints waits for the slowest one, not for
  the three in turn. Blocks with a variable endpoint are called when evaluation reaches them.
- **Answer cache**: `serve --service-cache 60` reuses an endpoint's answer to the same block for
  60 seconds across queries (off by default).
- **Failures**: `serve --service-timeout 10` bounds each call (within the query's own `timeout`);
  `serve --service-retries 2` tries again, with a short growing pause, when a call cannot connect
  or gets a 5xx answer (a 4xx is final). After that the query fails, or with `SERVICE SILENT`
  the block matches once with no bindings.
- The W3C federated query tests pass (7 of 7) with local endpoints standing in for the remote ones.

```sparql
SELECT ?item ?label WHERE {
  ?item a <http://example.org/Thing> .
  SERVICE <https://query.wikidata.org/sparql> { ?item <http://www.w3.org/2000/01/rdf-schema#label> ?label }
}
```

## Extensions

| Extension | Meaning |
|---|---|
| `?lit g:fts "query"` | full-text match ([fts.md](fts.md)); `g:` = `http://graphrd.org/fn#` |
| `g:ftsScore(?lit, "query")` | relevance of a full-text match |
| `?v g:knn ?q`, `g:knnN`, `g:similarity(?a, ?b)` | nearest vectors and cosine similarity ([vectors.md](vectors.md)) |
| `?node rank:score ?s` (`score3`, `score5`) | RDF Rank ([inference.md](inference.md#rdf-rank)); `rank:` = `http://graphrd.org/rank#` |
| `geof:…` | GeoSPARQL functions ([geosparql.md](geosparql.md)) |
| `limit`, `offset` request parameters | page a result without changing the query |

The well-known RDF, RDFS, OWL, XSD, function and GeoSPARQL prefixes, and GraphRD's own
(`g`, `graph`, `rank`), are predefined in new repositories; prefixes from uploaded Turtle and TriG are added.

## Time limits

`timeout=<seconds>` on a request (or `serve --timeout` for all requests without one) stops a
query that runs longer with an error. There is no limit otherwise.

## Time travel

A repository with the `historyRetention` setting keeps the history of its changes, and a query
can then run on the repository as it was at an earlier moment: pass `as-of` with an
`xsd:dateTime` (UTC when it has no timezone), an `xsd:date` (its midnight) or Unix milliseconds.

```bash
# keep the history for 7 days (also: 3600 seconds, 30m, 12h, unlimited; empty = off)
curl -X PUT localhost:7200/rest/repositories/zoo -H 'Content-Type: application/json' \
  -d '{"params": {"historyRetention": {"value": "7d"}}}'
curl -X POST localhost:7200/repositories/zoo/statements -H 'Content-Type: application/sparql-update' \
  --data 'INSERT DATA { <http://example.org/rex> <http://example.org/age> 3 }'
# the window that can be reached: {"enabled": true, "start": <Unix ms>, "retentionMs": …}
curl localhost:7200/rest/repositories/zoo/history
# as it was at a moment: here the end of the current second, in Unix milliseconds; or e.g.
# as-of=2026-10-16T09:30:00Z (a time before the history starts is an error naming its start)
curl localhost:7200/repositories/zoo --data-urlencode 'query=SELECT (COUNT(*) AS ?n) { ?s ?p ?o }' \
  --data-urlencode "as-of=$(date +%s)999"
```

`graphrd query --as-of <time>` does the same offline. The result is exact for every moment from
the start of the kept history on: the history is complete from when `historyRetention` was set
(and, after pruning, from the oldest moment the retention still covers). Each transaction is kept
with its commit time, inferences included as they followed from the statements then (with the
repository's current ruleset), and bulk loads count as one transaction.

The query runs on the current snapshot with the transactions after the requested moment undone in
memory, so its cost grows with the number of changes made since then, not with the size of the
repository. The history is `history.log` in the repository's storage directory; it costs a second
copy of every change written, so it is off by default. Turning it off deletes the file. A restored
backup starts a new history.

## Explaining a query

`graphrd query --explain true` prints the evaluation plan: the order chosen for each basic graph
pattern, joins, filters, and the time taken.

```bash
graphrd query --data db --repo zoo --explain true --format csv \
  'SELECT ?s WHERE { ?s a <http://example.org/Animal> ; <http://example.org/name> ?n }'
```

## How queries run

- Every query runs on a snapshot of the repository taken when it starts; it never waits for
  writers and never sees half of an update.
- Basic graph patterns are ordered by the number of bound positions, then smallest first by the
  indexes' estimate of how many statements match (read from the run files' block index, no data
  read); large joins switch from index lookups to hash joins, whose probe of 100,000 or more rows
  is split over the cores. `--explain` shows the order, the estimates and the join chosen.
- A `SELECT` with `LIMIT` and without `ORDER BY`, grouping or `DISTINCT` stops as soon as it has
  enough rows.
- A `FILTER` that reads one variable is evaluated once per distinct value of that variable.
- `ORDER BY … LIMIT k` ranks distinct values once and keeps only rows that can be in the first `k`.
- `COUNT` over one triple pattern (optionally with such a filter or grouped by its variables) counts
  directly from the index.

## Conformance

The W3C suites pass completely, including federated queries (7): SPARQL 1.0 (487), SPARQL 1.1 Query (353) and Update (157), result
formats (10), SPARQL 1.2 (271), and the SPARQL 1.1 Protocol and Service Description suites (37).
See the README's conformance section to run them.
