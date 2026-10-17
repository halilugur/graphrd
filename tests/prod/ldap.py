"""LDAP sign-in against a real OpenLDAP (Docker): users and a group are added to a directory, and
GraphRD (`--ldap-url`) checks their passwords by binding as them and maps the group to a grant.

    python3 tests/prod/ldap.py            (needs Docker; pulls osixia/openldap)
"""
import base64, json, os, socket, subprocess, sys, time, urllib.error, urllib.request

sys.path.insert(0, os.path.dirname(__file__))
from common import Server  # noqa: E402

IMAGE = os.environ.get("OPENLDAP_IMAGE", "osixia/openldap:1.5.0")
checks = 0

LDIF = """dn: ou=people,dc=example,dc=org
objectClass: organizationalUnit
ou: people

dn: ou=groups,dc=example,dc=org
objectClass: organizationalUnit
ou: groups

dn: uid=carol,ou=people,dc=example,dc=org
objectClass: inetOrgPerson
uid: carol
cn: Carol
sn: Clark
userPassword: carol-pw

dn: uid=dave,ou=people,dc=example,dc=org
objectClass: inetOrgPerson
uid: dave
cn: Dave
sn: Doe
userPassword: dave-pw

dn: cn=zoo-writers,ou=groups,dc=example,dc=org
objectClass: groupOfNames
cn: zoo-writers
member: uid=carol,ou=people,dc=example,dc=org
"""


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
            return r.status, r.read().decode(), r.headers
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), e.headers


def basic(user, pw):
    return {"Authorization": "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode()}


def main():
    print("=== LDAP sign-in: OpenLDAP")
    port = free_port()
    name = f"graphrd-openldap-{os.getpid()}"
    subprocess.run(["docker", "run", "-d", "--rm", "--name", name, "-p", f"{port}:389", "-e", "LDAP_DOMAIN=example.org", "-e", "LDAP_ADMIN_PASSWORD=admin", IMAGE],
                   check=True, capture_output=True)
    try:
        for _ in range(120):
            r = subprocess.run(["docker", "exec", "-i", name, "ldapadd", "-x", "-H", "ldap://localhost", "-D", "cn=admin,dc=example,dc=org", "-w", "admin"],
                               input=LDIF.encode(), capture_output=True)
            if r.returncode == 0:
                break
            time.sleep(1)
        else:
            check(False, f"OpenLDAP started and took the entries: {r.stderr[-300:]!r}")
        srv = Server(args=["--auth", "admin:local-admin-pw", "--ldap-url", f"ldap://127.0.0.1:{port}", "--ldap-user-dn", "uid={user},ou=people,dc=example,dc=org",
                           "--ldap-group-base", "ou=groups,dc=example,dc=org", "--ldap-group-map", "zoo-writers=WRITE_REPO_zoo",
                           "--ldap-bind-dn", "cn=admin,dc=example,dc=org", "--ldap-bind-password", "admin"]).start()
        check(http("POST", f"{srv.url}/rest/repositories", '{"id":"zoo"}', headers=basic("admin", "local-admin-pw"))[0] == 201, "the local administrator still works")
        st, b, _ = http("POST", f"{srv.url}/repositories/zoo/statements", 'INSERT DATA { <http://e/a> <http://e/b> "c" }', "application/sparql-update", basic("carol", "carol-pw"))
        check(st == 204, f"carol writes through her directory group: {st} {b[:200]}")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers=basic("carol", "carol-pw"))[1] == "1", "carol reads")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers=basic("dave", "dave-pw"))[0] == 403, "dave has no grant")
        st, b, _ = http("GET", f"{srv.url}/rest/security/authenticated-user", headers=basic("dave", "dave-pw"))
        check(st == 200 and "dave" in b, f"dave is signed in: {b[:200]}")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers=basic("carol", "wrong"))[0] == 401, "a wrong password is refused")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers=basic("carol", ""))[0] == 401, "an empty password (an anonymous bind) is refused")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers=basic("carol,ou=people,dc=example,dc=org", "carol-pw"))[0] == 401, "a name cannot reshape the DN")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers=basic("nobody", "x"))[0] == 401, "an unknown user is refused")
        st, b, headers = http("POST", f"{srv.url}/rest/login", json.dumps({"username": "carol", "password": "carol-pw"}))
        check(st == 200, f"carol signs in to the UI: {st}")
        token = headers.get("Authorization")
        check(http("GET", f"{srv.url}/repositories/zoo/size", headers={"Authorization": token})[0] == 200, "with a session token")
        srv.cleanup()
        print(f"--- LDAP sign-in (OpenLDAP): PASS, {checks} checks")
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)


if __name__ == "__main__":
    main()
