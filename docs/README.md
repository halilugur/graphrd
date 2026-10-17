# GraphRD documentation

| Page | Contents |
|---|---|
| [Getting started](getting-started.md) | install (cargo, Docker), first repository, loading data, first query |
| [Command line](cli.md) | `serve` and its options; `load`, `dump`, `query`, `update`, `backup`, `optimize`, `convert`, benchmarks |
| [HTTP API](http-api.md) | repository protocol, SPARQL 1.1 Protocol (`/sparql`), statements, Graph Store, management API (`/rest`) |
| [SPARQL](sparql.md) | query and update coverage, SPARQL 1.2, `SERVICE`, inference in queries, extensions, time limits, plans |
| [Inference](inference.md) | rulesets, explicit and inferred statements, retraction, custom `.pie` rules, RDF Rank |
| [Full-text search](fts.md) | `enableFtsIndex`, `g:fts`, query syntax, `g:ftsScore` |
| [Search connectors](connectors.md) | OpenSearch / Elasticsearch indexes kept in step with the repository, searched from SPARQL |
| [Vector search](vectors.md) | `g:vector` literals, `g:knn`, `g:similarity`, hybrid with full-text search, HNSW index |
| [GeoSPARQL](geosparql.md) | geometry literals, function families, units, limits |
| [SHACL](shacl.md) | `isShacl`, shapes graph, supported constraints, reports |
| [GraphQL](graphql.md) | the schema derived from the data, arguments, `?sdl` |
| [AI queries — questions in plain language](nl-query.md) | SPARQL written by a language model (Anthropic or OpenAI-compatible), checked and run |
| [SQL](sql.md) | `--pg-port`, tables and columns, supported SQL, `sparql()` |
| [Formats](formats.md) | RDF formats, RDF 1.2 syntaxes, canonical N-Quads, result formats, content negotiation |
| [Administration](admin.md) | users and permissions, backup and restore, monitoring and metrics, upkeep |
| [Cluster](cluster.md) | Raft cluster setup, reads and writes, failover, limits |
| [Storage](storage.md) | data directory, dictionary, LSM indexes, write-ahead log and checkpoints, snapshots, memory |
| [Performance](performance.md) | measured comparison, tuning, benchmarking tools |
| [Security](security.md) | authentication, lockout, TLS, guards against hostile input, supply chain |

The [README](../README.md) gives the overview. The benchmark scripts that produced these
numbers are not part of the repository.
