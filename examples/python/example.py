"""GraphRD from Python, standard library only.

Creates a repository, loads a JSON-LD document, runs a SPARQL query and reads
the graph back — against `graphrd serve` on localhost:7200
(or the URL in GRAPHRD_URL).

    python3 example.py
"""
import json
import os
import urllib.error
import urllib.request

BASE = os.environ.get("GRAPHRD_URL", "http://localhost:7200")
REPO = "demo"

DATA = {
    "@context": {
        "name": "http://example.org/name",
        "knows": "http://example.org/knows",
        "worksFor": "http://example.org/worksFor",
        "Person": "http://example.org/Person",
        "Company": "http://example.org/Company",
    },
    "@graph": [
        {"@id": "http://example.org/alice", "@type": "Person", "name": "Alice",
         "knows": {"@id": "http://example.org/bob"}, "worksFor": {"@id": "http://example.org/acme"}},
        {"@id": "http://example.org/bob", "@type": "Person", "name": "Bob",
         "worksFor": {"@id": "http://example.org/acme"}},
        {"@id": "http://example.org/acme", "@type": "Company", "name": "Acme"},
    ],
}

QUERY = """PREFIX ex: <http://example.org/>
SELECT ?friend ?company WHERE {
  ex:alice ex:knows ?f .
  ?f ex:name ?friend .
  OPTIONAL { ?f ex:worksFor ?c . ?c ex:name ?company }
}"""


def call(method, path, body=None, content_type=None, accept=None):
    req = urllib.request.Request(BASE + path, data=body, method=method)
    if content_type:
        req.add_header("Content-Type", content_type)
    if accept:
        req.add_header("Accept", accept)
    with urllib.request.urlopen(req) as resp:
        return resp.status, resp.read().decode()


def main():
    # 1. repository (409 = already exists, which is fine)
    try:
        call("POST", "/rest/repositories",
             json.dumps({"id": REPO}).encode(), "application/json")
        print("repository created")
    except urllib.error.HTTPError as e:
        if e.code != 409:
            raise
        print("repository already there")

    # 2. load the JSON-LD document
    status, _ = call("POST", f"/repositories/{REPO}/statements",
                     json.dumps(DATA).encode(), "application/ld+json")
    print(f"loaded JSON-LD ({status})")

    # 3. SPARQL — results as JSON
    _, body = call("POST", f"/repositories/{REPO}", QUERY.encode(),
                   "application/sparql-query", "application/sparql-results+json")
    for row in json.loads(body)["results"]["bindings"]:
        friend = row["friend"]["value"]
        company = row.get("company", {}).get("value", "-")
        print(f"{friend} works at {company}")

    # 4. the graph back as JSON-LD
    _, doc = call("GET", f"/repositories/{REPO}/statements", accept="application/ld+json")
    nodes = json.loads(doc)
    print(f"{len(nodes)} nodes back as JSON-LD")


if __name__ == "__main__":
    main()
