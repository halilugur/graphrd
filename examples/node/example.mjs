// GraphRD from Node.js (18+, fetch is built in). Run with: node example.mjs
const BASE = process.env.GRAPHRD_URL ?? "http://localhost:7200";
const REPO = "demo";

const data = {
  "@context": {
    name: "http://example.org/name",
    knows: "http://example.org/knows",
    worksFor: "http://example.org/worksFor",
    Person: "http://example.org/Person",
    Company: "http://example.org/Company",
  },
  "@graph": [
    { "@id": "http://example.org/alice", "@type": "Person", name: "Alice",
      knows: { "@id": "http://example.org/bob" }, worksFor: { "@id": "http://example.org/acme" } },
    { "@id": "http://example.org/bob", "@type": "Person", name: "Bob",
      worksFor: { "@id": "http://example.org/acme" } },
    { "@id": "http://example.org/acme", "@type": "Company", name: "Acme" },
  ],
};

const query = `PREFIX ex: <http://example.org/>
SELECT ?friend ?company WHERE {
  ex:alice ex:knows ?f .
  ?f ex:name ?friend .
  OPTIONAL { ?f ex:worksFor ?c . ?c ex:name ?company }
}`;

// 1. repository — 409 just means it exists already
let resp = await fetch(`${BASE}/rest/repositories`, {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ id: REPO }),
});
console.log(resp.status === 409 ? "repository already there" : "repository created");

// 2. load the JSON-LD document
resp = await fetch(`${BASE}/repositories/${REPO}/statements`, {
  method: "POST",
  headers: { "Content-Type": "application/ld+json" },
  body: JSON.stringify(data),
});
console.log(`loaded JSON-LD (${resp.status})`);

// 3. SPARQL — results as JSON
resp = await fetch(`${BASE}/repositories/${REPO}`, {
  method: "POST",
  headers: { "Content-Type": "application/sparql-query", "Accept": "application/sparql-results+json" },
  body: query,
});
const results = await resp.json();
for (const row of results.results.bindings) {
  console.log(`${row.friend.value} works at ${row.company?.value ?? "-"}`);
}

// 4. the graph back as JSON-LD
resp = await fetch(`${BASE}/repositories/${REPO}/statements`, {
  headers: { "Accept": "application/ld+json" },
});
const nodes = await resp.json();
console.log(`${nodes.length} nodes back as JSON-LD`);
