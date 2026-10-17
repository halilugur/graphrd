<p align="center"><img src="docs/logo.svg" alt="" width="280"></p>

# GraphRD

GraphRD is an RDF database written in Rust with **zero third-party dependencies** in its default
build (HTTPS is an optional `tls` feature backed by rustls): HTTP server, JSON,
RDF parsers and serializers, storage, SPARQL engine and reasoner are all implemented on the
standard library. It speaks the **repository REST protocol** and has its own **management REST API**, so
existing repository clients work against it, and it ships its own web UI inside the binary.

**Documentation:** [docs/](docs/README.md) — [getting started](docs/getting-started.md),
[command line](docs/cli.md), [HTTP API](docs/http-api.md), [SPARQL](docs/sparql.md),
[inference](docs/inference.md), [administration](docs/admin.md) and more.

<details>
<summary><b>Türkçe özet</b></summary>

GraphRD, Rust ile yazılmış, varsayılan derlemesinde **hiç üçüncü parti bağımlılığı olmayan** bir
RDF veritabanıdır: HTTP sunucusu, JSON, RDF ayrıştırıcıları, depolama (LSM, WAL, MVCC), SPARQL
motoru ve çıkarım motoru standart kütüphane üzerinde yazıldı. Tek ikili dosya; web arayüzü
içinde gelir. Depo REST protokolünü ve SPARQL 1.1 Protocol'ü konuşur, mevcut istemciler çalışır.

- **Sorgu**: SPARQL 1.1/1.2 sorgu ve güncelleme, RDF-star, federasyon (paralel `SERVICE`);
  tüm W3C RDF ve SPARQL test paketleri geçer. GraphQL ve PostgreSQL protokolüyle SQL.
- **Çıkarım**: RDFS, OWL 2 RL ve yaygın kural setleri (ileri zincirleme, artımlı geri alma),
  sorgu anında çıkarım (`infer=at-query`), özel `.pie` kuralları.
- **Arama**: BM25 tam metin arama, GeoSPARQL ve uzamsal indeks, HNSW vektör araması,
  OpenSearch/Elasticsearch bağlayıcıları.
- **Doğal dil**: `--llm-url`, `--llm-key`, `--llm-model` (ya da yalnız `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`) ile sorular düz cümleyle sorulur; dil modeli
  (Claude ya da OpenAI uyumlu herhangi bir sunucu) SPARQL yazar, GraphRD yalnız okuma sorgularını kabul eder.
- **Doğrulama ve geçmiş**: SHACL Core + SHACL-SPARQL, zamanda yolculuk sorguları, artımlı yedek
  ve zamana dönük geri yükleme.
- **İşletim**: Raft kümesi (log sıkıştırma, okuma kopyaları), Prometheus metrikleri, kullanıcılar
  ve roller, API anahtarları, OpenID Connect, LDAP, denetim kaydı, isteğe bağlı HTTPS.

Hızlı başlangıç: `cargo build --release`, sonra `target/release/graphrd serve` ve
<http://127.0.0.1:7200>. Belgeler [docs/](docs/README.md) altında (İngilizce); katkı kuralları
katkı rehberinde.

</details>

## Features

- **SPARQL 1.1 Query and Update**: SELECT, ASK, CONSTRUCT, DESCRIBE; OPTIONAL, UNION, MINUS,
  FILTER, BIND, VALUES, sub-queries, property paths (`/ | ^ * + ? !`), GROUP BY / HAVING with
  COUNT, SUM, AVG, MIN, MAX, SAMPLE, GROUP_CONCAT, and the built-in function library (string,
  numeric, date/time, hash, regex). All update operations, including DELETE/INSERT WHERE, WITH,
  USING, CLEAR, DROP, ADD, MOVE and COPY. Federated queries: `SERVICE <endpoint> { … }` (and
  `SERVICE ?var`, `SERVICE SILENT`) send the block to any SPARQL 1.1 endpoint and join its results.
- **Inference** by forward chaining with the rulesets `rdf`, `rdfs`, `rdfsplus`, `owl-horst`, `owl2-rl`,
  their `-optimized` variants, and `owl-max` / `owl2-*` aliases. Explicit and inferred statements are
  kept apart: queries take `infer=false`, and the `graph:explicit` / `graph:implicit` pseudo-graphs
  are supported. Deleting a fact retracts what was derived from it. `infer=at-query` infers at
  query time by backward chaining instead, with nothing stored.
