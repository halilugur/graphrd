# Performance

All figures are GraphRD 1.0.2 unless a column says otherwise. Query latency is measured natively
on an Apple M2 Pro (10 cores, 16 GB): the median of 30 runs after 3 warm-ups, CSV results over
HTTP. The concurrency, cold-start and resource figures come from the same machine with the server
confined to a **2 CPU / 2 GB** container — the tight envelope stated next to each number.

The benchmark data is one synthetic graph of **900,231 triples** — 100,000 persons with ages and
salaries, 500 companies, cities, tags and 100,000 posts — generated from a fixed seed. The twelve
queries below cover the shapes that matter: point lookups, filters, `GROUP BY` and aggregates
over joins, `OPTIONAL`, `REGEX`, a property path, a top-K sort, a 198,000-row join and a `UNION`.

## At a glance

| | |
|---|---|
| Point lookup | **0.06 ms** |
| Biggest join (198 K rows, counted) | **13.7 ms** |
| Bulk load over HTTP | ~370,000 triples/s |
| Bulk load offline (`graphrd load`) | ~1.5 million lines/s |
| Cold start to first query | **under a second** |
| Idle memory | **~21 MB** |
| With the benchmark data loaded | ~330 MB (2 GB envelope) |
| The data on disk | **71 MB** |
| The whole binary | 6 MB, no runtime |

## Query latency

| Query | Median (ms) |
|---|---:|
| Q1 point lookup | 0.06 |
| Q2 `COUNT` + `FILTER` on age | 0.58 |
| Q3 `GROUP BY` company, top 5 | 1.43 |
| Q4 `AVG` over a join, top 5 | 9.44 |
| Q5 `OPTIONAL` + `LIMIT 200` | 0.44 |
| Q6 `REGEX` count | 5.79 |
| Q7 property path `knows+` | 13.8 |
| Q8 `ORDER BY DESC` + `LIMIT 10` over 100 K | 6.98 |
| Q9 join count (20 K) | 3.13 |
| Q10 `UNION` count | 0.18 |
| Q11 date filter count | 2.76 |
| Q12 join count (198 K) | 13.7 |

Point lookups answer from the dictionary and the person index in one hop; the heaviest query in
the suite scans 198,000 solutions and still lands under 14 ms.

## Writes and durability

Every commit is fsynced before it is acknowledged — a process crash, a `kill -9`, a power loss:
what the server confirmed is on disk. A single write costs one fsync (a few milliseconds on real
disks), and concurrent writers share fsyncs through group commit, so throughput does not collapse
when many clients write at once. Loading the 900,231 triples over one HTTP request takes about
2.4 s; the offline `graphrd load` path skips the write-ahead log entirely and parses in parallel
at roughly 1.5 million lines/s.

## Concurrency

Eight clients, each on a persistent connection, running a mix of the five heaviest queries
against the server confined to two cores and two gigabytes:

| | |
|---|---|
| Throughput | **332 queries/s** |
| Latency p95 | 91 ms |
| Errors | 0 |

Snapshot isolation means readers never block on writers and vice versa; the numbers above were
taken with the data fully loaded.

## Cold start

The server restarted (container restart, data on disk), then the first query after it came up:

| First query after restart | Median |
|---|---:|
| Point lookup | 0.53 ms |
| `COUNT` + `FILTER` | 1.4 ms |
| 20 K join | 7.2 ms |

The binary is ready in well under a second — there is no runtime to warm, no JVM to boot, no
separate index to open; the store mmaps its files and answers immediately.

## Resource footprint

| | |
|---|---|
| Idle process (resident) | ~21 MB |
| With the benchmark data (resident, container) | ~330 MB |
| The same data on disk | 71 MB |
| Binary size | 6 MB |

Memory is bounded by the memtables (2 million entries), the block cache (~70 MB) and the
dictionary caches — it does not grow with the data. Data larger than memory works: hot parts
stay in the block cache and the operating system's page cache. Everything on this page after
a query was served inside the 2 CPU / 2 GB envelope.

## 1.0.0 → 1.0.2

Three versions ran side by side on the same machine, each on its own copy of the same data,
with every request taking turns between the three servers (the order alternating), so machine
load fell on them alike. Medians; ≈ marks a difference within the noise of the run.

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

On the 900,231-triple benchmark of the table above, natively:

| Query (the 12 above) | 1.0.0 (ms) | 1.0.1 (ms) | 1.0.2 (ms) | 1.0.0 → 1.0.2 |
|---|---:|---:|---:|---:|
| Q1 point lookup | 0.05 | 0.05 | 0.06 | ≈ |
| Q2 `COUNT` + `FILTER` on age | 0.84 | 0.58 | 0.58 | −31% |
| Q3 `GROUP BY` company, top 5 | 1.68 | 1.48 | 1.43 | −15% |
| Q4 `AVG` over a join, top 5 | 11.9 | 9.54 | 9.44 | −21% |
| Q5 `OPTIONAL` + `LIMIT 200` | 0.50 | 0.46 | 0.44 | −12% |
| Q6 `REGEX` count | 11.1 | 10.4 | 5.79 | −48% |
| Q7 property path `knows+` | 14.5 | 13.6 | 13.8 | ≈ |
| Q8 `ORDER BY DESC` + `LIMIT 10` over 100 K | 8.06 | 7.27 | 6.98 | −13% |
| Q9 join count (20 K) | 4.51 | 3.23 | 3.13 | −31% |
| Q10 `UNION` count | 0.20 | 0.18 | 0.18 | −10% |
| Q11 date filter count | 4.53 | 2.65 | 2.76 | −39% |
| Q12 join count (198 K) | 21.7 | 14.0 | 13.7 | −37% |

Point lookups, star joins, `ASK`, `VALUES`, `CONSTRUCT` and the property paths stay within the
noise (0.1–0.3 ms either way); so does loading the 900,231 triples over HTTP (about 2.4 s,
~370,000 triples/s, in four alternating runs, the server's CPU time the same). Single writes take
about 3 ms in every version: the fsync bounds them. The largest change is outside the suites: an
`OPTIONAL` over a `UNION` on 40,000 rows took 19.7 s in 1.0.0 and takes 0.02 s since 1.0.1.

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
| `--sync off` | writes skip the fsync; they survive a crash of the process, not a power loss |
| batching writes | one update or upload with many statements costs one fsync; concurrent writers share fsyncs anyway (group commit) |
| `graphrd optimize` | after large deletes or many small loads: one run per index, no deletion markers |
| `infer=false` | queries skip inferred statements when they do not need them |
| `LIMIT` without `ORDER BY` | the query stops as soon as it has enough rows |
| `timeout` / `serve --timeout` | bounds runaway queries |
| `unionDefaultGraph` | `true` (default) queries every graph as the default graph; `false` reads only statements without a graph |

## Measuring

| Tool | Measures |
|---|---|
| `graphrd bench` | load, query latency, concurrent throughput and write latency on a synthetic graph |
| `graphrd compare` / `compare-report` | any SPARQL 1.1 store through the standard protocols only — [cli.md](cli.md) |
| `graphrd query --explain true` | the plan and time of one query |
| `GET /metrics`, `--slow-query` | latency histograms and slow queries of a running server ([admin.md](admin.md#monitoring)) |

When comparing two builds, run them side by side on copies of the same data and alternate the
order: figures from different runs or different data layouts are not comparable.
