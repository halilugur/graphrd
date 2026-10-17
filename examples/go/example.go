// GraphRD from Go, standard library only. Run with: go run example.go
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
)

var base = func() string {
	if u := os.Getenv("GRAPHRD_URL"); u != "" {
		return u
	}
	return "http://localhost:7200"
}()

const repo = "demo"

var query = `PREFIX ex: <http://example.org/>
SELECT ?friend ?company WHERE {
  ex:alice ex:knows ?f .
  ?f ex:name ?friend .
  OPTIONAL { ?f ex:worksFor ?c . ?c ex:name ?company }
}`

func call(method, path string, body []byte, contentType, accept string) (int, []byte) {
	req, _ := http.NewRequest(method, base+path, bytes.NewReader(body))
	if contentType != "" {
		req.Header.Set("Content-Type", contentType)
	}
	if accept != "" {
		req.Header.Set("Accept", accept)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		panic(err)
	}
	defer resp.Body.Close()
	out, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, out
}

func main() {
	// 1. repository — 409 just means it exists already
	body, _ := json.Marshal(map[string]string{"id": repo})
	status, _ := call("POST", "/rest/repositories", body, "application/json", "")
	if status == 409 {
		fmt.Println("repository already there")
	} else {
		fmt.Println("repository created")
	}

	// 2. load the JSON-LD document
	data := map[string]any{
		"@context": map[string]string{
			"name":     "http://example.org/name",
			"knows":    "http://example.org/knows",
			"worksFor": "http://example.org/worksFor",
			"Person":   "http://example.org/Person",
			"Company":  "http://example.org/Company",
		},
		"@graph": []map[string]any{
			{"@id": "http://example.org/alice", "@type": "Person", "name": "Alice",
				"knows": map[string]string{"@id": "http://example.org/bob"},
				"worksFor": map[string]string{"@id": "http://example.org/acme"}},
			{"@id": "http://example.org/bob", "@type": "Person", "name": "Bob",
				"worksFor": map[string]string{"@id": "http://example.org/acme"}},
			{"@id": "http://example.org/acme", "@type": "Company", "name": "Acme"},
		},
	}
	body, _ = json.Marshal(data)
	status, _ = call("POST", "/repositories/"+repo+"/statements", body, "application/ld+json", "")
	fmt.Printf("loaded JSON-LD (%d)\n", status)

	// 3. SPARQL — results as JSON
	_, out := call("POST", "/repositories/"+repo, []byte(query),
		"application/sparql-query", "application/sparql-results+json")
	var results struct {
		Results struct {
			Bindings []map[string]struct{ Value string } `json:"bindings"`
		} `json:"results"`
	}
	if err := json.Unmarshal(out, &results); err != nil {
		panic(err)
	}
	for _, row := range results.Results.Bindings {
		company := "-"
		if c, ok := row["company"]; ok {
			company = c.Value
		}
		fmt.Printf("%s works at %s\n", row["friend"].Value, company)
	}

	// 4. the graph back as JSON-LD
	_, out = call("GET", "/repositories/"+repo+"/statements", nil, "", "application/ld+json")
	var nodes []map[string]any
	if err := json.Unmarshal(out, &nodes); err != nil {
		panic(err)
	}
	fmt.Printf("%d nodes back as JSON-LD\n", len(nodes))
}