- **Custom rulesets** in the `.pie` rule format (`Prefices`, `Axioms`, `Rules` with `Id:`
  blocks and `[Constraint a != b]`): set the repository's ruleset to the path of a `.pie` file.
- **RDF 1.2 and SPARQL 1.2**: triple terms `<<( s p o )>>`, reified triples `<< s p o ~ :id >>`
  (a reifier with `rdf:reifies`), annotations `{| … |}` with any number of reifiers, literals with a
  base direction (`"text"@ar--rtl`, `rdf:dirLangString`), `VERSION`, in every syntax (Turtle, TriG,
  N-Triples, N-Quads, RDF/XML with `rdf:parseType="Triple"`, `rdf:annotation` and `its:dir`) and in
  queries, templates and updates, with `TRIPLE`, `SUBJECT`, `PREDICATE`, `OBJECT`, `isTRIPLE`,
  `LANGDIR`, `hasLANG`, `hasLANGDIR` and `STRLANGDIR`. As in RDF 1.2, `<< s p o >>` is a reified
  triple (it denotes a reifier), not the triple itself as in the older RDF-star drafts; data
  stored with RDF-star subjects stays readable.
- **Full-text search**: set `enableFtsIndex` on a
  repository and query with `?literal g:fts "query"` (the prefixes `g:` = `http://graphrd.org/fn#`,
  `graph:` and `rank:` are predefined). The query language supports words (all
  must match, any inflection: English Porter and Turkish stemming), `"exact phrases"`, `prefix*`,
  `word~` (typo-tolerant, edit distance up to 2), `a OR b` and `-exclusions`; a query that finds
  nothing is retried typo-tolerant. `g:ftsScore(?literal, "query")` gives the BM25 score,
  `g:ftsHighlight(?literal, "query")` the text with the matching words marked, and
  `GET /rest/repositories/{id}/search?query=…&facet=<predicate>` hits, highlights and facet counts.
  Postings live in their own LSM index, are maintained on every write, and can be built over
  existing data.
- **Questions in plain language**: with an endpoint, key and model (`--llm-url`, `--llm-key`,
  `--llm-model`), or just `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` in the environment, a language model
  (Claude through the Anthropic Messages API, or any OpenAI-compatible server, local ones included)
  writes the SPARQL for a question from a summary of the repository's classes and properties;
  GraphRD accepts only read queries without `SERVICE`, retries once on a parse error, and can run
  the query: `POST /rest/repositories/{id}/nl-query` ([docs/nl-query.md](docs/nl-query.md)).
- **Search connectors**: entities of chosen classes kept in an OpenSearch or Elasticsearch index
  as the data changes, and searched from SPARQL with `?e <http://graphrd.org/connector/NAME> "query"`.
  See [docs/connectors.md](docs/connectors.md).
- **Vector search**: embeddings stored as `g:vector` literals are indexed in an HNSW graph;
  `?v g:knn ?q` finds the nearest (p95 1.5 ms over a million vectors), `g:similarity` scores them,
  and both combine with full-text search for hybrid ranking. See [docs/vectors.md](docs/vectors.md).
