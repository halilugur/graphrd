# Comparison

GraphRD 1.0.2 measured against two well-known RDF stores — **Oxigraph 0.5.11** and
**GraphDB Free 10.8.5** — on one benchmark: the same synthetic graph of **900,231 triples**
(100,000 persons with ages and salaries, 500 companies, cities, tags and 100,000 posts,
generated from a fixed seed). Everything ran as a container on one Linux VM (10 cores, 8 GB),
each engine confined to the same **2 CPU / 2 GB** envelope, and the load generator ran inside
the VM next to them. Figures are medians over 30 runs after 3 warm-ups, CSV results over HTTP;
writes are 200 sequential `INSERT DATA` commits.

One asymmetry to know upfront, because it colors every write number: GraphRD and GraphDB fsync
every commit before acknowledging it, Oxigraph does not fsync at all (verified with strace;
its engine has no sync setting). The single-write comparison is therefore between durability
classes, noted where it matters.

## Query latency (ms, median)

| Query | GraphRD 1.0.2 | Oxigraph 0.5.11 | GraphDB Free 10.8.5 |
|---|---:|---:|---:|
| Q1 point lookup | **0.16** ✅ | 0.20 | 2.28 |
| Q2 `COUNT` + `FILTER` on age | **0.67** ✅ | 4.43 | 7.73 |
| Q3 `GROUP BY` company, top 5 | **1.08** ✅ | 5.30 | 7.93 |
| Q4 `AVG` over a join, top 5 | **8.51** ✅ | 67.25 | 20.75 |
| Q5 `OPTIONAL` + `LIMIT 200` | **0.40** ✅ | 1.45 | 3.31 |
| Q6 `REGEX` count | **4.95** ✅ | 25.84 | 19.85 |
| Q7 property path `knows+` | **8.35** ✅ | 79.42 | 31.92 |
| Q8 `ORDER BY DESC` + `LIMIT 10` | **6.80** ✅ | 50.94 | 28.87 |
| Q9 join count (20 K) | **4.03** ✅ | 42.37 | 11.37 |
| Q10 `UNION` count | **0.22** ✅ | 0.36 | 1.77 |
| Q11 date filter count | **2.22** ✅ | 24.76 | 10.56 |
| Q12 join count (198 K) | **14.69** ✅ | 416.90 | 95.66 |

GraphRD leads on all twelve; the smallest gap is Q1 (1.25×) and the largest is Q12, where the
198,000-row join finishes 28× sooner than in Oxigraph and 6.5× sooner than in GraphDB.

## Writes

| | GraphRD | Oxigraph | GraphDB |
|---|---:|---:|---:|
| Single `INSERT DATA` commit | **0.25 ms** ✅ | 0.16 ms¹ | 7.09 ms |
| Bulk load, 900,231 triples over HTTP | 2.53 s | 7.93 s | **1.89 s** ✅ |
| Durability | fsync per commit | none¹ | fsync per commit |

¹ Oxigraph acknowledges writes without an fsync — a crash of the process can lose them — so its
write latency is a different durability class, not a like-for-like win. GraphDB loads the bulk
file fastest but pays for it afterwards: every single insert costs 7 ms. GraphRD's 38× faster
single-write path comes with the same per-commit fsync as GraphDB's.

## Concurrency

Eight clients, each on a persistent connection, mixed queries from the five heaviest families:

| | GraphRD | Oxigraph | GraphDB |
|---|---:|---:|---:|
| Throughput | **282 q/s** ✅ | 39 q/s | 118 q/s |
| Latency p95 | **96 ms** ✅ | 574 ms | 117 ms |

## Cold start

The container is restarted with the data on disk; the number is the first query after the
server came back up:

| First query after restart | GraphRD | Oxigraph | GraphDB |
|---|---:|---:|---:|
| Point lookup | 0.62 ms | **0.48 ms** ✅ | 2,504 ms |
| `COUNT` + `FILTER` | **1.3 ms** ✅ | 5.2 ms | 78 ms |
| 20 K join | **7.2 ms** ✅ | 52.2 ms | 171 ms |

GraphDB's first query waits for the JVM, Tomcat and its indexes to come up (about 2.5 seconds);
GraphRD serves within a second of process start, because there is no runtime to boot.

## Resources

With the benchmark data loaded, on the same 2 GB envelope:

| | GraphRD | Oxigraph | GraphDB |
|---|---:|---:|---:|
| Resident memory, loaded | **~330 MB** ✅ | ~836 MB | 1,321 MB |
| Resident memory, idle | **~21 MB** ✅ | ~26 MB | ~1,310 MB |
| The data on disk | **71 MB** ✅ | — | 128 MB |
| Container image | **157 MB** ✅ | 55 MB | ~1,200 MB |
| The binary alone | **6 MB** | — | — |

GraphDB reserves its heap whether or not data is loaded; Oxigraph holds the graph in memory.
GraphRD's memory is bounded by its memtables, block cache and dictionary caches — it does not
grow with the data, and data larger than memory keeps working from the block cache and the
page cache.

## Notes on method

- Versions: GraphRD 1.0.2, Oxigraph 0.5.11 (official image), GraphDB Free 10.8.5 (the
  container's embedded LITE license caps it at 1 CPU — the 2 CPU envelope is above its own
  license limit, so nothing here undercuts it).
- GraphDB stores 900,324 triples for the same file — it silently removes duplicates — while
  GraphRD keeps the raw 900,231; both counts verified with `COUNT(*)`.
- The engines were driven through their standard protocols only: the RDF4J repository protocol
  for GraphRD and GraphDB, the SPARQL 1.1 protocol for Oxigraph — the same client for the
  first two, a thin adapter for the third.
- Take these as one honest datapoint on one workload and one machine, not a universal ranking:
  reproduce it on your own data with `graphrd compare` ([cli.md](cli.md)).
