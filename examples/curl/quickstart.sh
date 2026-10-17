#!/bin/sh
# GraphRD in five commands: create a repository, load JSON-LD, query with SPARQL,
# read the graph back, and join in SQL. Run against `graphrd serve` on localhost:7200.
set -e
cd "$(dirname "$0")"

BASE=${GRAPHRD_URL:-http://localhost:7200}
REPO=demo
SQL_PORT=${GRAPHRD_SQL_PORT:-5432}

echo "== 1. create the repository (a 409 just means it already exists)"
curl -s -o /dev/null -w '%{http_code}\n' -X POST "$BASE/rest/repositories" \
  -H 'Content-Type: application/json' -d '{"id":"'"$REPO"'"}'

echo "== 2. load JSON-LD"
curl -s -o /dev/null -w '%{http_code}\n' -X POST "$BASE/repositories/$REPO/statements" \
  -H 'Content-Type: application/ld+json' --data-binary @data.json

echo "== 3. who does Alice know, and where do they work?"
curl -s -X POST "$BASE/repositories/$REPO" \
  -H 'Content-Type: application/sparql-query' -H 'Accept: text/csv' --data-binary @query.rq

echo "== 4. the same graph back as JSON-LD"
curl -s "$BASE/repositories/$REPO/statements" -H 'Accept: application/ld+json'

echo
echo "== 5. the same question as a SQL join (graphrd serve --pg-port $SQL_PORT)"
echo 'SELECT p.name AS person, c.name AS employer FROM Person p JOIN Company c ON p.worksFor = c.id;' |
  psql -h 127.0.0.1 -p "$SQL_PORT" -d "$REPO" 2>/dev/null ||
  echo '(skipped: psql not available or --pg-port not enabled)'
