# Command line

```
graphrd <command> [options]
```

| Command | What it does |
|---|---|
| [`serve`](#serve) | run the HTTP server (and optionally the PostgreSQL port and a cluster node) |
| [`load`](#load) | bulk-load RDF files into a repository, offline |
| [`dump`](#dump) | write a repository's statements to a file |
| [`query`](#query) | run a SPARQL query on a data directory |
| [`update`](#update) | run a SPARQL update on a data directory |
| [`backup`](#backup) | copy a data directory consistently |
| [`optimize`](#optimize) | compact every repository |
| [`validate`](#validate) | validate RDF files against SHACL shapes |
| [`convert`](#convert) | convert between RDF formats, or canonicalize (RDFC-1.0) |
| [`bench`, `compare`](#benchmarks) | benchmarks |

`graphrd help` prints the same summary; `graphrd --version` prints the version.

## Shared options of the offline tools

`load`, `dump`, `query`, `update`, `backup` and `optimize` work on a data directory directly, without
a server. The directory is locked while they run, so they fail with
`error: DIR is in use by another GraphRD process` while a server uses it.

| Option | Default | Meaning |
|---|---|---|
| `--data DIR` | `./graphrd-data` | data directory |
| `--repo ID` | `default` | repository |

Format names accepted by `--format`, `--from` and `--to`: `nt`, `nq`, `ttl`, `trig`, `rdf` (RDF/XML),
`jsonld`, `n3` (input only), or a media type such as `text/turtle`. Results of `query` take `json`,
`xml`, `csv` or `tsv`. Without `--format`, the file extension decides (`.gz` is not decompressed).

## serve

```bash
graphrd serve [options]
```

| Option | Default | Meaning |
|---|---|---|
| `--port N` | `7200` | HTTP port |
| `--host ADDR` | `127.0.0.1` | bind address; a warning is printed when it is not local and `--auth` is missing |
| `--data DIR` | `./graphrd-data` | data directory |
| `--memory` | off | keep repositories, users and saved queries in memory only; no data directory is used and everything ends with the process (not with `--cluster`) |
| `--auth USER:PASS` | none | require sign-in; this user is the administrator. Also read from `GRAPHRD_AUTH` |
| `--sync on\|off` | `on` | fsync each commit before acknowledging it; `off` survives a crash of the process but not a power loss |
| `--read-only` | off | refuse every change with 403 |
| `--cors` | off | answer cross-origin requests from any page |
| `--timeout S` | none | time limit in seconds for queries that do not set their own `timeout` |
| `--default-repo ID` | see below | repository answered at `/sparql`, `/query`, `/update` |
| `--service-cache S` | off | reuse SERVICE answers for S seconds ([sparql.md](sparql.md#federated-queries)) |
| `--service-timeout S` | 300 | time limit of one SERVICE call |
| `--service-retries N` | 0 | retry SERVICE calls that cannot connect or get a 5xx answer |
| `--slow-query MS` | off | log queries and updates slower than this (see [admin.md](admin.md#monitoring)) |
| `--llm-url URL`, `--llm-key KEY`, `--llm-model NAME` | off | answer questions in plain language with SPARQL written by a language model: its endpoint, key and model (also `GRAPHRD_LLM_URL`, `GRAPHRD_LLM_API_KEY`, `GRAPHRD_LLM_MODEL`); with only `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` set, that provider's API and default model (see [nl-query.md](nl-query.md)) |
| `--llm-api anthropic\|openai` | from the URL or key | the wire format |
| `--llm-effort E`, `--llm-timeout S` | model default, 120 | effort (`low` … `max`) and the time limit of one call |
| `--pg-port N` | off | also accept SQL over the PostgreSQL protocol (see [sql.md](sql.md)) |
| `--ui-dir DIR` | built in | serve another web UI build at `/`; the built-in UI moves to `/ui/` |
| `--tls-cert PEM`, `--tls-key PEM` | off | serve HTTPS (builds with `--features tls`) |
| `--cluster LIST`, `--node-id ID`, `--cluster-secret S` | off | Raft cluster (see [cluster.md](cluster.md)); the secret is also read from `GRAPHRD_CLUSTER_SECRET` |

Without `--default-repo`, `/sparql` answers the only repository when there is one, else the
repository named `default`.

## load

```bash
graphrd load [--data DIR] [--repo ID] [--ruleset R] [--format F] [--graph IRI] [--base IRI] [--lenient] FILE...
```

Loads files (or `-` for standard input, with `--format`) into a repository, creating it if needed
with the ruleset `--ruleset` (default `rdfsplus-optimized`, see [inference.md](inference.md)).

The data directory is locked, so `load` adds statements in bulk rather than through the server's
write path: files are parsed on all cores, each index gets the new statements as one sorted run,
and no write-ahead log is written. The repository's manifest is replaced at the end of each batch
(256 MB of N-Triples/N-Quads, or a whole file in other formats), so a crash during a load leaves
the repository as it was before that batch. Inference, full-text indexing and prefix registration
happen as with any other write. A repository with SHACL validation on loads through the checked
write path instead, batch by batch.

| Option | Meaning |
|---|---|
| `--graph IRI` | put statements without a graph into this named graph |
| `--base IRI` | base IRI for relative IRIs (Turtle, TriG, RDF/XML, JSON-LD) |
| `--lenient` | skip statements that do not parse and count them: a line in N-Triples/N-Quads, a statement in Turtle, a graph block in TriG; other formats stay strict |

Blank node labels mean the same node throughout one file, and different nodes in different files.

```bash
graphrd load --data db --repo zoo animals.ttl more.nq
graphrd load --data db --repo wiki --lenient latest-truthy.nt
zcat dump.nt.gz | graphrd load --data db --format nt -
```

## dump

```bash
graphrd dump [--data DIR] [--repo ID] [--format F] [--graph IRI|default] [--output FILE]
```

Writes the explicit statements (not inferred ones) as N-Quads, or in the format named by `--format`
or the extension of `--output`. `--graph` limits the output to one named graph, or to the default
graph with `--graph default`.

## query

```bash
graphrd query [--data DIR] [--repo ID] [--format F] [--output FILE] [--base IRI]
              [--infer false] [--explain true] (QUERY | --query-file FILE)
```

Without a query argument or `--query-file`, the query is read from standard input. SELECT and ASK
results default to JSON; CONSTRUCT and DESCRIBE to Turtle. `--explain true` prints the evaluation
plan and the time taken to standard error:

```bash
graphrd query --data db --repo zoo --format csv --explain true \
  'SELECT ?s WHERE { ?s a <http://example.org/Animal> }'
```

## update

```bash
graphrd update [--data DIR] [--repo ID] [--base IRI] (UPDATE | --update-file FILE)
```

## backup

```bash
graphrd backup [--data DIR] [--incremental] DESTINATION
```

Copies the data directory consistently. The copy is a data directory of its own: start a server on
it or use it with the other tools. `--incremental` updates an earlier backup in DESTINATION and
copies only what changed: index run files never change, so after the first backup only new runs,
the grown dictionary, the manifest and the logs are copied; files the data directory no longer has
are removed from the backup. Each backup is recorded in `DESTINATION/backup.log`. For backups of a
running server, use the REST API (see [admin.md](admin.md#backup-and-restore)).

## restore

```bash
graphrd restore --from BACKUP [--data DIR] [--to-time T]
```

Copies a backup into an empty data directory. `--to-time` (an `xsd:dateTime` such as
`2026-10-16T09:30:00Z`, or Unix milliseconds) then returns every repository to its state at that
moment, from its change history: point-in-time restore. Repositories need the
`historyRetention` setting, and T must lie within the kept history. The undone transactions also
leave the restored repository's history, so time travel there ends at T.

```bash
graphrd backup --data /var/lib/graphrd /backups/graphrd                 # first: full
graphrd backup --data /var/lib/graphrd --incremental /backups/graphrd   # later: changes only
graphrd restore --from /backups/graphrd --data /srv/graphrd-restored --to-time 2026-10-16T09:30:00Z
```

## optimize

```bash
graphrd optimize [--data DIR]
```

Merges each index of every repository into a single run file. Reads then touch one file per index
and deletion markers are gone. Useful after large deletions or many small loads.

## validate

```bash
graphrd validate --shapes shapes.ttl [--format turtle|json] [--base IRI] [--output FILE] data.ttl...
```

Validates RDF files against SHACL shapes in memory (no data directory) and writes the validation
report. Relative IRIs resolve against each file's own `file://` URL unless `--base` is given; the
shapes file may also be one of the data files. See [shacl.md](shacl.md).

## convert

```bash
graphrd convert [--from F] [--to F] [--base IRI] [--graph IRI] [--from-graph IRI|default] [INPUT] [--output FILE]
```

Converts between RDF formats without a data directory.

| Option | Meaning |
|---|---|
| `--graph IRI` | put statements without a graph into this graph |
| `--from-graph IRI\|default` | keep only one graph of the input |
| `--to rdfc` | canonical N-Quads (RDF Dataset Canonicalization, RDFC-1.0) |
| `--to rdfc-map` | the canonical blank node labels as JSON |
| `--hash SHA384` | hash function for RDFC-1.0 (default SHA256) |
| `--jsonld key=value` | JSON-LD options: `processingMode=json-ld-1.0`, `rdfDirection=…`, `produceGeneralizedRdf=true`, `expandContext=FILE` |

```bash
graphrd convert --from trig --to nt --from-graph http://example.org/g data.trig
graphrd convert --to rdfc data.nq --output canonical.nq
```

## Benchmarks

```bash
graphrd bench [--urls http://h:p[,http://h2:p2]] [--persons 100000] [--clients 8] [--seconds 5] [--repo bench]
graphrd compare --name NAME --query URL [--update URL] --load URL [--load-type T] [--graph IRI]
                [--persons 100000] [--clients 8] [--seconds 10] [--out FILE.json]
graphrd compare-report FILE.json...
```

`bench` loads a synthetic graph into GraphRD servers and measures load, query latency, concurrent
throughput and write latency. `compare` benchmarks any SPARQL 1.1 store through the standard
protocols only; `compare-report` prints Markdown tables of several runs. See
[performance.md](performance.md).
