# Formats

## RDF

| Format | Media types | Extensions | Read | Write |
|---|---|---|---|---|
| Turtle | `text/turtle`, `application/x-turtle` | `.ttl` | yes | yes |
| TriG | `application/trig` | `.trig` | yes | yes |
| N-Triples | `application/n-triples`, `text/plain` | `.nt` | yes | yes |
| N-Quads | `application/n-quads`, `text/x-nquads` | `.nq` | yes | yes |
| RDF/XML | `application/rdf+xml`, `application/xml` | `.rdf`, `.owl`, `.xml` | yes | yes |
| JSON-LD 1.1 | `application/ld+json` | `.jsonld`, `.json` | yes | yes |
| YAML-LD 1.0 | `application/ld+yaml` | `.yamlld` | yes | yes |
| Notation3 | `text/n3` | `.n3` | yes | without named graphs |

The parsers follow the W3C grammars strictly and pass the W3C test suites for RDF 1.1 and RDF 1.2
(see the README). Notable details:

- **RDF 1.2** in every syntax: triple terms `<<( s p o )>>`, reified triples `<< s p o ~ :r >>`,
  annotations `{| … |}`, base direction `"…"@ar--rtl`, `VERSION`; in RDF/XML
  `rdf:parseType="Triple"`, `rdf:annotation` and `its:dir`. The older RDF-star forms
  (`application/x-turtlestar`, `application/x-trigstar`) are read as the RDF 1.2 equivalents.
- **RDF/XML** handles DTD entities, with expansion capped (no "billion laughs") and nesting limited.
- **JSON-LD** is the full 1.1 toRdf algorithm, with remote contexts over HTTP(S) and the options of
  `graphrd convert --jsonld` (processing mode, `rdfDirection`, generalized RDF, an expanded
  context). An optional safe mode turns silently dropped keys into errors.
- **JSON-LD 1.2 / RDF 1.2 triple terms** (the JSON-LD-star draft): with
  `--jsonld processingMode=json-ld-1.2`, or `"@version": 1.2` in a context, a node object
  standing for exactly one relationship denotes that triple term — in `@id` or in value
  position — and `@annotation` asserts the triple and annotates the term:

  ```bash
  curl -X POST localhost:7200/repositories/zoo/statements -H 'Content-Type: application/ld+json' \
    --data-binary '{"@context":{"ex":"http://example.org/"},
                    "@id":"ex:alice",
                    "ex:claims":{"@id":"ex:bob","ex:knows":{"@id":"ex:alice"}}}'
  # :alice :claims <<( :bob :knows :alice )>> .
  ```

- **YAML-LD** is a JSON-LD document in YAML syntax (the canonical profile: block and flow
  mappings and sequences, plain, quoted and block scalars; anchors, aliases and tags are
  refused). Output is flow-style YAML. Example:

  ```bash
  curl -X POST localhost:7200/repositories/zoo/statements -H 'Content-Type: application/ld+yaml' \
    --data-binary '{"@context": {"ex": "http://example.org/"}, "@id": "ex:book/1", "ex:name": "Safahat"}'
  ```

  The same document in block style:

  ```yaml
  "@context":
    ex: http://example.org/
  "@id": ex:book/1
  ex:name: Safahat
  ```
- **Notation3**: formulas `{ … }` become blank-node named graphs (an empty formula is the literal
  `true`); N3 variables have no RDF equivalent and are errors.
- **Lenient loading** (`graphrd load --lenient`, `?lenient=true`) skips what does not parse:
  a line in N-Triples and N-Quads, a statement in Turtle, a graph block in TriG.

When writing, Turtle and TriG use the repository's prefixes; N3 output refuses data with named
graphs.

## Canonical N-Quads

`graphrd convert --to rdfc` writes canonical N-Quads (RDF Dataset Canonicalization, RDFC-1.0, with
SHA-256 or `--hash SHA384`), and `--to rdfc-map` the canonical blank node labels as JSON. The W3C
RDFC-1.0 suite passes.

## Query results

| Format | Media type | For |
|---|---|---|
| SPARQL JSON | `application/sparql-results+json` (default) | SELECT, ASK |
| SPARQL XML | `application/sparql-results+xml` | SELECT, ASK |
| CSV | `text/csv` | SELECT, ASK |
| TSV | `text/tab-separated-values` | SELECT, ASK |
| SPARQL-star JSON | `application/x-sparqlstar-results+json` | SELECT with triple terms |
| any RDF format above | by `Accept` (default Turtle) | CONSTRUCT, DESCRIBE |

`application/json` alone asks for SPARQL JSON results, not JSON-LD.

## Content negotiation

Uploads take their format from `Content-Type` (or a `format` parameter or import setting); URLs
loaded with `LOAD` or import-from-URL take it from the response's content type, then from the file
extension, defaulting to Turtle. Downloads follow `Accept`.

Responses of 1 KB to 32 MB in a text format are gzip-compressed when the client sends
`Accept-Encoding: gzip`.
