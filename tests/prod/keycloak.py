"""OpenID Connect against a real Keycloak (Docker): a realm with a confidential client, a role and
two users is set up through Keycloak's admin API; access tokens from it are then used against
GraphRD (`--oidc-issuer`), which maps the role to a repository grant.

    python3 tests/prod/keycloak.py            (needs Docker; pulls quay.io/keycloak/keycloak)
"""
import json, os, socket, subprocess, sys, tempfile, time, urllib.error, urllib.parse, urllib.request

sys.path.insert(0, os.path.dirname(__file__))
from common import Server  # noqa: E402

IMAGE = os.environ.get("KEYCLOAK_IMAGE", "quay.io/keycloak/keycloak:26.0")
checks = 0


def check(cond, what):
    global checks
    checks += 1
    if not cond:
        print(f"  FAIL {what}")
        sys.exit(1)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def http(method, url, body=None, ctype="application/json", headers=None):
    h = dict(headers or {})
    if body is not None:
        h["Content-Type"] = ctype
    req = urllib.request.Request(url, data=body.encode() if isinstance(body, str) else body, method=method, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def main():
    print("=== OpenID Connect: Keycloak")
    port = free_port()
    kc = f"http://127.0.0.1:{port}"
    name = f"graphrd-keycloak-{os.getpid()}"
    subprocess.run(["docker", "run", "-d", "--rm", "--name", name, "-p", f"{port}:8080", "-e", "KC_BOOTSTRAP_ADMIN_USERNAME=admin",
                    "-e", "KC_BOOTSTRAP_ADMIN_PASSWORD=admin", IMAGE, "start-dev"], check=True, capture_output=True)
    try:
        for _ in range(240):
            try:
                if http("GET", f"{kc}/realms/master")[0] == 200:
                    break
            except Exception:
                pass
            time.sleep(1)
        else:
            check(False, "Keycloak started")

        def form(d):
            return urllib.parse.urlencode(d)
        st, b = http("POST", f"{kc}/realms/master/protocol/openid-connect/token", form({"grant_type": "password", "client_id": "admin-cli", "username": "admin", "password": "admin"}), "application/x-www-form-urlencoded")
        check(st == 200, f"admin token: {st} {b[:200]}")
        admin = {"Authorization": "Bearer " + json.loads(b)["access_token"]}
        api = f"{kc}/admin/realms"
        check(http("POST", api, json.dumps({"realm": "graphrd", "enabled": True}), headers=admin)[0] == 201, "create realm")
        client = {"clientId": "graphrd-api", "publicClient": False, "secret": "client-s3cret", "directAccessGrantsEnabled": True, "standardFlowEnabled": False}
        check(http("POST", f"{api}/graphrd/clients", json.dumps(client), headers=admin)[0] == 201, "create client")
        check(http("POST", f"{api}/graphrd/roles", json.dumps({"name": "zoo-writer"}), headers=admin)[0] == 201, "create role")
        for user in ["alice", "bob"]:
            u = {"username": user, "enabled": True, "email": f"{user}@example.org", "emailVerified": True, "firstName": user.title(), "lastName": "Test",
                 "credentials": [{"type": "password", "value": f"{user}-pw", "temporary": False}]}
            check(http("POST", f"{api}/graphrd/users", json.dumps(u), headers=admin)[0] == 201, f"create {user}")
        uid = json.loads(http("GET", f"{api}/graphrd/users?username=alice&exact=true", headers=admin)[1])[0]["id"]
        role = json.loads(http("GET", f"{api}/graphrd/roles/zoo-writer", headers=admin)[1])
        check(http("POST", f"{api}/graphrd/users/{uid}/role-mappings/realm", json.dumps([role]), headers=admin)[0] == 204, "grant the role to alice")

        def token(user):
            st, b = http("POST", f"{kc}/realms/graphrd/protocol/openid-connect/token",
                         form({"grant_type": "password", "client_id": "graphrd-api", "client_secret": "client-s3cret", "username": user, "password": f"{user}-pw", "scope": "openid"}),
                         "application/x-www-form-urlencoded")
            check(st == 200, f"token for {user}: {st} {b[:300]}")
            return json.loads(b)["access_token"]

        audit = os.path.join(tempfile.mkdtemp(prefix="graphrd-oidc-"), "audit.jsonl")
        srv = Server(args=["--auth", "admin:local-admin-pw", "--oidc-issuer", f"{kc}/realms/graphrd", "--oidc-audience", "graphrd-api",
                           "--oidc-role-map", "zoo-writer=WRITE_REPO_zoo", "--audit-log", audit]).start()
        basic = {"Authorization": "Basic " + __import__("base64").b64encode(b"admin:local-admin-pw").decode()}
        check(http("POST", f"{srv.url}/rest/repositories", '{"id":"zoo"}', headers=basic)[0] == 201, "the local administrator creates a repository")
        alice, bob = token("alice"), token("bob")
        bearer = lambda t: {"Authorization": f"Bearer {t}"}  # noqa: E731
        st, b = http("POST", f"{srv.url}/repositories/zoo/statements", 'INSERT DATA { <http://e/a> <http://e/b> "c" }', "application/sparql-update", bearer(alice))
        check(st == 204, f"alice writes with her Keycloak token: {st} {b[:200]}")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers=bearer(alice))[1] == "1", "alice reads")
        check(http("GET", f"{srv.url}/rest/security/users", headers=bearer(alice))[0] == 403, "alice is no administrator")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers=bearer(bob))[0] == 403, "bob has no grant on zoo")
        st, b = http("GET", f"{srv.url}/rest/security/authenticated-user", headers=bearer(bob))
        check(st == 200 and "bob" in b, f"bob is signed in: {b[:200]}")
        head, payload, sig = alice.split(".")
        claims = json.loads(__import__("base64").urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
        claims["realm_access"] = {"roles": ["ROLE_ADMIN"]}
        forged_payload = __import__("base64").urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
        check(http("GET", f"{srv.url}/rest/security/users", headers=bearer(f"{head}.{forged_payload}.{sig}"))[0] == 401, "a token with changed claims is refused")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers=bearer(alice[:-4] + "AAAA"))[0] == 401, "a token with a changed signature is refused")
        lines = [json.loads(l) for l in open(audit)]
        check(any(l["user"] == "alice" and l["path"] == "/repositories/zoo/statements" and l["status"] == 204 for l in lines), "the audit log names alice")
        srv.cleanup()
        print(f"--- OpenID Connect (Keycloak): PASS, {checks} checks")
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)


if __name__ == "__main__":
    main()
