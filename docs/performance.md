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

## 1.0.0, 1.0.1 and 1.0.2

The table above was measured on 1.0.0 and has not been measured again. To compare the versions,
1.0.0 (commit `ad12df5`), 1.0.1 and 1.0.2 ran side by side on an Apple M2 Pro (10 cores, 16 GB),
each on its own copy of the same data, with every request taking turns between the three servers
(the order alternating), so load on the machine fell on all of them alike. Figures are medians,
JSON results over HTTP; ≈ marks a difference within the noise of the run.

On the `graphrd compare` graph (100,000 persons, ~600,000 triples; 40 runs, 15 for the slowest):

| Query (`graphrd compare` suite) | 1.0.0 (ms) | 1.0.1 (ms) | 1.0.2 (ms) | 1.0.0 → 1.0.2 |
|---|---:|---:|---:|---:|
| `MINUS` | 1.94 | 0.51 | 0.51 | −74% |
| Large result: every name, 100,000 rows | 204.0 | 83.8 | 75.6 | −63% |
| `REGEX` over all names | 43.1 | 39.9 | 18.4 | −57% |
| `COUNT(*)` over every triple | 23.2 | 10.4 | 10.0 | −57% |
| `ORDER BY` name, `OFFSET` 50,000 | 83.0 | 82.5 | 39.1 | −53% |
| `COUNT(DISTINCT ?o)` | 4.64 | 2.33 | 2.35 | −49% |
| `CONTAINS` filter over all bios | 4.89 | 3.37 | 3.14 | −36% |
| `GROUP BY` with `AVG` over all people | 29.6 | 21.3 | 21.0 | −29% |
| Join of three patterns, counted | 32.5 | 25.9 | 23.6 | −27% |
| `UNION` of two classes, counted | 0.44 | 0.35 | 0.34 | −23% |

On the data of the table above (900,231 triples; 30 runs each), natively rather than in Docker:

| Query (the 12 above) | 1.0.0 (ms) | 1.0.1 (ms) | 1.0.2 (ms) | 1.0.0 → 1.0.2 |
|---|---:|---:|---:|---:|
| Q1 point lookup | 0.05 | 0.05 | 0.06 | ≈ |
| Q2 `COUNT` + `FILTER` on age | 0.84 | 0.58 | 0.58 | −31% |
| Q3 `GROUP BY` company, top 5 | 1.68 | 1.48 | 1.43 | −15% |
| Q4 `AVG` over a join, top 5 | 11.9 | 9.54 | 9.44 | −21% |
| Q5 `OPTIONAL` + `LIMIT 200` | 0.50 | 0.46 | 0.44 | −12% |
| Q6 `REGEX` count | 11.1 | 10.4 | 5.79 | −48% |
| Q7 property path `knows+` | 14.5 | 13.6 | 13.8 | ≈ |
| Q8 `ORDER BY DESC` + `LIMIT 10` over 100K | 8.06 | 7.27 | 6.98 | −13% |
| Q9 join count (20K) | 4.51 | 3.23 | 3.13 | −31% |
| Q10 `UNION` count | 0.20 | 0.18 | 0.18 | −10% |
| Q11 date filter count | 4.53 | 2.65 | 2.76 | −39% |
| Q12 join count (198K) | 21.7 | 14.0 | 13.7 | −37% |

Point lookups, star joins, `ASK`, `VALUES`, `CONSTRUCT` and the property paths stay within the
noise (0.1–0.3 ms either way); so does loading the 900,231 triples over HTTP, about 2.4 s
(~370,000 triples/s) in four alternating runs of 1.0.0 and 1.0.1, the server's CPU time the same.
Single writes take about 3 ms in every version: the fsync bounds them. The largest change is
outside the suites: an `OPTIONAL` over a `UNION` on 40,000 rows took 19.7 s in 1.0.0 and takes
0.02 s since 1.0.1 (one run each, `graphrd query`).

What changed in 1.0.1: `MINUS` and `OPTIONAL` over a group hash the right side instead of
comparing every pair of rows; results larger than the term cache read their terms in bulk; an
ungrouped `COUNT` builds no group key per row; maps keyed by dictionary ids use a faster hash;
and the block cache gives blocks in use a second chance before evicting them. In 1.0.2: string
filters (`REGEX`, `CONTAINS`, `STRSTARTS`, `STRENDS` on a variable) test the term where it
lies instead of copying it, and an `ORDER BY` on one variable keys its values by id instead of
hashing each term.

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
| `python3 bench/queries.py ENDPOINT…` | the 12 queries and single inserts against any running servers, taking turns run by run |
| `python3 bench/geo.py [BIN]` | GeoSPARQL filters on 100,000 geometries with and without the spatial index ([geosparql.md](geosparql.md#spatial-index)) |
| `graphrd bench` | load, query latency, concurrent throughput and write latency on a synthetic graph |
| `graphrd compare` / `compare-report` | any SPARQL 1.1 store through the standard protocols only |
| `bench/run.sh`, `bench/cluster-bench.sh` | the GraphRD-only numbers, and a cluster |
| `graphrd query --explain true` | the plan and time of one query |
| `GET /metrics`, `--slow-query` | latency histograms and slow queries of a running server ([admin.md](admin.md#monitoring)) |

When comparing two builds, run them side by side on copies of the same data and alternate the
order: figures from different runs or different data layouts are not comparable.
