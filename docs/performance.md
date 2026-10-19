# Performance

## Where GraphRD stands

Measured in Docker with 4 CPUs and 6 GB, on synthetic data
(900,231 triples: persons, companies, cities, tags, posts), median of 30 runs after 3 warm-up runs,
CSV results over HTTP:

| Query | GraphRD (ms) |
|---|---|
| Q1 point lookup | 0.58 |
| Q2 `COUNT` + `FILTER` on age | 1.86 |
| Q3 `GROUP BY` company, top 5 | 2.55 |
| Q4 `AVG` over a join, top 5 | 12.4 |
| Q5 `OPTIONAL` + `LIMIT 200` | 1.18 |
| Q6 `REGEX` count | 7.56 |
| Q7 property path `knows+` | 16.6 |
| Q8 `ORDER BY DESC` + `LIMIT 10` over 100K | 10.0 |
| Q9 join count (20K) | 6.60 |
| Q10 `UNION` count | 0.80 |
| Q11 date filter count | 8.08 |
| Q12 join count (198K) | 35.2 |
| single `INSERT DATA` (fsync on) | 0.71 |

Bulk loading the same file offline: `load` takes 0.6 s (about 1.5 million lines/s), 1.8 s
including container start. The concurrency, cluster and durability measurements are below.

## 1.0.0 against 1.0.1

The table above was measured on 1.0.0 and has not been measured again. To compare the two
versions, 1.0.0 (commit `ad12df5`) and 1.0.1 ran side by side on an Apple M2 Pro (10 cores,
16 GB), each on its own copy of the same data: the `graphrd compare` graph with 100,000 persons
(~600,000 triples). Every request alternated between the two servers, so background load hit both
alike; the figure is the median of 40 runs (15 for the slowest queries), JSON results over HTTP. The
machine was busy (load average about 10), so the absolute times run high; the ratios are what to
read.

| Query (`graphrd compare` suite) | 1.0.0 (ms) | 1.0.1 (ms) | Change |
|---|---:|---:|---:|
| `MINUS` | 3.03 | 1.16 | −62% |
| Large result: every name, 100,000 rows | 205.6 | 87.3 | −58% |
| `COUNT(DISTINCT ?o)` | 6.47 | 2.99 | −54% |
| `COUNT(*)` over every triple | 27.9 | 13.1 | −53% |
| `CONTAINS` filter over all bios | 7.09 | 4.87 | −31% |
| Join of three patterns, counted | 33.2 | 24.1 | −27% |
| `GROUP BY` with `AVG` over all people | 36.1 | 26.9 | −26% |
| `UNION` of two classes, counted | 0.76 | 0.57 | −26% |
| Join: colleagues of a person | 0.28 | 0.23 | −18% |

Point lookups, star joins, `ASK`, `VALUES`, `CONSTRUCT` and the property paths stay within the
noise of this run (0.1–0.3 ms either way), as do `REGEX` and `ORDER BY` with a deep `OFFSET`.
Single writes, measured in separate `graphrd compare` runs, take about 3 ms either way: the fsync
bounds them. The largest change is outside the suite: an `OPTIONAL` over a `UNION` on 40,000
rows took 19.7 s in 1.0.0 and takes 0.02 s in 1.0.1 (one run each, `graphrd query`).

What changed in 1.0.1: `MINUS` and `OPTIONAL` over a group hash the right side instead of comparing
every pair of rows; results larger than the term cache read their terms in bulk; an ungrouped
`COUNT` builds no group key per row; maps keyed by dictionary ids use a faster hash; and the block
cache gives blocks in use a second chance before evicting them.

## Tuning

| Setting | Effect |
|---|---|
| `graphrd load` instead of HTTP uploads | the fastest way in: parallel parsing, no write-ahead log, one sorted run per index |
| `ruleset`: `empty` or an `-optimized` ruleset | inference costs time on writes and space for inferred statements; the `-optimized` rulesets skip the axioms and resource typing nobody queries |
| `--sync off` | writes take ~0.3 ms instead of an fsync; they survive a crash of the process, not a power loss |
| batching writes | one update or upload with many statements costs one fsync; concurrent writers share fsyncs anyway (group commit) |
| `graphrd optimize` | after large deletes or many small loads: one run per index, no deletion markers |
| `infer=false` | queries skip inferred statements when they do not need them |
| `LIMIT` without `ORDER BY` | the query stops as soon as it has enough rows |
| `timeout` / `serve --timeout` | bounds runaway queries |
| `unionDefaultGraph` | `true` (default) queries every graph as the default graph; `false` reads only statements without a graph |

Memory is bounded by the memtables (2 million entries), the block cache (~70 MB) and the dictionary
caches; it does not grow with the data. Data larger than memory works; hot parts stay in the block
cache and the operating system's page cache.

## Measuring

| Tool | Measures |
|---|---|
| `bench/docker.sh up`, then `queries`, `cold`, `mem`, `load` | fresh servers in Docker, on volumes that keep the loaded data |
| `python3 bench/queries.py ENDPOINT…` | the 12 queries and single inserts against any running servers, side by side |
| `python3 bench/geo.py [BIN]` | GeoSPARQL filters on 100,000 geometries with and without the spatial index ([geosparql.md](geosparql.md#spatial-index)) |
| `graphrd bench` | load, query latency, concurrent throughput and write latency on a synthetic graph |
| `graphrd compare` / `compare-report` | any SPARQL 1.1 store through the standard protocols only |
| `bench/run.sh`, `bench/cluster-bench.sh` | the GraphRD-only numbers, and a cluster |
| `graphrd query --explain true` | the plan and time of one query |
| `GET /metrics`, `--slow-query` | latency histograms and slow queries of a running server ([admin.md](admin.md#monitoring)) |

When comparing two builds, run them side by side on copies of the same data and alternate the
order: figures from different runs or different data layouts are not comparable.
