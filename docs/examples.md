# Examples

Runnable programs that talk to a running GraphRD server — in the repository's
`examples/` and `clients/` directories, one folder per language. They all play
the same small scene, so switching languages is a matter of reading another
folder, not another story.

## The scene

Create the repository `demo`, load this JSON-LD document, ask "who works at
Acme", and read the graph back:

```json
{
  "@context": {
    "name": "http://example.org/name",
    "age": "http://example.org/age",
    "salary": "http://example.org/salary",
    "worksFor": "http://example.org/worksFor",
    "Person": "http://example.org/Person",
    "Company": "http://example.org/Company"
  },
  "@graph": [
    { "@id": "http://example.org/alice", "@type": "Person", "name": "Alice",
      "age": 34, "salary": 82000, "worksFor": { "@id": "http://example.org/acme" } },
    { "@id": "http://example.org/bob", "@type": "Person", "name": "Bob",
      "age": 28, "salary": 61000, "worksFor": { "@id": "http://example.org/acme" } },
    { "@id": "http://example.org/acme", "@type": "Company", "name": "Acme" }
  ]
}
```

The endpoints the scene uses — everything else follows the same shape:

| Step | Call |
|---|---|
| create the repository | `POST /rest/repositories` with `{"id":"demo"}` |
| load JSON-LD | `POST /repositories/demo/statements` with `Content-Type: application/ld+json` |
| ask a question | `POST /repositories/demo` with `Content-Type: application/sparql-query` |
| read the graph | `GET /repositories/demo/statements` with `Accept: application/ld+json` |

## Python, whole and runnable

Standard library only; every other language in `examples/` does exactly this.

```python
BASE = "http://localhost:7200"          # or the URL in GRAPHRD_URL
REPO = "demo"

call("POST", "/rest/repositories",
     json.dumps({"id": REPO}).encode(), "application/json")       # 409 = already there
call("POST", f"/repositories/{REPO}/statements",
     json.dumps(DATA).encode(), "application/ld+json")            # the scene's data
_, body = call("POST", f"/repositories/{REPO}", QUERY.encode(),
               "application/sparql-query", "application/sparql-results+json")
for row in json.loads(body)["results"]["bindings"]:
    print(row["name"]["value"])                                    # Alice, Bob
```

## The other folders

| Folder | One line about it |
|---|---|
| `examples/curl` | the scene in five shell commands, no dependencies |
| `examples/python` | the scene, standard library only |
| `examples/node` | the scene on the built-in `fetch` (Node 18+) |
| `examples/go` | the scene, standard library only |
| `examples/quarkus` | MicroProfile REST client and JDBC over the PostgreSQL wire protocol |

Every example honors `GRAPHRD_URL` when the server is elsewhere than
`http://localhost:7200`, and each was run against a live server before it
shipped. Hibernate over the SQL wire works too — that is the subject of
[SQL](sql.md), not of a separate example.

## The showcase: a personnel application

`examples/spring-boot-graphrd` is the example to read when one example is all
you will read: a complete Spring Boot application whose data lives nowhere but
GraphRD, built on the client library below.

| Layer | What is in it |
|---|---|
| `model` | `Employee`, `Company`, `SalaryReview` — Lombok entities mapped with `@RdfType`/`@RdfId`/`@RdfProperty` |
| `repository` | thin, typed wrappers over the mapper |
| `service` | validation (duplicate email, raises above 25%), raises that leave a record, paging |
| `controller` | companies, employees, reports |
| `config` | timeouts in `application.yml`, beans closed by Spring on shutdown |

What asking it looks like:

