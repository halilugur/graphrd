# Examples

Small, runnable programs that talk to a running GraphRD server. They all play
the same scene — create the repository `demo`, load a JSON-LD document, ask a
SPARQL question, read the graph back — so switching languages is a matter of
reading another folder, not another story. The scene itself, the endpoints it
uses and what each example covers live in the documentation:
[Examples](../docs/examples.md).

## Pick your language

| Directory | Stack | Needs |
|---|---|---|
| [curl](curl) | one shell script | `curl` |
| [python](python) | Python 3, standard library only | — |
| [node](node) | Node.js 18+ (`fetch` is built in) | — |
| [go](go) | Go 1.22+ | — |
| [quarkus](quarkus) | Quarkus, REST client + JDBC | Maven |
| [spring-boot-graphrd](spring-boot-graphrd) | the showcase: a full personnel app on the client library | Maven |

## Also in the repository

- [clients/graphrd-client](../clients/graphrd-client) — the Java client library (`RdfClient` +
  `RdfEntityManager`, zero dependencies) the showcase runs on.
- [clients/graphrd-spring-data](../clients/graphrd-spring-data) — the optional Spring
  Data module: `GraphRdRepository` with `Pageable`, `Sort`, derived queries.

Every example honors the `GRAPHRD_URL` environment variable when the server
runs elsewhere than `http://localhost:7200`, and each was run against a live
server before it shipped. Authentication is off in the examples (`graphrd
serve`); with `--auth`, see [Administration](../docs/admin.md).
