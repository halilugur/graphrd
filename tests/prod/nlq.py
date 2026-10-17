"""Questions in plain language (serve --llm-api), end to end against a stand-in model server that
speaks the Anthropic Messages API and OpenAI-style chat completions: what GraphRD sends (headers,
model, schema summary, question), the query it hands back, running it, the second attempt after a
query that does not parse, and refusing updates, SERVICE, refusals and upstream errors.

    python3 tests/prod/nlq.py
"""
import json, os, sys, threading, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(__file__))
from common import Server, free_port  # noqa: E402

checks = 0


def check(cond, what):
    global checks
    checks += 1
    if not cond:
        print(f"  FAIL {what}")
        sys.exit(1)


class Mock:
    """Answers each request with the next scripted reply: (status, text, extra fields)."""

    def __init__(self):
        self.replies, self.requests = [], []
        mock = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                mock.requests.append((self.path, dict(self.headers), body))
                status, text, extra = mock.replies.pop(0)
                if self.path == "/v1/messages":
                    out = {"type": "message", "role": "assistant", "stop_reason": "end_turn",
                           "content": [{"type": "thinking", "thinking": "", "signature": "sig"}, {"type": "text", "text": text}]}
                else:
                    out = {"choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": text}}]}
                if status >= 400:
                    out = {"type": "error", "error": {"type": "authentication_error", "message": text}}
                out.update(extra)
                data = json.dumps(out).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.port = free_port()
        self.http = ThreadingHTTPServer(("127.0.0.1", self.port), H)
        threading.Thread(target=self.http.serve_forever, daemon=True).start()

    def reply(self, text, status=200, **extra):
        self.replies.append((status, text, extra))


DATA = """PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
INSERT DATA {
  ex:ali a ex:Person ; ex:name "Ali" ; ex:age 31 ; ex:worksFor ex:acme .
  ex:ayse a ex:Person ; ex:name "Ayse" ; ex:age 45 ; ex:worksFor ex:acme .
  ex:can a ex:Person ; ex:name "Can" ; ex:age 28 .
  ex:acme a ex:Company ; ex:name "ACME" ; ex:founded "1999-05-01"^^xsd:date .
}"""
Q_OVER_30 = "```sparql\nPREFIX ex: <http://example.org/>\nSELECT ?name WHERE { ?p a ex:Person ; ex:name ?name ; ex:age ?a FILTER(?a > 30) } ORDER BY ?name\n```"


def ask(srv, question, run=False):
    st, b = srv.req("POST", "/rest/repositories/r/nl-query", json.dumps({"question": question, "run": run}).encode(), "application/json", timeout=30)
    return st, (json.loads(b) if st == 200 else b.decode())


def setup(args):
    srv = Server(args=["--memory", *args]).start()
    cfg = {"id": "r", "params": {"ruleset": {"value": "empty"}}}
    st, _ = srv.req("POST", "/rest/repositories", json.dumps(cfg).encode(), "application/json")
    check(st in (200, 201), "create repository")
    st, _ = srv.req("POST", "/repositories/r/namespaces/ex", b"http://example.org/", "text/plain")
    st, b = srv.req("POST", "/repositories/r/statements", urllib.parse.urlencode({"update": DATA}).encode(), "application/x-www-form-urlencoded")
    check(st == 204, f"load: {st} {b[:200]!r}")
    return srv


VARS = ("GRAPHRD_LLM_API", "GRAPHRD_LLM_URL", "GRAPHRD_LLM_API_KEY", "GRAPHRD_LLM_MODEL", "GRAPHRD_LLM_EFFORT", "ANTHROPIC_API_KEY", "OPENAI_API_KEY")


def env(**kv):
    for k in VARS:
        os.environ.pop(k, None)
    os.environ.update(kv)


def log(srv):
    with open(srv.log_path, encoding="utf-8", errors="replace") as f:
        return f.read()