- **GeoSPARQL 1.1 functions** (`geof:` = `http://www.opengis.net/def/function/geosparql/`) over
  `geo:wktLiteral` and `geo:geoJSONLiteral` values (CRS84, EPSG:4326/4258/4269, Web Mercator and
  WGS 84 / UTM zones): the Simple Features (`sfEquals`,
  `sfDisjoint`, `sfIntersects`, `sfTouches`, `sfCrosses`, `sfWithin`, `sfContains`, `sfOverlaps`),
  Egenhofer (`eh*`) and RCC8 (`rcc8*`) relation families, `relate` with a DE-9IM pattern,
  `intersection`, `union`, `difference`, `symDifference`, `envelope`, `convexHull`, `centroid`,
  `boundary`, `dimension`, `coordinateDimension`, `spatialDimension`, `isEmpty`, `isSimple`,
  `asWKT`, `asGeoJSON`, `getSRID`, `minX`/`minY`/`maxX`/`maxY`, `numGeometries`, and `distance`,
  `area`, `length`, `perimeter` in metres (or another `uom:` unit) on the sphere. The geometry
  engine is GraphRD's own and is checked against GEOS on random geometries (DE-9IM matrices and
  set operations agree on 10,000 pairs). Topology is planar in coordinate space, as with most
  GeoSPARQL implementations. A spatial index over every geometry literal answers relation and
  `distance < r` filters against a geometry without trying every geometry (100,000 geometries:
  0.4-3 ms instead of ~500 ms; see [docs/geosparql.md](docs/geosparql.md#spatial-index)).
- **Formats**: Turtle(-star), TriG(-star), N-Triples(-star), N-Quads(-star), RDF/XML (including DTD
  entities) and JSON-LD for import and export, and Notation3 (N3) for import, with formulas as
  blank-node named graphs; SPARQL results as JSON, XML, CSV and TSV.
- **Snapshot isolation (MVCC)**: every query runs on a consistent snapshot of the repository and
  never waits for a writer; writers never wait for readers. A snapshot costs a few reference-count
  increments: memtables are frozen into immutable sorted levels and run files are shared.
- **Time travel**: with the `historyRetention` repository setting (e.g. `7d`), a query takes
  `as-of=<dateTime>` and runs on the repository as it was then (`graphrd query --as-of` offline);
  see [docs/sparql.md](docs/sparql.md#time-travel).
- **Disk-based storage**: dictionary-encoded quads in four LSM indexes (SPOG, POSG, OSPG, GSPO).
  Writes go to a write-ahead log and an in-memory memtable; checkpoints fold them into immutable,
  block-indexed run files that are merged in a size-tiered way. Terms live in an on-disk dictionary
  with a hash index. Memory use is bounded by the memtable and caches, not by the data size, and
  inferred statements are persisted, so restarts are instant.
- **Web UI** at `/`: repositories and rulesets, a SPARQL editor with highlighting and saved queries,
  file/text/server-side import, a resource and class-hierarchy explorer, named graphs, namespaces,
  light and dark themes, and a visual graph explorer of its own (canvas rendering, Barnes–Hut
  force layout plus radial and hierarchy layouts, neighbourhood highlighting, search, minimap,
  context menu and PNG export; thousands of nodes) for a resource, a named graph or the schema.
- **SHACL validation** (`isShacl` repository setting): shapes are loaded into the
  shapes graph (`urn:graphrd:shapes`). Every write is validated for the nodes it
  touches (and nodes linking to them); a violating write is rolled back and answered with a 400 and
  the validation report. All of SHACL Core (complex paths, `and`/`or`/`not`/`xone`, `node`,
  qualified value shapes, property pairs, `closed`, `languageIn`, `uniqueLang`, severities) and
  SHACL-SPARQL (`sh:sparql`, SPARQL-based constraint components); the W3C SHACL test suite passes
  120/120. `graphrd validate --shapes S DATA...` validates files offline.
  `GET /rest/repositories/{id}/shacl/validate` reports on the whole
  repository (JSON, or Turtle `sh:ValidationReport`).
- **Backup and restore**: `GET /rest/recovery/backup` returns a tar archive with every
  repository's configuration, namespaces and statements; `POST /rest/recovery/restore` brings
  them back (also from the Repositories page).
- **RDF Rank**: `POST /rest/rank/compute` runs PageRank over the links between resources;
  query scores with `?node rank:score ?score` (or `score3`/`score5` for rounded
  values). The status turns OUTDATED when data changes.
- **Autocomplete**: `/rest/autocomplete/query?q=` suggests IRIs by local-name prefix (used by the
  Explore search box), from an index rebuilt only after data changes.
- **GraphQL without writing a schema**: `POST /rest/repositories/{id}/graphql` with
  `{"query": "...", "variables": {...}}`. The schema is derived from the data: every class is a
  query field, every predicate a field named by its local name, links nest. Root fields take
  `id`, `where`, `limit`, `offset`, `orderBy` and `desc`; `?sdl` returns the derived schema.
  Mutations `createT` / `updateT` / `deleteT` write through the same schema, one transaction per
  request (SHACL-checked); the web UI has a GraphQL page.
- **SQL over the PostgreSQL protocol** (`--pg-port 5432`): connect with psql, psycopg, JDBC or BI
  tools; the database name is the repository. Tables are classes, columns are `id` plus
  predicates: `SELECT name, age FROM Person WHERE age > 30 ORDER BY age DESC LIMIT 10`,
  `COUNT(*)`, `LIKE`, `IS NULL`, `SHOW TABLES`, `information_schema`, and
  `SELECT * FROM sparql('…')` for any SPARQL query, `[LEFT] JOIN … ON a = b`, `CREATE VIEW`
  (kept in the repository), and `INSERT` / `UPDATE` / `DELETE` (one transaction each,
  SHACL-checked, replicated in a cluster). Simple and extended (prepared) protocol, text and
  binary parameters; same users and permissions as HTTP.
- **HTTPS** (build with `--features tls`, which adds rustls): serve HTTPS with
  `--tls-cert cert.pem --tls-key key.pem`, and fetch `https://` sources (`SSL_CERT_FILE` adds
  certificate authorities to trust, such as a company CA). The library also has an ACME client
  (`graphrd::acme`: certificates from Let's Encrypt with HTTP-01 challenges) and an HTTPS server
  that picks the certificate by server name. The default build stays free of third-party
  crates and still fetches `http://` URLs.
- **Loading from the web**: SPARQL `LOAD <url> [INTO GRAPH <g>]` and import-from-URL download RDF
  (content type, then file extension decide the format; redirects are followed).
- **Security** (optional, `--auth admin:password`): multiple users with roles
  (`ROLE_ADMIN`, `ROLE_REPO_MANAGER`, `ROLE_USER`) and per-repository `READ_REPO_<id>` /
  `WRITE_REPO_<id>` grants, managed through `/rest/security/users` or the Users page; HTTP Basic or
  `Authorization: Bearer <token>` from `/rest/login`. Passwords are stored salted and iterated. Also API keys for
  programs, OpenID Connect access tokens (`--oidc-issuer`, tested with Keycloak), LDAP sign-in
  (`--ldap-url`, tested with OpenLDAP) and a JSONL audit log (`--audit-log`); see
  [docs/security.md](docs/security.md).

## Quick start

```bash
cargo build --release
./target/release/graphrd serve
```

Open <http://127.0.0.1:7200/>, create a repository, import some RDF and run queries.

```
graphrd serve [OPTIONS]

  --port <n>          HTTP port (default 7200)
  --host <addr>       Bind address (default 127.0.0.1)
  --data <dir>        Data directory (default ./graphrd-data)
  --ui-dir <dir>      Serve another web UI build at / (GraphRD's UI moves to /ui/)
  --auth <user:pass>  Require this user; also read from GRAPHRD_AUTH
```

The server binds to localhost by default. Use `--auth` before exposing it on a network; it prints a
warning when bound elsewhere without authentication.

In a container (the image runs as an unprivileged user, with the data in a volume):

```bash
docker build -t graphrd .
docker run -d -p 7200:7200 -v graphrd-data:/data -e GRAPHRD_AUTH=admin:change-me graphrd
```

### Using it from the command line

```bash
# create a repository with RDFS-Plus inference
curl -X POST localhost:7200/rest/repositories -H 'Content-Type: application/json' \
  -d '{"id":"zoo","params":{"ruleset":{"value":"rdfsplus-optimized"}}}'

# load Turtle
curl -X POST localhost:7200/repositories/zoo/statements -H 'Content-Type: text/turtle' \
  --data-binary @animals.ttl

# query (prefixes declared in loaded Turtle are registered automatically)
curl -G localhost:7200/repositories/zoo -H 'Accept: text/csv' \
  --data-urlencode 'query=SELECT ?s ?type WHERE { ?s a ?type }'

# update
curl -X POST localhost:7200/repositories/zoo/statements \
  --data-urlencode 'update=PREFIX ex: <http://example.org/> INSERT DATA { ex:a ex:b ex:c }'
```

Offline tools work on a data directory without a server (it is locked while they run). `load`
adds statements in bulk: files are parsed on all cores and each index gets one sorted run, without
the write-ahead log; the new manifest commits each batch at once, so a crash leaves the data as it
was (about 1.5 million lines/s on 4 cores; repositories that validate SHACL load through the
checked path):

```bash
graphrd load --data db --repo zoo animals.ttl more.nq      # create the repository if needed
graphrd load --data db --lenient wikidata-dump.nt            # skip and count unparsable statements
graphrd query --data db --repo zoo --format csv --explain true 'SELECT ...'   # plan on stderr
graphrd update --data db --repo zoo --base http://example.org/ 'INSERT DATA { <a> <b> <c> }'
graphrd dump --data db --repo zoo --graph default --output zoo.ttl
graphrd convert --from trig --to nt --from-graph http://example.org/g data.trig
graphrd backup --data db /backups/db-2026-10-14         # a data directory of its own
graphrd optimize --data db                              # compact every index
graphrd serve --data db --read-only --cors --timeout 30 # query-only public endpoint
graphrd serve --memory                                  # nothing on disk; data ends with the process
```

## HTTP API

**Repository protocol** (`/repositories`): repository list; query and update endpoint
(`query`, `update`, `infer`, `timeout`, `default-graph-uri`, `named-graph-uri`, plus
`limit`/`offset` paging); `/statements` GET/POST/PUT/DELETE with `subj`/`pred`/`obj`/`context` filters;
`/size`, `/contexts`, `/namespaces`; SPARQL Graph Store protocol at `/rdf-graphs/service`;
`PUT /repositories/{id}` to create a repository; `/protocol`. `POST /statements?lenient=true` skips
statements that do not parse and answers `{"added", "skipped", "errors"}` (imports: `"lenient": true`).

**SPARQL 1.1 Protocol** at the standard names: `/sparql` (also `/query`, `/update`) answers the
default repository (`serve --default-repo`, else the only repository, else `default`), and
`/repositories/{id}/sparql` any repository; `using-graph-uri`/`using-named-graph-uri` for updates.
A GET without a query returns the SPARQL 1.1 Service Description.

**Monitoring**: `GET /metrics` in the Prometheus text format (query/update/WAL fsync latency
histograms, responses, block cache hits, statements per repository, cluster state);
`serve --slow-query <ms>` logs slow queries and updates (`/rest/monitor/slow-queries`, Monitoring page).

**Management REST API** (`/rest`): repository management (`/rest/repositories`, `default-config`, `size`,
`restart`), import (`import/upload/text|file|url`, `import/server`), saved queries
(`/rest/sparql/saved-queries`), locations, security and login, version and monitoring
endpoints.

## Architecture

```
src/
  main.rs        CLI
  http.rs        HTTP/1.1 server (thread per connection, keep-alive, chunked, multipart)
  json.rs        JSON parser/serializer
  geo.rs         GeoSPARQL geometry: WKT/GeoJSON, DE-9IM, overlay, metrics
  rdf/           term model, Turtle/TriG/N-Triples/N-Quads parser, RDF/XML (incl. XML parser),
                 JSON-LD reader, serializers
  store.rs       indexed quad store, snapshot + WAL persistence, inferred pseudo-graph
  reason.rs      semi-naive forward-chaining rule engine and rulesets
  sparql/        lexer/parser to algebra, evaluator, functions, regex engine, hashes, result formats
  server.rs      repositories, repository protocol, management REST API, auth, embedded UI
ui/              the web UI (compiled into the binary with include_str!)
```

## Performance

On an M2 Pro
with inference and full-text search on: 3 million triples load at 81k triples/s; point lookups,
joins, property paths and full-text phrase queries answer in 0.1–0.3 ms; a durable single write
(fsynced before it is acknowledged) takes 1–3 ms, and concurrent writers share fsyncs through
group commit (850 commits/s with 8 writers); three servers load 285k triples/s combined; a
three-node Raft cluster serves 4,942 mixed QPS across its nodes.

## Security

- Users and roles with per-repository read and write grants; passwords salted and iterated.
- A user is locked for 15 minutes after 10 wrong passwords; login tokens expire after 12 hours.
- The HTTP server takes `set_limits` (largest request body, most connections at once) for
  embedding in a service that faces the internet.
- XML input: entity expansion is capped (no "billion laughs"), nesting depth too.
- The UI never triggers the browser's own login prompt; it shows its sign-in form.

## Cluster

`--cluster`, `--node-id` and `--cluster-secret` make a Raft cluster: every server holds all the
data, followers forward writes to the leader, and a change counts once a majority has it.
Followers answer reads from their own copy, which may lag a moment behind; a read with the
header `X-GraphRD-Consistency: leader` is answered by the leader and sees every acknowledged
write.

## Durability

Every transaction is appended to a write-ahead log with a commit marker and fsynced before the
client gets its answer, so an acknowledged write survives a crash of the process or of the
machine. A transaction cut short by a crash is dropped whole on restart. `--sync off` skips the
fsync: writes then take 0.3 ms and survive a crash of the process, but not a power loss.

A SPARQL update request is applied completely or not at all: if one of its operations fails,
the ones before it are undone. Embedded use can also commit without waiting for the disk
(`StoreLock::write_relaxed`, for records such as logs) and wait for everything with
`StoreLock::sync`.

## Limitations

- Search connectors cover OpenSearch and Elasticsearch, not Solr.
- GeoSPARQL: no rewriting of the relation properties (`?a geo:sfWithin ?b` as a triple pattern).
- Queries have no default time limit unless the server is started with `--timeout`; pass `timeout`
  per request.

## Development

```bash
cargo test --release
```

`python3 tests/docs/check_examples.py` runs the curl examples of `docs/` against a fresh server.

Unit tests cover the parsers, serializers, regex and hash code, the store and its persistence,
inference and retraction, and SPARQL evaluation. HTTP integration tests drive the server end to end,
including the authentication flow.

### W3C conformance

`tests/w3c/run.py` runs the official W3C test suites ([w3c/rdf-tests](https://github.com/w3c/rdf-tests))
against GraphRD:

| Suite | Result |
|---|---|
| N-Triples, N-Quads, Turtle, TriG, RDF/XML (RDF 1.1) | 993 / 993 |
| N-Triples, N-Quads, Turtle, TriG, RDF/XML (RDF 1.2) | 1,334 / 1,334 |
| SPARQL 1.0 syntax and query evaluation | 487 / 487 |
| SPARQL 1.1 Query | 353 / 353 |
| SPARQL 1.1 Update | 157 / 157 |
| SPARQL 1.1 result formats (CSV, TSV) | 10 / 10 |
| SPARQL 1.2 (syntax, evaluation, triple terms, base direction) | 271 / 271 |
| SPARQL 1.1 Federated Query (SERVICE, local stand-in endpoints) | 7 / 7 |
| SPARQL 1.1 Entailment Regimes (RDF, RDFS, D, OWL 2 RDF-Based via OWL 2 RL) | 46 / 48 (22 OWL Direct/RIF-only skipped) |
| RDF Dataset Canonicalization (RDFC-1.0, [w3c/rdf-canon](https://github.com/w3c/rdf-canon)) | 86 / 86 |
| JSON-LD 1.1 to RDF ([w3c/json-ld-api](https://github.com/w3c/json-ld-api)) | 456 / 456 |
| Notation3 grammar and extended grammar ([w3c/N3](https://github.com/w3c/N3)) | 1,083 / 1,085 |

```bash
git clone --depth 1 https://github.com/w3c/rdf-tests /tmp/rdf-tests
python3 tests/w3c/run.py /tmp/rdf-tests
```

The other suites are cloned next to rdf-tests and run by name (`rdfc10`, `jsonld-toRdf`, `n3`). The
two N3 cases not passed are a 1,080-level nesting of formulas, beyond GraphRD's nesting guard
against hostile input, and a test whose expected file holds a cwm-specific absolute `file:` IRI.

The suites run with `unionDefaultGraph` = false (a separate default graph, as the tests assume).
The SPARQL 1.1 Protocol and Service Description suites (37 tests) run with
`uv run --with rdflib python3 tests/w3c/protocol.py /tmp/rdf-tests`.
Run them by name: `entailment`, `service`. Not covered: OWL 2 Direct Semantics and RIF entailment
(they need a description logic reasoner). RDF 1.2
and SPARQL 1.2 are W3C drafts; the suites are those of the drafts at the time of testing.

The production suite (`tests/prod/run.sh`, about 2 minutes; `FULL=1` for long runs) drives real
server processes:

| Test | What it checks |
|---|---|
| `differential.py` | 110+ queries and 18 updates compared with rdflib (spec-checked where rdflib is wrong) |
| `regex_diff.py` | thousands of random regex patterns compared with Python's `re`; pathological patterns stay linear |
| `robustness.py` | protocol abuse, slow clients, deeply nested queries and Turtle, 2,300 fuzzed inputs, PostgreSQL port garbage |
| `crash.py` | SIGKILL at random moments under concurrent writes and bulk loads: no acknowledged write lost, no transaction half-applied |
| `geo_diff.py` | GeoSPARQL geometry vs GEOS: DE-9IM matrices and set operations on random geometries |
| `soak.py` | concurrent bank transfers (no lost updates), atomic pairs seen by readers, latency, memory growth, restart |

## Client libraries

The Java client and its Spring Data module live in their own repositories:
[graphrd-client](https://github.com/halilugur/graphrd-client) and
[graphrd-spring-data](https://github.com/halilugur/graphrd-spring-data).
