# AI queries — questions in plain language

GraphRD can answer a question written in plain language ("Which people over 30 work for ACME?")
by having a language model write the SPARQL query, checking that query, and running it. It is off
until the server is given a model. GraphRD talks to either:

- the **Anthropic Messages API** (`--llm-api anthropic`): Claude models at Anthropic or through a
  compatible gateway;
- **OpenAI-style chat completions** (`--llm-api openai`): OpenAI, and the many servers and gateways
  that speak the same protocol: Ollama, vLLM, LM Studio, llama.cpp's server, LiteLLM, OpenRouter.

Nothing is sent anywhere until a question is asked.

## Setting it up

Three settings: the **endpoint**, the **API key** (if the server needs one) and the **model**.
Each is a `serve` option or an environment variable:

| Option | Variable | Default | Meaning |
|---|---|---|---|
| `--llm-url URL` | `GRAPHRD_LLM_URL` | see below | the model's server; for Anthropic GraphRD adds `/v1/messages`, for OpenAI-style servers `/chat/completions` (so give the URL up to `/v1`) |
| `--llm-key KEY` | `GRAPHRD_LLM_API_KEY` | none | the server's API key: `x-api-key` for Anthropic, `Authorization: Bearer` for OpenAI-style servers |
| `--llm-model NAME` | `GRAPHRD_LLM_MODEL` | `claude-opus-5-5` at Anthropic, `gpt-5` at OpenAI; required for other servers | the model, as the server names it |
| `--llm-api anthropic\|openai` | `GRAPHRD_LLM_API` | from the URL or key | the wire format |
| `--llm-effort E` | `GRAPHRD_LLM_EFFORT` | the model's default | `low`, `medium`, `high`, `xhigh` or `max`: how hard the model thinks (`output_config.effort` for Anthropic, `reasoning_effort` for OpenAI-style servers) |
| `--llm-timeout S` | | 120 | time limit of one call to the model |

How they combine:

- **Only a provider's key**: with `ANTHROPIC_API_KEY` (or `OPENAI_API_KEY`) in the environment and
  no endpoint, GraphRD uses that provider's own API (`https://api.anthropic.com`,
  `https://api.openai.com/v1`) and its default model. When both are set, Anthropic is used;
  `--llm-api` or `--llm-url` chooses otherwise.
- **An endpoint**: the wire format is Anthropic's for a URL with `anthropic` in it (Anthropic's
  own, or an Anthropic-compatible one such as `https://api.z.ai/api/anthropic`) and OpenAI-style for
  any other (local servers and gateways speak it); `--llm-api` overrides that. A server other than
  the two providers' own needs `--llm-model`.
- **A key in `GRAPHRD_LLM_API_KEY` without an endpoint**: a key starting `sk-ant-` means Anthropic,
  any other OpenAI.
- `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` are only ever sent to their provider's own API: with
  another endpoint, give its key with `--llm-key` or `GRAPHRD_LLM_API_KEY`.

```bash
# Claude at Anthropic: the key alone is enough (HTTPS: build with --features tls)
export ANTHROPIC_API_KEY=sk-ant-...
graphrd serve

# OpenAI with another model
export OPENAI_API_KEY=sk-...
graphrd serve --llm-model gpt-5-mini

# a model on this machine through Ollama's OpenAI-compatible endpoint (no key, plain HTTP)
graphrd serve --llm-url http://127.0.0.1:11434/v1 --llm-model qwen3-coder

# a gateway that needs its own key
GRAPHRD_LLM_API_KEY=gw-... graphrd serve --llm-url http://gateway:4000/v1 --llm-model claude-opus-5-5
```

### In the web UI

Administrators (everyone, when the server runs without `--auth`) have an **AI Assistant** page:
the endpoint, key, model, protocol, effort and time limit, a **Test** button that calls the model
once, and **Reset** to go back to the startup settings. Saved settings are in use at once and
after a restart; they take precedence over the command-line options and environment. They are
kept in `llm.json` in the data directory, which only the server's user can read (with the key in
it: back it up and protect it like the rest of the data directory). The key is never sent back
to the browser. With `--memory` they last until the server stops. In a cluster, each server has
its own settings.

With a model in use, the SPARQL page shows **Ask AI** (marked with sparkles): GraphRD AI answers,
showing that it is writing while the model works, then typing the query into the editor (at once
when the system asks for less motion), which keeps a "Written by GraphRD AI" tag until it is
edited. **Write query** stops there, **Ask and run** also runs it. The model's name appears only
on the AI Assistant page.

The same through HTTP (administrators):

