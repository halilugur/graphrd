# Quarkus + GraphRD

A Quarkus application that talks to GraphRD two ways:

- **MicroProfile REST client** (`GraphrdApi`) — the repository protocol:
  create the repository, load `data.json` as JSON-LD, run a SPARQL SELECT;
- **JDBC** (`AgroalDataSource`) — GraphRD's PostgreSQL wire protocol: the same
  question as a SQL join (`Person` and `Company` are tables because the
  classes are).

## Run

```bash
graphrd serve --pg-port 5432
mvn quarkus:dev
```

## Environments — the URL is configuration, not code

Development defaults are local; in the prod profile there are no fallbacks —
the environment must say where the server is:

```bash
GRAPHRD_URL=https://graphrd.internal:7200 GRAPHRD_SQL_URL=jdbc:postgresql://graphrd.internal:5432/demo GRAPHRD_REPOSITORY=demo java -Dquarkus.profile=prod -jar target/quarkus-app/quarkus-run.jar
```

Then open <http://localhost:8080/demo> — one line per step:

```
[repository created, loaded JSON-LD, SPARQL answer: {"head":{"vars":["friend", ...} ...},
 SQL join over the same data:,   Alice works at Acme,   Bob works at Acme]
```

## Where to look

| File | What it shows |
|---|---|
| `src/main/java/com/example/graphrd/GraphrdApi.java` | the repository protocol as a typed REST client |
| `src/main/java/com/example/graphrd/DemoResource.java` | the whole flow, incl. the SQL join |
| `src/main/resources/application.properties` | endpoints: the REST client URL and the JDBC URL (the database name is the repository id) |
