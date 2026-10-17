# SHACL validation

A repository with `isShacl` on validates every write against SHACL shapes and refuses writes that
would leave the data invalid.

## Turning it on

```bash
curl -X POST localhost:7200/rest/repositories -H 'Content-Type: application/json' \
  -d '{"id":"people","params":{"isShacl":{"value":"true"}}}'
```

## Shapes

Shapes are statements in the named graph `urn:graphrd:shapes` (a compatibility name some
clients use is answered as the same graph). Load them like any data:

```turtle
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix ex: <http://example.org/> .

ex:PersonShape a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [
    sh:path ex:name ; sh:minCount 1 ; sh:maxCount 1 ; sh:datatype xsd:string ;
    sh:message "A person has exactly one name"
  ] ;
  sh:property [ sh:path ex:age ; sh:datatype xsd:integer ; sh:minInclusive 0 ; sh:maxInclusive 150 ] .
```

```bash
curl -X POST 'localhost:7200/repositories/people/statements?context=%3Curn%3Agraphrd%3Ashapes%3E' \
  -H 'Content-Type: text/turtle' --data-binary @shapes.ttl
```

Shapes take effect for the next write. Shapes are not validated against the data when loaded; use
the report endpoint below to check existing data.

## Supported features

All of SHACL Core and SHACL-SPARQL, plus SPARQL-based targets from SHACL Advanced Features. The W3C SHACL test suite passes
120 of 120 tests (core 98, sparql 22; see [Conformance](#conformance)).

| Kind | Supported |
|---|---|
| Targets | `sh:targetClass` (with `rdfs:subClassOf*`), `sh:targetNode`, `sh:targetSubjectsOf`, `sh:targetObjectsOf`, implicit class targets (a shape that is also an `rdfs:Class`), `sh:target [ a sh:SPARQLTarget ; sh:select "SELECT ?this ..." ]` |
| Paths | a predicate IRI, sequence paths `( ex:a ex:b )`, `sh:inversePath`, `sh:alternativePath`, `sh:zeroOrMorePath`, `sh:oneOrMorePath`, `sh:zeroOrOnePath`, nested |
| Cardinality | `sh:minCount`, `sh:maxCount` |
| Values | `sh:datatype` (ill-formed literals fail), `sh:class`, `sh:nodeKind`, `sh:in`, `sh:hasValue` |
| Ranges | `sh:minInclusive`, `sh:maxInclusive`, `sh:minExclusive`, `sh:maxExclusive` |
| Strings | `sh:pattern` (with `sh:flags`), `sh:minLength`, `sh:maxLength`, `sh:languageIn`, `sh:uniqueLang` |
| Property pairs | `sh:equals`, `sh:disjoint`, `sh:lessThan`, `sh:lessThanOrEquals` |
| Logic and shapes | `sh:and`, `sh:or`, `sh:not`, `sh:xone`, `sh:node`, `sh:property`, `sh:qualifiedValueShape` with `sh:qualifiedMinCount` / `sh:qualifiedMaxCount` / `sh:qualifiedValueShapesDisjoint` |
| Other | `sh:closed` with `sh:ignoredProperties`, `sh:deactivated`, `sh:severity` (`sh:Violation`, `sh:Warning`, `sh:Info`), `sh:message` |
| SPARQL | `sh:sparql` constraints (`sh:select`, `sh:prefixes` / `sh:declare`, `sh:message` with `{?var}`), SPARQL-based constraint components (`sh:ConstraintComponent` with `sh:parameter`, `sh:validator` ASK, `sh:nodeValidator`, `sh:propertyValidator`) |

Shapes referring to each other recursively are followed 64 levels deep; deeper references count as
conforming (SHACL leaves recursion undefined).

### SPARQL constraints

A SELECT query runs for each focus node with `$this` bound to it (and `$currentShape`,
`$shapesGraph`; `$PATH` is replaced by the property shape's path). Every solution is a result: its
`?value` and `?path` become `sh:value` and `sh:resultPath`, and `?failure` true makes the validation
fail. As SHACL requires, the query may not use `MINUS`, `VALUES` or `SERVICE` or assign `$this`,
and a subquery must project the pre-bound variables.

```turtle
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .

ex:OrderShape a sh:NodeShape ;
  sh:targetClass ex:Order ;
  sh:sparql [
    sh:message "The total {?t} is negative" ;
    sh:select "SELECT $this ?t WHERE { $this <http://example.org/total> ?t FILTER (?t < 0) }"
  ] .
```

### Severity

Only `sh:Violation` results (the default) reject a write; `sh:Warning` and `sh:Info` results show up
in the report of the whole repository.

## Validation of writes

Each write (upload, update, delete, import) is checked for the nodes it touches and the nodes that
link to them. If a violation remains, the write is rolled back and answered with `400` and the
report:

```json
{
  "conforms": false,
  "results": [
    {
      "focusNode": "<http://example.org/ann>",
      "resultPath": "<http://example.org/name>",
      "value": null,
      "sourceShape": "_:b0",
      "sourceConstraintComponent": "http://www.w3.org/ns/shacl#MinCountConstraintComponent",
      "resultSeverity": "Violation",
      "resultMessage": "A person has exactly one name"
    }
  ]
}
```

`graphrd load` into a SHACL repository goes through the same checks, batch by batch (see
[cli.md](cli.md#load)).

## Validating everything

```bash
curl localhost:7200/rest/repositories/people/shacl/validate                         # JSON
curl -H 'Accept: text/turtle' localhost:7200/rest/repositories/people/shacl/validate  # sh:ValidationReport
```

## Validating files

`graphrd validate` validates RDF files against a shapes file in memory, without a data directory,
and prints the `sh:ValidationReport` (Turtle, or `--format json`):

```bash
graphrd validate --shapes shapes.ttl data.ttl more-data.ttl
```

## Conformance

`tests/w3c/shacl.py` runs the W3C SHACL test suite through `graphrd validate`:

```bash
git clone --depth 1 https://github.com/w3c/data-shapes /tmp/data-shapes
uv run --with rdflib python3 tests/w3c/shacl.py /tmp/data-shapes
```

A report matches when `sh:conforms` agrees and the results agree as a multiset of focus node, path
(by structure), value, source shape, constraint component, severity and, where the expected report
names one, `sh:sourceConstraint`. Blank nodes compare equal to each other, and messages are not
compared.
