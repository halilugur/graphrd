# Vector search

GraphRD indexes embedding vectors stored as literals and finds the nearest ones from SPARQL, so
semantic search (for example over text embedded by an LLM) runs next to the graph, the full-text
index and the rest of the query.

## Storing vectors

A vector is a literal of datatype `g:vector` (`http://graphrd.org/fn#vector`) listing its numbers:

```turtle
@prefix ex: <http://example.org/> .
@prefix g: <http://graphrd.org/fn#> .

ex:doc1 ex:text "Graph databases store relations" ;
        ex:embedding "[0.12, -0.53, 0.33, 0.08]"^^g:vector .
```

Every `g:vector` literal added to a repository is indexed: no setting to turn on. All vectors of a
repository should have the same length; vectors of another length than the first indexed one are
stored but not indexed. Similarity is the cosine (vectors are normalized when indexed).

## Nearest neighbours

`?v g:knn ?q` binds `?v` to the 10 vector literals nearest to the vector `?q`; `g:knnN` (for
example `g:knn50`) returns N. `?q` is a vector literal in the query, or a variable bound before
the pattern (by `VALUES`, `BIND` or another pattern).

```sparql
PREFIX g: <http://graphrd.org/fn#>
PREFIX ex: <http://example.org/>
SELECT ?doc ?score WHERE {
  ?emb g:knn5 "[0.10, -0.50, 0.30, 0.10]"^^g:vector .
  ?doc ex:embedding ?emb .
  BIND(g:similarity(?emb, "[0.10, -0.50, 0.30, 0.10]"^^g:vector) AS ?score)
}
ORDER BY DESC(?score)
```

`g:similarity(?a, ?b)` is the cosine similarity of two vectors (an `xsd:double` between -1 and 1).

## Hybrid search

Combine the full-text index and the vector index in one query: take the text matches, then rank
them by similarity, or add both scores.

```sparql
PREFIX g: <http://graphrd.org/fn#>
PREFIX ex: <http://example.org/>
SELECT ?doc ?score WHERE {
  ?text g:fts "graph database" .
  ?doc ex:text ?text ; ex:embedding ?emb .
  BIND(g:ftsScore(?text, "graph database") / 10 + g:similarity(?emb, "[0.10, -0.50, 0.30, 0.10]"^^g:vector) AS ?score)
}
ORDER BY DESC(?score)
LIMIT 10
```

Or the other way round: `?emb g:knn100 ?q` for candidates, then `?text g:fts …` to keep those that
also match the words.

## How it works

The index is an HNSW graph (hierarchical navigable small world: 16 links per node, 32 on the
bottom layer, neighbours chosen to spread in every direction). Searches return approximate
nearest neighbours: on clustered data, recall@10 is 1.0 at 100,000 vectors and 0.96 at a million.
The graph is kept in memory (about the vectors' size plus 128 bytes a node) and written to
`vectors.bin` at checkpoints; vectors added since are indexed again when the log is replayed.
Literals the repository no longer holds are skipped by searches.

Measured with `uv run --with numpy python3 bench/vectors.py` (64-dimensional vectors around 1,000
centres, `knn10` queries over HTTP, Apple M2 Pro):

| Vectors | Indexing (`graphrd load`) | p50 | p95 | p99 | recall@10 |
|---|---|---|---|---|---|
| 100,000 | 22 s | 0.51 ms | 0.71 ms | 0.86 ms | 1.000 |
| 1,000,000 | 267 s | 0.82 ms | 1.47 ms | 2.07 ms | 0.957 |

Indexing runs on one core; loading vectors over HTTP indexes them as they arrive.