| Request | Meaning |
|---|---|
| `GET /rest/llm` | `{"enabled", "active", "saved", "startup", "persistent"}`: what is in use (`api`, `url`, `model`, `effort`, `timeoutSecs`, `keySet`, `source`), what is saved and the startup settings; never a key |
| `PUT /rest/llm` | save and apply `{"api", "url", "key", "model", "effort", "timeoutSecs"}` (empty or missing fields are unset; a missing `key` keeps the saved one, `""` removes it); 400 when the settings are incomplete |
| `DELETE /rest/llm` | remove the saved settings: back to the startup ones |
| `POST /rest/llm/test` | call the model once: `{"ok", "model", "answer", "ms"}` or `{"ok": false, "error"}`; with a JSON body, tries those settings without saving them |
| `GET /rest/llm/status` | (any signed-in user) `{"enabled", "model"}` |

The server log says which model, endpoint and source it uses (never the key). `--llm-key` shows
in process lists; prefer `GRAPHRD_LLM_API_KEY` where that matters.

HTTPS endpoints need a build with `--features tls` (see [security.md](security.md)); the default
build, which has no third-party crates, reaches `http://` endpoints such as a model server on the
same machine or a gateway in the same network. In a build without TLS, a provider's key alone in
the environment leaves the feature off with a warning rather than stopping the server.

On Anthropic's own API with a model that supports it (Claude Fable 5.1, Opus 5.5, Opus 5 and
Sonnet 5.5), GraphRD asks for server-side fallback: if the model declines a question, the API
answers it with another suitable model in the same call.

## Asking

`POST /rest/repositories/{id}/nl-query` with a JSON body (needs read access to the repository):

```bash
curl -s -X POST localhost:7200/rest/repositories/demo/nl-query \
  -H 'Content-Type: application/json' \
  -d '{"question": "Which people are older than 30?", "run": true}'
```

```json
{
  "question": "Which people are older than 30?",
  "sparql": "PREFIX ex: <http://example.org/>\nSELECT ?name WHERE { ?p a ex:Person ; ex:name ?name ; ex:age ?a FILTER(?a > 30) } ORDER BY ?name LIMIT 1000",
  "model": "claude-opus-5-5",
  "attempts": 1,
  "results": { "head": { "vars": ["name"] }, "results": { "bindings": [ ... ] } }
}
```

- `run` (default `false`): also run the query and include its results (SPARQL JSON results) as
  `results`, or the error as `error`. Without it you get the query to look at, change or run
  yourself. Query parameters of the request (`timeout`, `infer`, `as-of`...) apply to the run.
- `attempts`: 2 when the first query did not parse and the model corrected it.

Errors: 400 for a missing question, 501 when no model is configured, 502 when the model's server
fails, declines the question or does not write a usable query (the message says which).

## What the model sees, and what it may write

With each question the model gets instructions and a summary of the repository, made from a
sample of the data: the classes (largest first, at most 60) with their number of instances, and
for each class the properties its instances use, with the kinds of values (linked classes,
literal datatypes and language tags) and an example literal:

```
PREFIX ex: <http://example.org/>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>

class ex:Person (20224 instances)
  ex:name -> literal xsd:string, e.g. "Ceren Polat 0"
  ex:age -> literal xsd:integer, e.g. "45"
  ex:worksFor -> ex:Company
class ex:Post (about 100352 instances)
  ex:createdOn -> literal xsd:date, e.g. "2023-12-28"
```

The summary uses the repository's namespaces as prefixes, is made once and again when the
repository's size changes, and is sent as a cacheable block to Anthropic-style servers. Example
values come from the data, so whoever asks questions sees through the model what a read could
show them anyway; do not configure a model at a provider your data must not reach.

GraphRD then checks the answer before using it:

- it must parse as a SPARQL **query**: updates are refused, so a question can never change data;
- it must not use `SERVICE`, so a question cannot make the server call other hosts;
- a query that does not parse goes back to the model once, with the parser's message.

The query runs like any other, with the asker's permissions, time limit and inference settings.

## How well it works

`tests/prod/nlq_eval.py` measures it: 20 questions over the comparison data (people, companies,
cities, posts, tags), each checked against a reference query by its results.

```bash
ANTHROPIC_API_KEY=... python3 tests/prod/nlq_eval.py --llm-api anthropic
python3 tests/prod/nlq_eval.py --llm-url http://127.0.0.1:11434/v1 --llm-model qwen3-coder
```

Each run calls the model about 20 times, at the provider's prices. `tests/prod/nlq.py` checks the
protocol side (requests, retries, refusals, errors) against a stand-in server, without a model.
