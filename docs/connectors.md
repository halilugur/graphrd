# Search connectors

A connector keeps a repository's entities of chosen classes in an OpenSearch (or Elasticsearch)
index, so the search engine's relevance ranking, analyzers, fuzzy matching and aggregations work
on graph data, and SPARQL queries can start from a search.

## Creating a connector

```bash
curl -X PUT localhost:7200/rest/repositories/zoo/connectors/people -H 'Content-Type: application/json' -d '{
  "type": "opensearch",
  "url": "http://localhost:9200",
  "index": "zoo-people",
  "types": ["http://example.org/Person"],
  "fields": [
    {"name": "name", "property": "http://example.org/name"},
    {"name": "city", "property": "http://example.org/city"}
  ]
}'
```

| Key | Meaning |
|---|---|
| `type` | `opensearch` or `elasticsearch` (the same API) |
| `url` | the engine's base URL; `https://` needs a build with the `tls` feature |
| `index` | index name (lower case; default: the connector name) |
| `types` | classes whose instances are indexed; inferred types count |
| `fields` | document fields and the predicate each one takes its values from |

The index is created when it does not exist (fields as `text` with a `.raw` keyword subfield), and
every entity is sent once. A connector that cannot reach its engine is refused with `502`.
Connectors are saved with the repository (`connectors.json`) and start again with the server.

## What gets indexed

Each entity becomes a document whose `_id` is its IRI, with `id`, `types` and one value or a list
of values per field (literals as their text, resources as their IRI). The store records the
subjects every write touches; a thread per repository sends their documents with the `_bulk` API
every few hundred milliseconds, and deletes the documents of those that are no longer instances of
the connector's classes. Writes never wait for the engine: when it is unreachable, the subjects stay
queued and the error is shown in the connector's status until a later round succeeds.

## Searching from SPARQL

`?e <http://graphrd.org/connector/NAME> "query"` binds `?e` to the entities the engine finds for a
`query_string` query (its full syntax: fields, `AND`/`OR`, wildcards, fuzzy `~`, boosts), best first
(up to 1,000):

```sparql
PREFIX ex: <http://example.org/>
SELECT ?person ?name WHERE {
  ?person <http://graphrd.org/connector/people> "name:smi* AND city:ankara" .
  ?person ex:name ?name ; ex:worksFor ?company .
}
```

## Managing connectors

| Endpoint | Does |
|---|---|
| `GET /rest/repositories/{id}/connectors` | every connector with its status |
| `GET /rest/repositories/{id}/connectors/{name}` | definition and status: documents indexed and deleted, subjects pending, last sync, last error |
| `PUT /rest/repositories/{id}/connectors/{name}` | create or replace (a full sync follows) |
| `DELETE /rest/repositories/{id}/connectors/{name}` | remove; `?dropIndex=true` also deletes the index |

Creating and removing connectors needs the repository manager role.

## Testing

`tests/prod/opensearch.py` starts OpenSearch in Docker and checks indexing (including inferred
types), updates, inserts, deletes, searches joined with graph patterns, a server restart, an
unreachable engine and removal with the index.

## Limits

- In a cluster, connectors are set up on each node separately (they are not replicated).
- After a restart a connector sends every entity again, since changes made while the server was
  down were not recorded for it.
