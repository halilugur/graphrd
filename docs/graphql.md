# GraphQL

Every repository answers GraphQL queries with a schema derived from its data: no schema to write
and nothing to keep in sync.

## The derived schema

- Every class (each object of `rdf:type`) is a root query field and a type, named by the class
  IRI's local name (`http://example.org/Person` → `Person`).
- Every predicate used by the instances of a class is a field of that type, named by its local
  name (`http://example.org/worksFor` → `worksFor`).
- `id` is the resource's IRI, `types` its classes.
- A field without a selection set returns the value (or a list when there are several); with a
  selection set it follows the links to other resources.

`GET /rest/repositories/{id}/graphql?sdl` returns the derived schema in SDL.

## Queries

```bash
curl -X POST localhost:7200/rest/repositories/zoo/graphql -H 'Content-Type: application/json' \
  -d '{"query": "{ Person(limit: 5, orderBy: \"age\", desc: true) { id name age worksFor { name } } }"}'
```

Root field arguments:

| Argument | Meaning |
|---|---|
| `id` | one resource, by IRI |
| `where` | equality on fields: `where: {name: "Ann", city: "Ankara"}` (all must hold) |
| `orderBy`, `desc` | sort by a field's first value, descending with `desc: true` |
| `limit`, `offset` | page the result (default limit 100) |

Nested fields take `limit` too. Variables (`query ($n: String) { Person(where: {name: $n}) { id } }`
with `"variables": {"n": "Ann"}`), aliases and `__typename` are supported. The body may also be
`application/graphql` with the query as the whole body.

The answer is the standard `{"data": …}` or `{"errors": [{"message": …}]}`. Selections nest at most
8 levels deep, and a result is cut off with an error when it grows too large: narrow it with
`where` and `limit`.

## Mutations

The same schema has a `Mutation` type: for every type `T`, `createT`, `updateT` and `deleteT`.

```bash
curl -X POST localhost:7200/rest/repositories/zoo/graphql -H 'Content-Type: application/graphql' \
  -d 'mutation { createDog(id: "http://example.org/fido", input: {name: "Fido"}) { id name types } }'
```

| Field | Arguments | Effect |
|---|---|---|
| `createT` | `id` (optional), `input`, `graph`, `type` | adds `id rdf:type <class>` and the input's values; without `id` an IRI `<namespace>t/<random hex>` is made. An existing `T` with that id is an error |
| `updateT` | `id`, `input`, `graph` | replaces the values of each field in `input`; `null` removes them; other fields stay |
| `deleteT` | `id`, `graph` | removes the resource's statements and the statements linking to it |

Each field returns the resource with its selection (`deleteT` as it was before), or the id when
the field has no selection set.

Input values:

- A plain value follows what the predicate already holds: when its values are links, a string is
  an IRI (`knows: ["http://example.org/ann"]`); when they are typed literals, the value gets that
  datatype (`birthDate: "2001-05-04"` becomes an `xsd:date`, and a value that is not a valid number
  for a numeric datatype is an error). Otherwise strings are `xsd:string`, integers
  `xsd:integer`, floats `xsd:decimal` and booleans `xsd:boolean`.
- A list gives several values; `{id: "…"}` is a link and `{value: "…", lang: "tr"}` or
  `{value: "…", datatype: "…"}` a literal spelled out.
- A field the type does not have yet becomes a predicate in the class's namespace
  (`http://example.org/Person` → `http://example.org/email`). A type the data does not have yet can
  be created with its class IRI: `createRobot(type: "http://example.org/Robot", input: {…})`.

New values go to the `graph` given, else to the graph of the resource's `rdf:type` statement;
`graph` also limits what `update` and `delete` remove. A mutation request is one transaction: the
fields run in order, and when one fails, or a SHACL repository refuses the result (the report is
in `errors[0].extensions`), nothing of the request stays. Mutations need write access to the
repository and a POST; queries need read access only. The web UI's GraphQL page runs both.

## Limits

Names are local names, so two predicates with the same local name in different namespaces share a
field (a mutation picks the one in the type's namespace, else reports the field as ambiguous).
