# Getting started

GraphRD is a single binary: the server, the web UI and the offline tools. This page takes you from
nothing to a first query.

## Install

### From source

GraphRD needs only a Rust toolchain (1.85 or newer, edition 2024); it has no third-party
dependencies.

```bash
git clone <repository-url> graphrd
cd graphrd
cargo build --release
./target/release/graphrd serve
```

The optional `tls` feature adds HTTPS through rustls:

```bash
cargo build --release --features tls
```

### With Docker

The image runs as an unprivileged user and keeps its data in the `/data` volume.

```bash
docker build -t graphrd .
docker run -d --name graphrd -p 7200:7200 -v graphrd-data:/data \
  -e GRAPHRD_AUTH=admin:change-me graphrd
```

`GRAPHRD_AUTH` turns on authentication (see [admin.md](admin.md)). Without it, anyone who can reach
the port can read and change data.

## Start the server

```bash
graphrd serve
```

The server listens on <http://127.0.0.1:7200/> and stores its data in `./graphrd-data`. The most
used options:

| Option | Meaning |
|---|---|
| `--port 7200` | HTTP port |
| `--host 127.0.0.1` | bind address (`0.0.0.0` for every interface) |
| `--data ./graphrd-data` | data directory |
| `--memory` | keep everything in memory, nothing on disk |
| `--auth user:password` | require sign-in |

Every option is listed in [cli.md](cli.md).

## Create a repository

A repository holds RDF statements and has its own inference ruleset. Open the web UI at
<http://127.0.0.1:7200/> and choose **Repositories → Create**, or use the REST API:

```bash
curl -X POST localhost:7200/rest/repositories -H 'Content-Type: application/json' \
  -d '{"id":"zoo","params":{"ruleset":{"value":"rdfs"}}}'
```

## Load data

Save this as `animals.ttl`:

```turtle
@prefix ex: <http://example.org/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

ex:Dog rdfs:subClassOf ex:Animal .
ex:rex a ex:Dog ; ex:name "Rex" .
ex:tom a ex:Animal ; ex:name "Tom" .
```

Then send it to the repository:

```bash
curl -X POST localhost:7200/repositories/zoo/statements \
  -H 'Content-Type: text/turtle' --data-binary @animals.ttl
```

Large files load much faster offline, while the server is stopped (see [cli.md](cli.md#load)):

```bash
graphrd load --data ./graphrd-data --repo zoo animals.ttl
```

## Run a query

```bash
curl -G localhost:7200/repositories/zoo -H 'Accept: text/csv' \
  --data-urlencode 'query=PREFIX ex: <http://example.org/> SELECT ?a ?name WHERE { ?a a ex:Animal ; ex:name ?name }'
```

Both Rex and Tom are animals: the `rdfs` ruleset inferred `ex:rex a ex:Animal` from the class
hierarchy. Add `infer=false` to see only what was stated:

```bash
curl -G localhost:7200/repositories/zoo -H 'Accept: text/csv' --data-urlencode infer=false \
  --data-urlencode 'query=PREFIX ex: <http://example.org/> SELECT ?a WHERE { ?a a ex:Animal }'
```

Prefixes declared in loaded Turtle are remembered by the repository, so later queries can use
`ex:` without a `PREFIX` line.

## Update data

```bash
curl -X POST localhost:7200/repositories/zoo/statements \
  --data-urlencode 'update=PREFIX ex: <http://example.org/> INSERT DATA { ex:kit a ex:Dog ; ex:name "Kit" }'
```

## Next steps

- [sparql.md](sparql.md): what the query engine supports and the request parameters.
- [http-api.md](http-api.md): every HTTP endpoint.
- [inference.md](inference.md): rulesets and custom rules.
- [formats.md](formats.md): file formats for import and export.
