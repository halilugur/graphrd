# HTTP API

GraphRD answers four families of requests on one port (default 7200):

| Path | Protocol |
|---|---|
| `/repositories/...` | the repository REST protocol: queries, updates, statements, namespaces, Graph Store |
| `/sparql`, `/query`, `/update` | the SPARQL 1.1 Protocol at its standard names |
| `/rest/...` | GraphRD's management API: repositories, import, users, monitoring, backup |
| `/metrics` | Prometheus metrics |

Everything else serves the web UI. With `--auth`, every request except `/protocol`, `/rest/login`,
`/rest/logout`, `/rest/info/version` and the UI files needs a user (see [security.md](security.md)).
Examples on this page assume a server on `localhost:7200` without authentication.

## Queries and updates

### Repository endpoint: `/repositories/{id}`

| Method | Body | Does |
|---|---|---|
| `GET ?query=…` | none | query |
| `POST` | `application/x-www-form-urlencoded` with `query=` or `update=` | query or update |
| `POST` | `application/sparql-query` | query (the body is the query) |
| `POST` | `application/sparql-update` | update |
| `GET` (no query) | none | the SPARQL Service Description of the repository |

Parameters (query string or form):

| Parameter | Meaning |
|---|---|
| `query`, `update` | the SPARQL text (exactly one per request) |
| `infer` | `false` to leave inferred statements out (default `true`); `at-query` to infer at query time ([inference.md](inference.md#query-time-inference)) |
| `timeout` | time limit in seconds (default: `serve --timeout`, else none) |
| `default-graph-uri`, `named-graph-uri` | the query's dataset (repeatable); overrides `FROM` / `FROM NAMED` |
| `using-graph-uri`, `using-named-graph-uri` | an update's dataset; an error together with `USING` or `WITH` |
| `baseURI` | base IRI for relative IRIs (default: the endpoint's URL) |
| `limit`, `offset` | page a SELECT result (GraphRD extension) |
| `as-of` | run the query on the repository as it was then: dateTime, date or Unix ms; needs `historyRetention` ([sparql.md](sparql.md#time-travel)) |

The result format follows `Accept`:

| Result | Media types (first is the default) |
|---|---|
| SELECT, ASK | `application/sparql-results+json`, `application/sparql-results+xml`, `text/csv`, `text/tab-separated-values`, `application/x-sparqlstar-results+json` |
| CONSTRUCT, DESCRIBE | `text/turtle`, `application/n-triples`, `application/n-quads`, `application/trig`, `application/rdf+xml`, `application/ld+json` |

```bash
curl -G localhost:7200/repositories/zoo -H 'Accept: application/sparql-results+json' \
  --data-urlencode 'query=SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 5'

curl -X POST localhost:7200/repositories/zoo -H 'Content-Type: application/sparql-update' \
  --data 'INSERT DATA { <http://example.org/a> <http://example.org/b> 1 }'
```

Status codes: `200` with results, `204` after an update, `400` for a syntax error or a protocol error
(both `query` and `update`, two queries), `405` for an update sent with GET, `415` for a body media
type the protocol does not define, `500` for an evaluation error. An update request is applied
completely or not at all.

### SPARQL 1.1 Protocol: `/sparql`

`/sparql`, `/query` and `/update` behave exactly like `/repositories/{id}` for the repository chosen
with `serve --default-repo` (else the only repository, else the one named `default`). Any repository
also answers at `/repositories/{id}/sparql`. Generic SPARQL clients work with these names. The W3C Protocol and Service
Description test suites pass (see [sparql.md](sparql.md#conformance)).

```bash
curl -X POST localhost:7200/sparql -H 'Content-Type: application/sparql-query' \
  -H 'Accept: text/csv' --data 'SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }'
```

## Statements: `/repositories/{id}/statements`

| Method | Does |
|---|---|
| `GET` | statements matching `subj`, `pred`, `obj`, `context` (each optional, N-Triples syntax: `<iri>`, `"literal"`, `_:b`); `infer=false` for explicit ones only |
| `POST` | add the RDF in the body (format from `Content-Type`); with `update=` in a form body, run an update |
| `PUT` | replace the default graph, or the graphs named by `context`, with the body |
| `DELETE` | remove statements matching `subj`, `pred`, `obj`, `context` |

`context` is repeatable; `null` names the default
graph. `baseURI` sets the base for relative IRIs in the body. `lenient=true` (a GraphRD extension)
skips statements that do not parse and answers `200` with
`{"added": n, "skipped": n, "errors": [...]}` instead of rejecting the whole body.

```bash
# load Turtle into a named graph
curl -X POST 'localhost:7200/repositories/zoo/statements?context=%3Chttp://example.org/g%3E' \
  -H 'Content-Type: text/turtle' --data-binary @animals.ttl

# everything about one subject, as N-Triples
curl -G localhost:7200/repositories/zoo/statements -H 'Accept: application/n-triples' \
  --data-urlencode 'subj=<http://example.org/rex>'

# delete a graph's statements
curl -X DELETE 'localhost:7200/repositories/zoo/statements?context=%3Chttp://example.org/g%3E'
```

## Other repository endpoints

| Endpoint | Does |
|---|---|
| `GET /repositories` | the repository list, as SPARQL results |
| `PUT /repositories/{id}` | create a repository from a repository configuration in Turtle |
| `DELETE /repositories/{id}` | delete a repository and its data |
| `GET /repositories/{id}/size` | number of statements (`context=` for given graphs) |
| `GET /repositories/{id}/contexts` | named graphs, as SPARQL results |
| `GET /repositories/{id}/namespaces` | prefixes, as SPARQL results; `DELETE` clears them |
| `GET\|PUT\|DELETE /repositories/{id}/namespaces/{prefix}` | one prefix (the body of `PUT` is the namespace IRI) |
| `/repositories/{id}/rdf-graphs/service?graph=IRI` (or `?default`) | SPARQL 1.1 Graph Store protocol: `GET`, `PUT`, `POST`, `DELETE` |
| `/repositories/{id}/rdf-graphs/{name}` | Graph Store for the graph `…/rdf-graphs/{name}` |
| `GET /protocol` | the repository protocol version (`12`) |

Prefixes declared in Turtle and TriG uploads are added to the repository's namespaces (existing
prefixes are not changed), so queries can use them without `PREFIX`.

## Management API: `/rest`

All bodies are JSON unless noted.

### Repositories

| Endpoint | Does |
|---|---|
| `GET /rest/repositories` | list: `id`, `title`, `type`, size, permissions |
| `POST /rest/repositories` | create: `{"id": "zoo", "title": "…", "params": {"ruleset": {"value": "rdfs"}}}` |
| `GET /rest/repositories/{id}` | configuration |
| `PUT /rest/repositories/{id}` | change the configuration (ruleset, FTS, SHACL, union default graph, history); the `params` given replace those of the same name, the others are kept |
| `DELETE /rest/repositories/{id}` | delete |
| `GET /rest/repositories/{id}/size` | `{"explicit", "inferred", "total"}` |
| `GET /rest/repositories/{id}/history` | the time-travel window: `{"enabled", "start", "retentionMs"}` |
| `GET /rest/repositories/default-config/{type}` | the parameters a new repository takes, with defaults |
| `GET /rest/repositories/ruleset/...`, `GET /rest/rulesets` | the built-in rulesets |
| `GET /rest/repositories/{id}/search?query=Q` | full-text search with highlights and facet counts ([fts.md](fts.md#search-endpoint)) |
| `GET /rest/repositories/{id}/shacl/validate` | SHACL report for the whole repository (JSON; Turtle with `?format=turtle` or `Accept: text/turtle`) |
| `GET\|PUT\|DELETE /rest/llm`, `POST /rest/llm/test`, `GET /rest/llm/status` | the language model's settings, for administrators (see [nl-query.md](nl-query.md#in-the-web-ui)) |
| `POST /rest/repositories/{id}/nl-query` | a question in plain language: the SPARQL a language model writes for it, and with `"run": true` its results (see [nl-query.md](nl-query.md)) |
| `GET\|POST /rest/repositories/{id}/graphql` | GraphQL (see [graphql.md](graphql.md)) |
| `GET\|PUT\|DELETE /rest/repositories/{id}/connectors[/{name}]` | search connectors (see [connectors.md](connectors.md)) |

Repository parameters that take effect:

| Parameter | Default | Meaning |
|---|---|---|
| `ruleset` | `rdfsplus-optimized` | inference ruleset or path of a `.pie` file ([inference.md](inference.md)) |
| `disableSameAs` | `true` | `false` turns on `owl:sameAs` reasoning in rulesets that have it |
| `enableFtsIndex` | `false` | full-text index ([fts.md](fts.md)) |
| `isShacl` | `false` | validate writes with SHACL ([shacl.md](shacl.md)) |
| `unionDefaultGraph` | `true` | the default graph of queries is the union of all graphs; `false` keeps it separate |
| `historyRetention` | empty | keep the change history for time travel this long (`3600`, `30m`, `12h`, `7d`, `unlimited`; [sparql.md](sparql.md#time-travel)) |
| `title` | empty | description |

Other parameters (`queryTimeout`, `entityIndexSize`, `baseURL`, …) are accepted for compatibility with
compatibility configurations and stored, but have no effect.

### Import

Imports run inside the request and are recorded with their status and message.

| Endpoint | Body |
|---|---|
| `POST /rest/repositories/{id}/import/upload/text` | `{"name", "data": "<RDF text>", "format": "text/turtle", "context", "baseURI", "replaceGraphs": [...], "lenient": true}` |
| `POST /rest/repositories/{id}/import/upload/file` | multipart: `file` and `importSettings` (JSON as above) |
| `POST /rest/repositories/{id}/import/upload/url` | settings with `"data": "<url>"`; the format comes from `format`, the response's content type, or the extension |
| `GET /rest/repositories/{id}/import/server` | files in `DATA/import/` on the server |
| `POST /rest/repositories/{id}/import/server` | `{"fileNames": [...], "importSettings": {...}}` |
| `GET /rest/repositories/{id}/import/upload` | recorded imports |
| `DELETE /rest/repositories/{id}/import/upload` | forget recorded imports (`?name=` for one) |

`context` is the target named graph (`default` or empty for the default graph); `replaceGraphs`
lists graphs to clear first.

### Queries, prefixes, search

| Endpoint | Does |
|---|---|
| `GET\|POST\|PUT\|DELETE /rest/sparql/saved-queries` | saved queries: `{"name", "body"}` |
| `POST /rest/sparql/add-known-prefixes` | the query in the body with `PREFIX` lines added for prefixes it uses but does not declare |
| `GET /rest/autocomplete/query?q=…` | IRIs whose local name starts with `q`; the repository in the `X-GraphRD-Repository` header or `?repository=` |
| `POST /rest/rank/compute` | compute RDF Rank (repository in `X-GraphRD-Repository` or `?repository=`) |
| `GET /rest/rank/status?repository=ID` | `EMPTY`, `COMPUTED` or `OUTDATED` |

### Security, monitoring, backup

| Endpoint | Does |
|---|---|
| `POST /rest/login` | `{"username", "password"}` → the user and a token, also in the `Authorization: Bearer …` response header |
| `POST /rest/logout` | end the token in `Authorization` |
| `GET /rest/security/authenticated-user` | who is signed in |
| `GET\|POST\|PATCH\|DELETE /rest/security/users/{name}` | users ([admin.md](admin.md)) |
| `GET /rest/monitor/slow-queries` | the slow query log |
| `GET /rest/cluster/status` | Raft state of this node ([cluster.md](cluster.md)) |
| `GET /rest/recovery/backup` | tar archive of every repository ([admin.md](admin.md#backup-and-restore)) |
| `POST /rest/recovery/restore` | restore such an archive (multipart `file` or the raw tar) |
| `GET /rest/info/version` | product name and version |
| `GET /metrics` | Prometheus metrics ([admin.md](admin.md#monitoring)) |

## Errors

Errors are plain text (repository endpoints) or `{"message": "…"}` (management API), with `400` for bad
input, `401` without a valid user, `403` when the user lacks the permission or the server is
read-only, `404` for an unknown repository, `409` when creating a repository that exists, and `500`
for failures of the server.
