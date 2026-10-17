# SQL over the PostgreSQL protocol

With `--pg-port`, GraphRD also speaks the PostgreSQL wire protocol (version 3), so psql, psycopg,
JDBC drivers and BI tools can query repositories in SQL.

```bash
graphrd serve --pg-port 5432
psql -h 127.0.0.1 -p 5432 -d zoo
```

The database name is the repository id. With `--auth`, sign in with the same users and passwords
as over HTTP (cleartext password authentication; use it on a trusted network or a tunnel), and the
same permissions apply: reading needs read access to the repository, `INSERT`, `UPDATE`,
`DELETE` and views need write access.

## Tables

The tables are derived from the data, as for [GraphQL](graphql.md):

- every class is a table, named by its local name (`Person`, matched case-insensitively);
- its columns are `id` (the IRI) and the local names of the predicates its instances use;
- a column with several values for one row shows them joined with `, `.

`SHOW TABLES`, `information_schema.tables`, `information_schema.columns` and
`information_schema.views` list them and the views, so tools can browse the schema.

## Statements

```sql
SELECT name, age FROM Person WHERE age > 30 ORDER BY age DESC LIMIT 10;
SELECT COUNT(*) FROM Person WHERE name LIKE 'A%';
SELECT * FROM Person WHERE city IS NOT NULL OFFSET 20 LIMIT 10;
SELECT id FROM Person WHERE name = 'Ann';
SELECT * FROM sparql('SELECT ?s ?n WHERE { ?s <http://example.org/name> ?n } LIMIT 5');
SELECT version();
```

| Part | Supported |
|---|---|
| select list | `*`, columns (`p.name`), `col AS name`, `COUNT(*)` |
| `WHERE` | conditions joined with `AND`: `=`, `<>` / `!=`, `<`, `<=`, `>`, `>=`, `LIKE` (`%`, `_`), `IS NULL`, `IS NOT NULL` |
| `FROM` | a table or a view, with an alias (`Person p`, `Person AS p`) |
| `JOIN` | `[INNER] JOIN` / `LEFT [OUTER] JOIN t ON a.col = b.col` (several in a row) |
| `ORDER BY` | one column (or a select-list `AS` name), `ASC` / `DESC` |
| `LIMIT`, `OFFSET` | yes |
| `sparql('…')` | any SPARQL SELECT; its variables are the columns |

Both the simple and the extended (prepared statement) protocol work, with text and binary
parameters (`$1`, `$2`, …). `SET`, `BEGIN`, `COMMIT` and similar session statements are accepted and
do nothing.

## Joins

A join matches equal values; a column with several values (a person who works for two companies)
matches each of them, so it gives one row per match. Columns are named `alias.column`; an
unqualified name works when only one of the joined tables has it.

```sql
SELECT p.name AS person, c.name AS company
FROM Person p JOIN Company c ON p.worksFor = c.id
WHERE c.name LIKE 'A%' ORDER BY person;

SELECT d.name, p.name FROM Dog d LEFT JOIN Person p ON d.owner = p.id;
```

## Views

```sql
CREATE VIEW staff AS
  SELECT p.name AS person, p.age, c.name AS employer FROM Person p JOIN Company c ON p.worksFor = c.id;
SELECT person FROM staff WHERE employer = 'Acme' ORDER BY age DESC;
CREATE OR REPLACE VIEW elders AS SELECT name FROM Person WHERE age > 60;
DROP VIEW [IF EXISTS] elders;
```

A view is any `SELECT` above: over a table, a join, another view or `sparql('…')`. It is checked
when created (a view whose query fails is refused) and runs each time it is read. Views are kept in
the repository itself, as `<urn:graphrd:sql-view:NAME> <urn:graphrd:sql> "SELECT …"` in the graph
`urn:graphrd:sql-views`, so backups, time travel and a cluster carry them. Views cannot be written
to, and a view cannot take the name of a table.

## Changing data

Rows are resources, as in [GraphQL mutations](graphql.md#mutations):

```sql
INSERT INTO Person (id, name, age, worksFor)
  VALUES ('http://example.org/dan', 'Dan', 22, 'http://example.org/acme'), ('http://example.org/eve', 'Eve', 29, NULL);
INSERT INTO Person (name) VALUES ('Fay');           -- id made up: http://example.org/person/<hex>
UPDATE Person SET age = 23, email = 'dan@example.org' WHERE name = 'Dan';
UPDATE Person SET worksFor = NULL WHERE age > 60;  -- NULL removes the values
DELETE FROM Person WHERE age >= 65;
```

- `INSERT` names its columns; `id` is the IRI. A row whose id already is a resource of that table
  is an error.
- `UPDATE` replaces the values of the columns it sets (the row's other columns stay); the `WHERE`
  is the same as in `SELECT`. The id cannot be set.
- `DELETE` removes a row's statements and the statements linking to it.
- Values follow what the predicate already holds: a string for a link column is an IRI, a value
  for a typed column gets its datatype (an invalid number is an error). Otherwise strings are
  `xsd:string`, integers `xsd:integer`, decimals `xsd:decimal`, `TRUE`/`FALSE` booleans. A column
  the table does not have yet becomes a predicate in the class's namespace.

Each statement is one transaction: when it fails, or a SHACL repository refuses the result,
nothing of it stays. The tag reports the rows (`INSERT 0 2`, `UPDATE 1`, `DELETE 3`). `BEGIN` /
`COMMIT` do not group statements. In a cluster, write through the leader; it replicates the
change.

## Limits

No `GROUP BY`, `OR`, subqueries or expressions in the select list; join conditions are equalities.
`INSERT` creates rows of tables the data already has (a new class needs SPARQL or a GraphQL
`create` with its class IRI). Use `sparql('…')` for anything beyond these.
