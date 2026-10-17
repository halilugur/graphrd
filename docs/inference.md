# Inference

Each repository has a ruleset. When statements are added, GraphRD derives what follows from them
by forward chaining and stores the results as *inferred* statements, next to the *explicit* ones
that were stated. Queries see both, so reading is as fast as for explicit data, and inferred
statements survive restarts like any other data.

## Rulesets

Set the ruleset when creating the repository (`params.ruleset`), with `graphrd load --ruleset`, or
by changing the repository's configuration (`PUT /rest/repositories/{id}`); changing it recomputes
the inferred statements.

| Ruleset | Derives |
|---|---|
| `empty` | nothing (see [query-time inference](#query-time-inference) to infer without storing) |
| `rdf` | RDF entailment: every predicate is an `rdf:Property` |
| `rdfs` | RDFS entailment: `rdfs:domain`, `rdfs:range`, `rdfs:subClassOf` and `rdfs:subPropertyOf` (with transitivity), plus the RDFS axioms and the `rdfs:Resource` / `rdf:Property` / `rdfs:Class` typing of everything used |
| `rdfsplus` | `rdfs`, plus `owl:inverseOf`, `owl:SymmetricProperty`, `owl:TransitiveProperty`, `owl:equivalentClass`, `owl:equivalentProperty`, and with `owl:sameAs` on: `owl:FunctionalProperty`, `owl:InverseFunctionalProperty` and `owl:sameAs` closure |
| `owl-horst` | `rdfsplus`, plus `owl:hasValue`, `owl:someValuesFrom`, `owl:allValuesFrom` restrictions and `owl:Class` typing |
| `owl2-rl` | the OWL 2 RL/RDF rules: `owl-horst`, plus the schema rules (`scm-*`: domains and ranges along the hierarchies, subsumption between restrictions, every class between `owl:Nothing` and `owl:Thing`), domains and ranges across `owl:inverseOf`, `someValuesFrom owl:Thing`, and the axioms written as RDF lists: `owl:intersectionOf` (both directions), `owl:unionOf`, `owl:propertyChainAxiom`, `owl:oneOf`, and with `owl:sameAs` on `owl:hasKey`, `owl:maxCardinality 1` and `eq-ref` (everything the same as itself; not in `-optimized`) |
| `*-optimized` | the same without the axioms and the typing of every resource (`x a rdfs:Resource`, `p a rdf:Property`, …), which most applications never query and which multiplies the data |
| `owl-max` | alias of `owl2-rl` |
| `owl2-ql` | alias of `rdfsplus` |

OWL 2 RL's list axioms are compiled into rules for the lists the data holds. When such an axiom is
added or removed, the rules are compiled again and every inference is derived again, so the
schema may arrive before or after the data. OWL 2 RL's rules that only detect inconsistencies
(`owl:disjointWith`, `owl:AsymmetricProperty`, `owl:Nothing` members, …) derive nothing here.

The default for new repositories is `rdfsplus-optimized`. `owl:sameAs` reasoning is off unless the
repository sets `disableSameAs` to `false`.

## Explicit and inferred statements

```bash
# only what was stated
curl -G localhost:7200/repositories/zoo --data-urlencode infer=false \
  --data-urlencode 'query=SELECT * WHERE { ?s a ?type }'

# sizes
curl localhost:7200/rest/repositories/zoo/size
# {"inferred":12,"total":19,"explicit":7}
```

In a query, `GRAPH <http://graphrd.org/graph#explicit> { … }` and
`GRAPH <http://graphrd.org/graph#implicit> { … }` select one kind (see [sparql.md](sparql.md)).
Exports (`graphrd dump`, `GET /statements?infer=false`, backups) contain explicit statements only.

Stating something that was inferred makes it explicit; it then stays when the premises go.

## Query-time inference

`infer=at-query` (on the query endpoints, or `graphrd query --infer at-query`) infers at query time
instead of reading stored inferences, so a repository with the `empty` ruleset can still be queried
with RDFS and OWL property semantics, and a schema change takes effect without recomputing
anything. The schema of the explicit statements is read when the query starts; then each triple
pattern of the default graph with a fixed predicate is answered by backward chaining:

- `?x rdf:type C` matches what is stated of C and of its subclasses (`rdfs:subClassOf`,
  `owl:equivalentClass`), and the subjects and objects of properties whose domain or range is
  one of them;
- `?x p ?y` matches `p` and its subproperties (`rdfs:subPropertyOf`, `owl:equivalentProperty`),
  read backwards for `owl:inverseOf` and `owl:SymmetricProperty`, and transitively for an
  `owl:TransitiveProperty`.

```bash
curl -G localhost:7200/repositories/zoo --data-urlencode infer=at-query \
  --data-urlencode 'query=SELECT ?a WHERE { ?a a <http://example.org/Animal> }'
```

The answers are those of `rdfsplus-optimized` materialization (without `owl:sameAs`). Patterns with
a variable predicate and patterns inside `GRAPH` are answered as stated. Each pattern costs a few
index scans more than a stored one: materialize for heavy query loads, use `at-query` for
changing schemas or occasional reasoning.

## Deleting

Removing an explicit statement retracts what was derived from it, and only that: GraphRD uses the
DRed algorithm (delete and re-derive). It first removes every inference that might depend on the
deleted statements, then derives again those that still have another support. A deletion costs
time in proportion to what depends on it, not to the size of the repository.

## Custom rules (`.pie` files)

Set the ruleset to the path of a `.pie` file to use your own rules. The format has three blocks:

```
// family rules
Prefices
{
  ex : http://ex.org/
}
Axioms
{
  <ex:ancestorOf> <ex:kind> <ex:Transitive>
}
Rules
{
Id: parent_is_ancestor
  x <ex:parentOf> y
  ------------------------
  x <ex:ancestorOf> y

Id: ancestor_transitive
  x <ex:ancestorOf> y
  y <ex:ancestorOf> z   [Constraint x != z]
  ------------------------
  x <ex:ancestorOf> z
}
```

- `Prefices` maps prefixes to namespaces; IRIs are written `<prefix:local>` or as full IRIs.
- `Axioms` are triples that always hold.
- Each rule has an `Id:`, one or more premises (triple patterns; single letters are variables), a
  dashed line, and one or more consequences. `[Constraint a != b]` after a premise requires two
  variables to differ.

```bash
curl -X POST localhost:7200/rest/repositories -H 'Content-Type: application/json' \
  -d '{"id":"family","params":{"ruleset":{"value":"/srv/rules/family.pie"}}}'
```

A rule file that cannot be read or parsed turns inference off for the repository, with an error on
standard error.

## Conformance

The SPARQL 1.1 entailment regime tests (`tests/w3c/run.py … entailment`) run each test under the
weakest regime it allows that GraphRD implements: RDF (`rdf`), RDFS and D (`rdfs`), OWL 2
RDF-Based Semantics (`owl2-rl`, with `owl:sameAs`). 46 of 48 pass; the other 22 accept only OWL 2
Direct Semantics or RIF, which need a description logic reasoner. The two failing tests list both
OWL Direct and RDF-Based semantics and expect only named classes, where RDF-Based semantics also
entails the anonymous restriction class GraphRD returns.

## How it runs

Rules are evaluated semi-naively: each round joins only the statements new in the previous round
with the rest of the data, until nothing new appears. A rule is skipped when one of its other
premises cannot match anything in the repository (instance data without schema triples costs
almost nothing). Bulk loads run inference once over the whole batch.

Forward chaining on a large A-box (`bench/inference.py`: the comparison data plus an ontology with
class hierarchies, domains and ranges, a symmetric and an inverse property; `graphrd load` on an
Apple M2 Pro):

| Ruleset | 0.9M triples: load | inferred | 8M triples: load | inferred |
|---|---|---|---|---|
| `empty` | 1.4 s | 0 | 10.5 s | 0 |
| `rdfs-optimized` | 2.3 s | 122,151 | 23.2 s | 1,110,151 |
| `rdfsplus-optimized` | 3.8 s | 322,117 | 35.6 s | 2,610,107 |
| `owl2-rl-optimized` | 4.4 s | 322,132 | 41.0 s | 2,610,122 |

## RDF Rank

RDF Rank is PageRank over the links between resources (statements whose object is an IRI).

```bash
curl -X POST 'localhost:7200/rest/rank/compute?repository=zoo'
curl 'localhost:7200/rest/rank/status?repository=zoo'     # EMPTY, COMPUTED or OUTDATED
```

```sparql
PREFIX rank: <http://graphrd.org/rank#>
SELECT ?node ?score WHERE { ?node rank:score ?score } ORDER BY DESC(?score) LIMIT 10
```

`rank:score3` and `rank:score5` give the score rounded to 3 or 5 digits. The status becomes
`OUTDATED` when the data changes; compute again to refresh it.