def main():
    print("=== questions in plain language")
    env()

    # not configured
    srv = setup([])
    st, b = ask(srv, "who is over 30?")
    check(st == 501 and "AI Assistant" in b and "--llm-url" in b, f"not configured: {st} {b}")
    srv.stop()

    # a provider's key alone turns it on with that provider's API; this build (no tls) cannot reach
    # it over HTTPS, so the server starts with a warning and the feature stays off
    env(OPENAI_API_KEY="sk-test")
    srv = setup([])
    st, b = ask(srv, "who is over 30?")
    if "warning: OPENAI_API_KEY is set" in log(srv):
        check(st == 501, f"provider key without tls: {st} {b}")
    else:
        check("questions in plain language: gpt-5 at https://api.openai.com/v1" in log(srv), log(srv))
    srv.stop()

    # Anthropic Messages API
    env(GRAPHRD_LLM_API_KEY="test-key-123")
    mock = Mock()
    srv = setup(["--llm-api", "anthropic", "--llm-url", f"http://127.0.0.1:{mock.port}", "--llm-effort", "low"])
    mock.reply(Q_OVER_30)
    st, a = ask(srv, "Which people are older than 30?", run=True)
    check(st == 200, f"answer: {st} {a}")
    check("FILTER(?a > 30)" in a["sparql"] and a["attempts"] == 1 and a["model"] == "claude-opus-5-5", a)
    names = [r["name"]["value"] for r in a["results"]["results"]["bindings"]]
    check(names == ["Ali", "Ayse"], f"results: {names}")
    path, headers, body = mock.requests[-1]
    h = {k.lower(): v for k, v in headers.items()}
    check(path == "/v1/messages", path)
    check(h.get("x-api-key") == "test-key-123" and h.get("anthropic-version") == "2023-06-01", h)
    check("anthropic-beta" not in h and "fallbacks" not in body, "no fallback off Anthropic's own API")
    check(body["model"] == "claude-opus-5-5" and body["max_tokens"] == 16000 and body["output_config"] == {"effort": "low"}, body)
    check(body["messages"] == [{"role": "user", "content": "Which people are older than 30?"}], body["messages"])
    schema = body["system"][1]["text"]
    check(body["system"][1]["cache_control"] == {"type": "ephemeral"}, body["system"])
    check("class ex:Person (3 instances)" in schema and "ex:worksFor -> ex:Company" in schema, schema)
    check('ex:founded -> literal xsd:date, e.g. "1999-05-01"' in schema and "PREFIX ex: <http://example.org/>" in schema, schema)
    check("SPARQL" in body["system"][0]["text"], "instructions")

    # without run: the query only
    mock.reply(Q_OVER_30)
    st, a = ask(srv, "older than 30")
    check(st == 200 and "results" not in a, a)

    # a query that does not parse goes back once, with the parser's message and the first answer
    mock.reply("```sparql\nSELECT ?n WHERE { ?p <http://example.org/name> }\n```")
    mock.reply("```sparql\nSELECT (COUNT(*) AS ?n) WHERE { ?p a <http://example.org/Person> }\n```")
    st, a = ask(srv, "How many people are there?", run=True)
    check(st == 200 and a["attempts"] == 2, a)
    check(a["results"]["results"]["bindings"][0]["n"]["value"] == "3", a)
    msgs = mock.requests[-1][2]["messages"]
    check(len(msgs) == 3 and msgs[1]["role"] == "assistant" and msgs[1]["content"][0]["type"] == "thinking", "assistant turn replayed unchanged")
    check("cannot run that query" in msgs[2]["content"], msgs[2])

    # updates and SERVICE are refused, even after the second attempt
    mock.reply("DELETE WHERE { ?s ?p ?o }")
    mock.reply("```sparql\nDELETE WHERE { ?s ?p ?o }\n```")
    st, b = ask(srv, "delete everything")
    check(st == 502 and "update" in b, f"update refused: {st} {b}")
    st, n = srv.req("POST", "/repositories/r", urllib.parse.urlencode({"query": "SELECT (COUNT(*) AS ?c) {?s ?p ?o}"}).encode(), "application/x-www-form-urlencoded", accept="text/csv")
    check(n.decode().split()[1] == "14", f"data unchanged: {n}")
    mock.reply("SELECT * { SERVICE <http://169.254.169.254/latest/> { ?s ?p ?o } }")
    mock.reply("SELECT * { SERVICE <http://169.254.169.254/latest/> { ?s ?p ?o } }")
    st, b = ask(srv, "fetch metadata")
    check(st == 502 and "SERVICE" in b, f"SERVICE refused: {st} {b}")

    # a refusal and an upstream error come back as 502 with the reason
    mock.reply("", stop_reason="refusal", stop_details={"type": "refusal", "category": "cyber", "explanation": "not allowed"})
    st, b = ask(srv, "something")
    check(st == 502 and "declined" in b and "not allowed" in b, f"refusal: {st} {b}")
    mock.reply("invalid x-api-key", status=401)
    st, b = ask(srv, "something")
    check(st == 502 and "401" in b and "invalid x-api-key" in b, f"upstream error: {st} {b}")
    st, b = ask(srv, "   ")
    check(st == 400, f"empty question: {st}")
    srv.stop()

    # OpenAI-compatible chat completions, set up from the environment alone (a local server needs
    # no key, and a provider's key in the environment never goes to it)
    mock2 = Mock()
    env(GRAPHRD_LLM_URL=f"http://127.0.0.1:{mock2.port}/v1", GRAPHRD_LLM_MODEL="qwen3-coder", OPENAI_API_KEY="sk-must-not-leak", ANTHROPIC_API_KEY="sk-ant-must-not-leak")
    srv = setup([])
    check("qwen3-coder at http://127.0.0.1" in log(srv) and "no key" in log(srv), log(srv))
    mock2.reply("Here you go:\n" + Q_OVER_30)
    st, a = ask(srv, "Which people are older than 30?", run=True)
    check(st == 200 and [r["name"]["value"] for r in a["results"]["results"]["bindings"]] == ["Ali", "Ayse"], a)
    path, headers, body = mock2.requests[-1]
    h = {k.lower(): v for k, v in headers.items()}
    check(path == "/v1/chat/completions" and "authorization" not in h, (path, h))
    check(body["model"] == "qwen3-coder" and body["messages"][0]["role"] == "system" and "class ex:Person" in body["messages"][0]["content"], body)
    check(body["messages"][1] == {"role": "user", "content": "Which people are older than 30?"}, body["messages"])
    srv.stop()

    # the endpoint, key and model as options: the key goes as a Bearer token
    env()
    srv = setup(["--llm-url", f"http://127.0.0.1:{mock2.port}/v1", "--llm-key", "local-key", "--llm-model", "llama4"])
    mock2.reply(Q_OVER_30)
    st, a = ask(srv, "older than 30")
    path, headers, body = mock2.requests[-1]
    h = {k.lower(): v for k, v in headers.items()}
    check(st == 200 and h.get("authorization") == "Bearer local-key" and body["model"] == "llama4", (st, h, body.get("model")))
    srv.stop()

    # an endpoint with no model name is refused at start
    srv = Server(args=["--memory", "--llm-url", "http://127.0.0.1:9/v1"])
    try:
        srv.start(timeout=3)
        started = True
    except Exception:
        started = False
    check(not started and "model name" in log(srv), log(srv))
    srv.stop()

    # settings from the UI: /rest/llm is for administrators, saved in the data directory (owner
    # only), in use at once and after a restart; the key is never shown; DELETE goes back
    env()
    import base64, stat, tempfile
    admin = "Basic " + base64.b64encode(b"admin:pw-1").decode()
    work = tempfile.mkdtemp(prefix="graphrd-nlq-ui-")
    srv = Server(data=os.path.join(work, "data"), args=["--auth", "admin:pw-1"]).start()
    st, _ = srv.req("GET", "/rest/llm")
    check(st == 401, f"settings need sign-in: {st}")
    st, _ = srv.req("POST", "/rest/security/users/reader", json.dumps({"password": "pw-2", "grantedAuthorities": ["ROLE_USER", "READ_REPO_*"]}).encode(), "application/json", auth=admin)
    reader = "Basic " + base64.b64encode(b"reader:pw-2").decode()
    st, _ = srv.req("GET", "/rest/llm", auth=reader)
    check(st == 403, f"settings are for administrators: {st}")
    st, b = srv.req("GET", "/rest/llm/status", auth=reader)
    check(st == 200 and json.loads(b) == {"enabled": False, "model": None}, (st, b))
    put = lambda body: srv.req("PUT", "/rest/llm", json.dumps(body).encode(), "application/json", auth=admin)
    st, b = put({"url": f"http://127.0.0.1:{mock2.port}/v1"})
    check(st == 400 and b"model name" in b, f"incomplete settings refused: {st} {b}")
    st, b = put({"url": f"http://127.0.0.1:{mock2.port}/v1", "model": "ui-model", "key": "ui-secret-9", "effort": "high"})
    check(st == 200 and b"ui-secret-9" not in b, b)
    j = json.loads(b)
    check(j["enabled"] and j["active"]["model"] == "ui-model" and j["active"]["keySet"] and j["active"]["source"] == "settings saved in the UI", j)
    path = os.path.join(work, "data", "llm.json")
    check(stat.S_IMODE(os.stat(path).st_mode) == 0o600 and "ui-secret-9" in open(path).read(), "saved, readable by the owner only")
    mock2.reply("ok")
    st, b = srv.req("POST", "/rest/llm/test", auth=admin)
    check(st == 200 and json.loads(b)["ok"] and json.loads(b)["answer"] == "ok", f"test: {b}")
    srv.stop()
    srv.start()
    st, b = srv.req("GET", "/rest/llm/status", auth=reader)
    check(json.loads(b) == {"enabled": True, "model": "ui-model"}, f"kept after a restart: {b}")
    h = {k.lower(): v for k, v in mock2.requests[-1][1].items()}
    check(h.get("authorization") == "Bearer ui-secret-9" and mock2.requests[-1][2].get("reasoning_effort") == "high", h)
    # saving again without a key keeps it; an empty key removes it
    st, b = put({"url": f"http://127.0.0.1:{mock2.port}/v1", "model": "ui-model-2"})
    check(json.loads(b)["active"]["keySet"], "key kept")
    st, b = put({"url": f"http://127.0.0.1:{mock2.port}/v1", "model": "ui-model-2", "key": ""})
    check(not json.loads(b)["active"]["keySet"], "key removed")
    st, b = srv.req("DELETE", "/rest/llm", auth=admin)
    check(st == 200 and not json.loads(b)["enabled"] and not os.path.exists(path), f"reset: {b}")
    srv.stop()

    print(f"  ok: {checks} checks")


if __name__ == "__main__":
    main()
