# Security

## Defaults

- The server binds to `127.0.0.1`. Binding elsewhere without `--auth` prints a warning: anyone who
  can reach the port could read and change data.
- With `--auth`, every endpoint except `/protocol`, `/rest/login`, `/rest/logout`,
  `/rest/info/version` and the UI's files needs a signed-in user with the right permission
  ([admin.md](admin.md#users-and-permissions)). Requests without one get `401`, requests beyond the
  user's rights `403`.

## Authentication

- **HTTP Basic** (`Authorization: Basic …`), checked on each request, or
- **Tokens**: `POST /rest/login` returns `Authorization: Bearer <token>`; tokens expire after 12 hours
  and end at logout or when the password changes.
- **API keys** (`X-API-Key: grd_…` or `Authorization: ApiKey grd_…`) for programs: an
  administrator makes one for a user (`POST /rest/security/apikeys {"user": "ci", "name": "build"}`);
  the key is shown once, only a digest of it is kept, and it has its user's rights.
- **OpenID Connect** access tokens (`Authorization: Bearer <JWT>`) from an identity provider such as
  Keycloak, Entra ID, Okta or Auth0, with `--oidc-issuer` (below).
- **LDAP**: users that are not local sign in with their directory password, with `--ldap-url`
  (below).
- The PostgreSQL port uses cleartext password authentication with the same users; use it on a
  trusted network or behind TLS termination.

### OpenID Connect

```bash
graphrd serve --auth admin:change-me --oidc-issuer https://keycloak.example/realms/main \
  --oidc-audience graphrd-api --oidc-role-map "graph-editors=WRITE_REPO_zoo,graph-admins=ROLE_ADMIN"
```

GraphRD finds the provider's signing keys through discovery (`/.well-known/openid-configuration`,
then its JWKS; fetched again when a token names a new key) and accepts a token when its RS256
signature verifies, its issuer is `--oidc-issuer`, `--oidc-audience` is in `aud` or is `azp`, and
it has not expired. The user name is `preferred_username` (else `sub`); the roles are read from
`--oidc-roles-claim` (default `realm_access.roles`, Keycloak's) and mapped with
`--oidc-role-map role=AUTHORITY[+AUTHORITY],...`; roles already named like authorities
(`ROLE_ADMIN`, `READ_REPO_zoo`...) need no mapping. Every token user gets `ROLE_USER`. Without
`--auth`, OpenID Connect alone turns security on, with no local administrator. The signature check
is GraphRD's own RSA code (no crypto library); keys below 2048 bits are refused.
`tests/prod/keycloak.py` runs the whole flow against a Keycloak in Docker.

### LDAP

```bash
graphrd serve --auth admin:change-me --ldap-url ldap://ldap.example:389 \
  --ldap-user-dn "uid={user},ou=people,dc=example,dc=org" \
  --ldap-group-base "ou=groups,dc=example,dc=org" --ldap-group-map "zoo-writers=WRITE_REPO_zoo" \
  --ldap-bind-dn "cn=reader,dc=example,dc=org" --ldap-bind-password "$PW"
```

A user that is not local signs in by binding to the directory as `--ldap-user-dn` (the name is
escaped, so it cannot change the DN; empty passwords are refused, since they would be anonymous
binds). Their authorities: `--ldap-default-roles`, plus those of the groups under
`--ldap-group-base` that list them as a `member`, mapped with `--ldap-group-map` (or named like
authorities). Groups are searched as the user, or as `--ldap-bind-dn` where users cannot read
them; when they cannot be read, the user signs in with the default roles only. A directory
sign-in is trusted for five minutes before the password is checked with the directory again;
session tokens from `/rest/login` work for directory users too. The directory is reached over plain
`ldap://`: keep it on a trusted network. `tests/prod/ldap.py` runs against OpenLDAP in Docker.

Passwords are stored as salted SHA-256 iterated 10,000 times (GraphRD has no third-party crypto
library in its default build). Comparisons do not leak timing.

**Lockout**: after 10 wrong passwords for a user, that user is locked for 15 minutes, even with the
right password; this stops online guessing.

The UI handles `401` with its own sign-in form; it never triggers the browser's Basic prompt.
Scripted requests (`fetch`, XHR) get no `WWW-Authenticate` challenge either.

## Audit log

`serve --audit-log /var/log/graphrd/audit.jsonl` appends one JSON line for every change (with the
SPARQL update text, up to 2,000 characters), sign-in and sign-out, and refused request (401, 403);
`--audit-reads` adds reads. Each line has `time` (Unix ms), `user` (for a refused request, the name
it claimed), `method`, `path`, `repository`, `status`, `ms` and `client` (from
`X-Forwarded-For`).

```json
{"time":1792140000000,"user":"ci","method":"POST","path":"/repositories/zoo/statements","repository":"zoo","status":204,"ms":3,"client":null,"update":"INSERT DATA { ... }"}
```

## Transport

Build with `--features tls` and start with `--tls-cert cert.pem --tls-key key.pem` to serve HTTPS
(rustls with the ring provider). Otherwise terminate TLS in a reverse proxy. The `tls` build also
fetches `https://` URLs (LOAD, imports, SERVICE, JSON-LD contexts), trusting the Mozilla roots plus
any CA in `SSL_CERT_FILE`. The library contains an ACME client (`graphrd::acme`) for Let's Encrypt
certificates.

Cluster traffic between nodes is authenticated with the shared `--cluster-secret`; run it on a
private network or behind TLS.

## Hostile input

| Input | Guard |
|---|---|
| HTTP | header size capped at 64 KB, body size and connection count bounded (`--max-body`, `--max-connections`), slow clients time out after 120 s |
| Queries | parser nesting and chain-length limits; regular expressions run in linear time (no catastrophic backtracking); `timeout` per request |
| Turtle, JSON | nesting limits, so deep documents cannot exhaust the stack |
| RDF/XML | entity expansion capped (no "billion laughs"), nesting limited, no external entities fetched |
| Repository ids, import file names | restricted to safe characters; server-side imports read only `DATA/import/` |
| Panics | a request that panics is answered with `500`; the server keeps running |

The production test suite (`tests/prod/robustness.py`) runs protocol abuse, slow clients, deeply
nested input and 2,300 fuzzed requests against a real server, including the PostgreSQL port.

## Supply chain

The default build has no third-party crates: the whole server, from HTTP to SPARQL, is in this
repository. Only the optional `tls` feature adds rustls and its dependencies. The container image
is distroless and runs as an unprivileged user.

## Reporting

Report vulnerabilities privately to the maintainers rather than in a public issue.