```bash
curl "localhost:8080/employees?minAge=30&company=http%3A%2F%2Fexample.org%2Fcompany%2Facme"
# → {"items":[{"name":"Alice", "companyName":"Acme", …}], "total":1, "page":0, "size":20}

curl -X POST "localhost:8080/employees/raise?id=http%3A%2F%2F…%2Falice" \
     -H 'Content-Type: application/json' -d '{"amount":9000,"reason":"promotion"}'
# → Alice at 91000 — and a SalaryReview on record

curl localhost:8080/reports/payroll
# → Acme: 2 people, 143000 total, 71500 average — a GROUP BY the server ran
```

## The same employee also comes out as RDF — content negotiation picks the
format, one endpoint for all: `Accept: application/ld+json`, `text/turtle`,
`application/n-triples` (`GET /employees/one?id=…` and the whole listing at
`GET /employees/rdf`; `em.toRdf(entity, RdfFormat)` in the library).

The Java client library

`clients/graphrd-client` is the pure-Java library the showcase runs on: a low-level
`RdfClient` (SPARQL, statements, repositories; pooled connections, timeouts,
zero dependencies) and an `RdfEntityManager` for annotated entities — no
Spring in the jar. All Spring integration (starter + Spring Data) lives in
`clients/graphrd-spring-data`.

```java
try (RdfClient client = RdfClient.builder()
             .baseUrl("http://localhost:7200").connectTimeout(ofSeconds(5)).build();
     RdfEntityManager em = new RdfEntityManager(client, "demo")) {
    em.save(new Person("http://example.org/alice", "Alice", acme));   // one request
    em.saveSmart(alice);                    // writes only what differs
    Person alice = em.find(Person.class, "http://example.org/alice");
    em.query(Person.class).where(Person::getAge, Op.GT, 30)          // typo-proof
      .orderBy(Person::getSalary, true).limit(10).list();
}
```

Derived queries (`findByAgeGreaterThanAndSalaryLessThan(…, …)` through a JDK
proxy), lazy `Ref<T>` references and `lang`/`datatype` literals are in the
library README — and in a Spring Boot application the jar configures itself:
`graphrd.*` in application.yml is all it takes.

Its tests boot a throwaway server from `target/release/graphrd` and run the
whole suite against it — `mvn test` in `clients/graphrd-client` is itself an example.

For Spring's own interface structure there is an optional module,
`clients/graphrd-spring-data`: `GraphRdRepository` extends Spring Data's
repositories, so the standard CRUD, `Pageable`, `Sort` and derived method
names (`findByAgeGreaterThanAndSalaryLessThan`, `Page<Person> findBy…(…,
Pageable)`) arrive as they always do — Spring Data parses the names, GraphRD
answers them — and custom queries are `@Query` SPARQL with `:param` binding,
mapped to records (the showcase's payroll reports are exactly that). The module being on the classpath is the whole setup: the
application's packages are scanned, no annotation anywhere, just like JPA.

## What the examples cover

Every operation below was run against a live server somewhere in the example
suite — nothing on this page is aspirational:

| Operation | Where it shows up |
|---|---|
| point read by IRI, filters, LIKE, ranges, IN, OR, EXISTS | the query languages, the library's criteria queries |
| joins between classes | the personnel app (`worksFor`), `examples/curl` step 5 |
| aggregates — COUNT, SUM, AVG, MIN, MAX, GROUP BY, HAVING | `/reports/payroll` (computed by the server) |
| create — JSON-LD, Turtle, N-Triples, SPARQL `INSERT DATA` | every language's step 2 |
| update — computed (`salary * 1.10`), with a record kept | `/employees/raise`, `SalaryReview` |
| delete — by condition, by IRI, whole graph (`DROP GRAPH`) | the Graph Store tour |
| named graphs — PUT/POST/GET via the Graph Store, `FROM <g>`, `WITH <g>` | the Graph Store tour |
| CONSTRUCT and DESCRIBE answers | the query languages |

## Authentication

The examples assume `graphrd serve` with authentication off. With `--auth`,
log in first (`POST /rest/login` with `{"username","password"}`) and carry the
returned bearer token on later requests; the same users and permissions then
apply to SQL connections as well.
