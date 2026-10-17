# A personnel application on GraphRD

A small but complete Spring Boot application: companies, employees and salary
reviews live in GraphRD through the client library
([clients/graphrd-client](../../clients/graphrd-client)); joins are IRIs, reports are computed by
the server in SPARQL. Lombok keeps the models short.

## The package layout

| Package | What lives there |
|---|---|
| `model` | the mapped entities: `Employee`, `Company`, `SalaryReview` (Lombok + `@RdfType`/`@RdfId`/`@RdfProperty`) |
| `repository` | `EmployeeRepository`, `CompanyRepository`, `SalaryReviewRepository` — thin wrappers over `RdfEntityManager` |
| `service` | `EmployeeService` (validation, raises that leave a record, paging) and `PayrollService` (SPARQL aggregates) |
| `controller` | `CompanyController`, `EmployeeController`, `ReportController` |
| `dto` | one record per file (`EmployeeIn`, `EmployeeOut`, `PayrollRow`, `ApiError`, …); the listing answers with Spring's own `Page`, no page DTO of ours |
| `config` | `GraphRdProperties` (timeouts in application.yml), `GraphRdConfig` (beans, closed by Spring), `SeedData` |


## The endpoints

| Call | What it shows |
|---|---|
| `GET /employees?page=0&size=2&orderBy=salary&desc=true` | paging: `items` plus `total` under the same filters |
| `GET /employees?minAge=30&company=http%3A%2F%2F…` | filters; a company filter is an IRI join condition (`whereRef`) |
| `GET /employees/one?id=…` | one employee, the company joined in by its IRI |
| `POST /employees` | validation: duplicate email and unknown company are 400 |
| `POST /employees/raise?id=…` `{"amount":9000,"reason":"promotion"}` | a raise above 25% is refused; every raise writes a `SalaryReview` |
| `GET /employees/history?id=…` | the raise history of one employee, newest first |
| `GET /reports/payroll` | per company: headcount, total, average, lowest, highest — one GROUP BY on the server |
| `GET /reports/totals` | the whole organization in one row |
| `GET /reports/raise-cost` | what the raises on record have cost (SUM over the review records) |

IRIs contain slashes, so they travel as the `id`/`company` query parameter —
no path encoding games, no server configuration.

## Run

```bash
mvn -f clients/graphrd-client install        # once: the libraries into the local repository
mvn -f clients/graphrd-spring-data install
graphrd serve
mvn spring-boot:run
```

## Environments — the URL is configuration, not code

Development defaults live in `application.yml`; the production profile reads
the same settings from the environment:

```bash
SPRING_PROFILES_ACTIVE=prod GRAPHRD_URL=https://graphrd.internal:7200 GRAPHRD_REPOSITORY=personnel-prod PORT=8080 java -jar target/graphrd-spring-boot-app-1.0.0.jar
```

The client beans follow whichever profile won — pooled connections, the
timeouts of that profile, closed on shutdown.

## The vocabulary is configuration too

The entities carry short names (`@RdfProperty("salary")`, `@RdfType("Employee")`);
the namespace they resolve against is `graphrd.namespace` in the same files —
`GRAPHRD_NAMESPACE` in the environment moves the whole vocabulary (queries
included: `@Query` strings say `PREFIX ex: <{ns}>`), no Java change. Full IRIs
in an annotation still win, for the odd predicate from another schema.

## Tests — the acceptance gate for library changes

The suite has three layers, all booting a throwaway server from
`../../target/release/graphrd` when they need one:

- `controller/*.Test` — `@WebMvcTest` slices with a mocked service: binding,
  status codes, the error shape, RDF content negotiation — no server at all;
- `service/*IT` — `@SpringBootTest` against a live GraphRD: validation rules,
  raises and their records, specification searches, the @Query aggregates
  agreeing with each other, RDF serialization;
- `PersonnelApiIT` — the end-to-end tour over HTTP: seed, paging, filters,
  minted-IRI create, the 400s, raise + history, all three RDF formats, delete.

Any change to `clients/graphrd-client` must pass this suite too, not only the library's
own tests.

Note: build with JDK 17 (`/usr/libexec/java_home -v 17`) — the Microsoft JDK
21.0.12 compiler still trips Lombok's annotation processor.
