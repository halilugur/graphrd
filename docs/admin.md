# Administration

## Users and permissions

Without `--auth` there are no users and every request is allowed (keep such a server on
localhost). `serve --auth admin:secret` (or `GRAPHRD_AUTH=admin:secret`) turns security on: this
user always exists with `ROLE_ADMIN`, and its password is reset to the given one at each start.

| Authority | Allows |
|---|---|
| `ROLE_ADMIN` | everything: users, backup and restore, every repository |
| `ROLE_REPO_MANAGER` | create, configure and delete repositories; read and write all of them |
| `ROLE_USER` | sign in |
| `READ_REPO_<id>` | query repository `<id>` |
| `WRITE_REPO_<id>` | query and change repository `<id>` |
| `READ_REPO_*`, `WRITE_REPO_*` | the same for every repository |

Manage users in the UI (**Users**) or through the API (admin only):

```bash
A='admin:secret'
curl -u $A -X POST localhost:7200/rest/security/users/alice -H 'Content-Type: application/json' \
  -d '{"password":"pw-alice","grantedAuthorities":["ROLE_USER","READ_REPO_zoo"]}'
curl -u $A localhost:7200/rest/security/users                      # list
curl -u $A -X PUT localhost:7200/rest/security/users/alice -H 'Content-Type: application/json' \
  -d '{"grantedAuthorities":["ROLE_USER","WRITE_REPO_zoo"]}'      # change authorities
curl -u $A -X DELETE localhost:7200/rest/security/users/alice
```

Users change their own password with `PATCH /rest/security/users/<own name>` and
`{"password": "…"}`. A password change ends the user's open sessions. The bootstrap administrator
cannot be deleted or lose `ROLE_ADMIN`. Users are stored in `DATA/users.json` (salted, iterated
hashes; see [security.md](security.md)).

Clients authenticate with HTTP Basic, or with a token: `POST /rest/login` with
`{"username", "password"}` answers with `Authorization: Bearer <token>` to send on later requests.

## Backup and restore

| Method | How |
|---|---|
| Running server | `GET /rest/recovery/backup` returns a tar archive of every repository's configuration, namespaces and explicit statements (`{"repositories": ["zoo"]}` in a POST body for some); `POST /rest/recovery/restore` with the archive (multipart `file` or the raw tar) brings them back. Also on the Repositories page. |
| Stopped server | `graphrd backup --data DIR DEST` copies the data directory consistently; the copy is a data directory of its own. `--incremental` updates an earlier backup with what changed since. |
| Point in time | `graphrd restore --from DEST --data NEW --to-time T` restores a backup as it was at T, from the repositories' change history (`historyRetention`); see [cli.md](cli.md#restore). |

```bash
curl -u admin:secret localhost:7200/rest/recovery/backup -o graphrd-backup.tar
curl -u admin:secret -X POST localhost:7200/rest/recovery/restore --data-binary @graphrd-backup.tar
```

Inferred statements are not in archives; they are derived again on restore.

For point-in-time recovery, give repositories a `historyRetention` that covers the window you want
to go back over (for example `7d`), take backups regularly with `--incremental` (cheap: mostly
new run files), and restore the latest backup to the moment you need. `tests/prod/pitr.py` tests
the whole cycle.

## Monitoring

`GET /metrics` returns Prometheus metrics (with `--auth`, any signed-in user may read them; give
Prometheus a user and `basic_auth`):

| Metric | Type |
|---|---|
| `graphrd_query_duration_seconds`, `graphrd_update_duration_seconds` | histograms |
| `graphrd_wal_fsync_duration_seconds` | histogram of write-ahead log fsyncs |
| `graphrd_query_errors_total`, `graphrd_update_errors_total`, `graphrd_slow_queries_total` | counters |
| `graphrd_http_responses_total{code="2xx"}` … | counters by status class |
| `graphrd_statements_loaded_total` | statements added by uploads and imports |
| `graphrd_statements{repository="…"}`, `graphrd_repositories` | gauges |
| `graphrd_resident_memory_bytes` | resident memory (Linux) |
| `graphrd_cluster_leader`, `graphrd_cluster_term`, `graphrd_cluster_commit_index`, `graphrd_cluster_applied_index` | cluster nodes only |
| `graphrd_uptime_seconds`, `graphrd_info{version}` | gauges |

```yaml
# prometheus.yml
scrape_configs:
  - job_name: graphrd
    static_configs: [{ targets: ["graphrd:7200"] }]
    basic_auth: { username: monitor, password: "…" }
```

`serve --slow-query 500` writes every query or update slower than 500 ms to standard error as one
JSON line (`{"slow":"query","repository":…,"ms":…,"text":…}`) and keeps the last 100 for
`GET /rest/monitor/slow-queries`. The UI's **Monitoring** page charts queries and updates per
second, latency percentiles and memory from `/metrics`, and lists slow queries.

Every request to `/repositories` and `/rest` is logged to standard error with its status and time.

## Upkeep

A repository whose writes stop for a few seconds is checkpointed in the background: memtables
become run files and the write-ahead log is emptied, so idle servers stay small and restarts replay
nothing. `graphrd optimize` (server stopped) merges each index into one file.

## Limits for exposed servers

`graphrd::http::set_limits(max_body, max_connections)` (when embedding) bounds request bodies
(default 2 GB) and concurrent connections. `--read-only` refuses every change, `--timeout` bounds
queries, and `--cors` lets web pages on other origins call the API.
