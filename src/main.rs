//! GraphRD: a dependency-free RDF database in Rust.

use graphrd::{bench, cli, compare, http, pgwire, raft, server, store};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
GraphRD - an RDF database

USAGE:
  graphrd serve [OPTIONS]

OPTIONS:
  --port <n>          HTTP port (default 7200)
  --host <addr>       Bind address (default 127.0.0.1; there is no authentication)
  --data <dir>        Data directory (default ./graphrd-data)
  --memory            Keep everything in memory: no data directory is used, and
                      repositories, users and saved queries end with the process
  --ui-dir <dir>      Serve another web UI build from this directory
  --cluster-learners <list> Members that receive every change but do not vote (read
                      replicas): id=host:port,...; give every node the same list
  --cluster <list>    Raft cluster members: id=host:port,id=host:port,... (all nodes,
                      including this one); needs --node-id and --cluster-secret
  --node-id <id>      This node's id in --cluster
  --cluster-secret <s> Shared secret for cluster traffic (or GRAPHRD_CLUSTER_SECRET)
  --tls-cert <pem>    Serve HTTPS with this certificate chain (needs --tls-key;
  --tls-key <pem>     available in builds with `--features tls`)
  --pg-port <n>       Also accept SQL over the PostgreSQL wire protocol on this port
                      (database name = repository id)
  --sync <on|off>     fsync every commit before acknowledging it (default on). With off,
                      commits survive a crash of the process but not a power loss.
  --read-only         Serve queries only; every change is refused with 403
  --quiet             Do not log requests and other informational messages
  --cors              Allow cross-origin requests from any web page
  --timeout <s>       Time limit for queries that do not set their own
  --default-repo <id> Repository answered at the standard endpoint /sparql (default:
                      the only repository, or the one named \"default\")
  --service-cache <s> Reuse SERVICE answers for this many seconds (default: no reuse)
  --service-timeout <s> Time limit of one SERVICE call (default 300, within the query's)
  --service-retries <n> Retry a SERVICE call that fails to connect or gets a 5xx answer
  --oidc-issuer <url> Accept access tokens (Bearer JWT, RS256) of this OpenID Connect
                      provider, e.g. https://keycloak.example/realms/main
  --oidc-audience <a> Only tokens for this audience (aud or azp)
  --oidc-roles-claim <path> Claim with the user's roles (default realm_access.roles)
  --oidc-role-map <m> Provider role to GraphRD authorities: role=AUTH[+AUTH],...
                      (roles named like authorities need no mapping)
  --ldap-url <url>    Check passwords of users that are not local with this directory
                      (ldap://host:389), binding as --ldap-user-dn
  --ldap-user-dn <dn> The user's DN with {user}, e.g. uid={user},ou=people,dc=example,dc=org
  --ldap-group-base <dn> Search groups listing the user (member=DN) under this DN
  --ldap-bind-dn <dn> Search groups as this account (with --ldap-bind-password or
                      GRAPHRD_LDAP_BIND_PASSWORD) instead of as the user
  --ldap-group-map <m> Group cn to GraphRD authorities: group=AUTH[+AUTH],...
  --ldap-default-roles <list> Authorities every directory user gets (comma-separated)
  --audit-log <file>  Append one JSON line per change, sign-in and refused request
  --audit-reads       Also log reads in the audit log
  --slow-query <ms>   Log queries and updates slower than this to standard error
                      (one JSON line each; also at /rest/monitor/slow-queries)
  --llm-url <url>     Answer questions in plain language with SPARQL a language
                      model writes: its server (also GRAPHRD_LLM_URL). With only
                      ANTHROPIC_API_KEY or OPENAI_API_KEY set, that provider's API
  --llm-key <key>     The server's API key, if it needs one (GRAPHRD_LLM_API_KEY)
  --llm-model <name>  Model (GRAPHRD_LLM_MODEL; default claude-opus-5-5 / gpt-5
                      at Anthropic / OpenAI)
  --llm-api <api>     anthropic (Messages API) or openai (chat completions, also
                      local servers); default: from the URL or key
  --llm-effort <e>    low, medium, high, xhigh or max
  --llm-timeout <s>   Time limit of one call to the model (default 120)
  --auth <user:pass>  Require this user (HTTP Basic or token from /rest/login)
                      Also read from the GRAPHRD_AUTH environment variable.

Offline tools (work on a data directory without a server; --data defaults to
./graphrd-data, --repo to \"default\"; formats by name: nt nq ttl trig rdf jsonld,
results: json xml csv tsv):
  graphrd load [--data DIR] [--repo ID] [--ruleset R] [--format F] [--graph IRI]
               [--lenient] FILE...
                      Load RDF files (or - for standard input) into a repository,
                      creating it if needed; --lenient skips statements that do not
                      parse (nt/nq: the line, ttl: the statement) and counts them
  graphrd dump [--data DIR] [--repo ID] [--format F] [--graph IRI] [--output FILE]
                      Write the repository's explicit statements (default N-Quads);
                      --graph default for the default graph only
  graphrd query [--data DIR] [--repo ID] [--format F] [--output FILE]
                [--base IRI] [--explain true] [--as-of TIME] QUERY
                      Run a SPARQL query (or --query-file FILE, or standard input);
                      --explain prints the evaluation plan and time to stderr;
                      --as-of runs it on the repository as it was at TIME (dateTime
                      or Unix ms; needs the repository setting historyRetention)
  graphrd update [--data DIR] [--repo ID] [--base IRI] UPDATE
                      Run a SPARQL update (or --update-file FILE)
  graphrd backup [--data DIR] [--incremental] DESTINATION
                      Copy the data directory consistently; the copy is a data
                      directory of its own. --incremental updates an earlier
                      backup in DESTINATION, copying only what changed
  graphrd restore --from BACKUP [--data DIR] [--to-time T]
                      Restore a backup into an empty data directory; --to-time
                      (xsd:dateTime or Unix ms) returns every repository to its
                      state at T, from its change history (historyRetention)
  graphrd optimize [--data DIR]
                      Compact every repository (one run file per index)
  graphrd validate --shapes FILE [--format turtle|json] [--base IRI] [DATA...]
                      Validate RDF files against SHACL shapes (SHACL Core and
                      SHACL-SPARQL) in memory and print the validation report;
                      the shapes file may also be one of the data files
  graphrd convert [--from F] [--to F] [--base IRI] [INPUT] [--output FILE]
                      Convert between RDF formats; --to rdfc writes canonical N-Quads
                      (RDF Dataset Canonicalization, RDFC-1.0), --to rdfc-map the
                      canonical blank node labels as JSON (--hash SHA384 to change
                      the hash function); --from-graph IRI|default keeps one
                      graph of the input

  graphrd bench [--urls http://h:p[,http://h2:p2]] [--persons 100000]
                [--clients 8] [--seconds 5] [--repo bench]
                      Load a synthetic graph into each server and measure load,
                      query latency, concurrent throughput and write latency.
  graphrd compare --name NAME --query URL [--update URL] --load URL
                  [--load-type T] [--graph IRI] [--persons 100000]
                  [--clients 8] [--seconds 10] [--out FILE.json]
                      Benchmark any SPARQL 1.1 store with standard protocols only
                      (26 queries, 4 kinds of update, concurrent reads with and
                      without a writer); results as JSON.
  graphrd compare-report FILE.json...
                      Markdown tables comparing compare results side by side.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => match serve(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Some(
            cmd @ ("load" | "dump" | "query" | "update" | "convert" | "backup" | "restore"
            | "optimize" | "validate"),
        ) => {
            let run = match cmd {
                "load" => cli::load,
                "backup" => cli::backup,
                "restore" => cli::restore,
                "optimize" => cli::optimize,
                "dump" => cli::dump,
                "query" => cli::query,
                "update" => cli::update,
                "validate" => cli::validate,
                _ => cli::convert,
            };
            match run(&args[1..]) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(cmd @ ("compare" | "compare-report")) => {
            let r = if cmd == "compare" {
                compare::run(&args[1..])
            } else {
                compare::report(&args[1..])
            };
            match r {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("bench") => match bench::run(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Some("-V" | "--version" | "version") => {
            println!("graphrd {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("error: unknown command {other:?}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn serve(args: &[String]) -> Result<(), String> {
    let mut port = "7200".to_string();
    let mut host = "127.0.0.1".to_string();
    let mut data = PathBuf::from("graphrd-data");
    let mut ui_dir = None;
    let mut auth = std::env::var("GRAPHRD_AUTH").ok();
    let mut pg_port: Option<String> = None;
    let mut tls_cert: Option<PathBuf> = None;
    let mut tls_key: Option<PathBuf> = None;
    let mut cluster: Option<String> = None;
    let mut learners: Option<String> = None;
    let mut node_id: Option<String> = None;
    let mut cluster_secret = std::env::var("GRAPHRD_CLUSTER_SECRET").ok();
    let (mut read_only, mut cors, mut timeout) = (false, false, None);
    let mut quiet = false;
    let mut default_repo: Option<String> = None;
    let mut memory = false;
    let mut audit_log: Option<PathBuf> = None;
    let mut audit_reads = false;
    let (mut oidc_issuer, mut oidc_audience, mut oidc_roles, mut oidc_map): (
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = (None, None, None, String::new());
    let (mut ldap_url, mut ldap_user_dn, mut ldap_groups, mut ldap_map, mut ldap_roles): (
        Option<String>,
        String,
        Option<String>,
        String,
        String,
    ) = (None, String::new(), None, String::new(), String::new());
    let (mut ldap_bind_dn, mut ldap_bind_pw): (Option<String>, Option<String>) =
        (None, std::env::var("GRAPHRD_LDAP_BIND_PASSWORD").ok());
    let mut llm = graphrd::nlq::Settings::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--port" => port = value()?,
            "--llm-api" => llm.api = Some(value()?),
            "--llm-url" => llm.url = Some(value()?),
            "--llm-key" => llm.key = Some(value()?),
            "--llm-model" => llm.model = Some(value()?),
            "--llm-effort" => llm.effort = Some(value()?),
            "--llm-timeout" => {
                llm.timeout_secs = value()?
                    .parse::<u64>()
                    .map_err(|_| "--llm-timeout expects seconds")?
            }
            "--host" => host = value()?,
            "--data" => data = PathBuf::from(value()?),
            "--ui-dir" => {
                let dir = PathBuf::from(value()?);
                if !dir.join("index.html").is_file() {
                    return Err(format!("{} does not contain index.html", dir.display()));
                }
                ui_dir = Some(dir);
            }
            "--auth" => auth = Some(value()?),
            "--sync" => {
                let on = match value()?.as_str() {
                    "on" => true,
                    "off" => false,
                    v => return Err(format!("--sync expects on or off, not {v}")),
                };
                store::SYNC_COMMITS.store(on, std::sync::atomic::Ordering::Relaxed);
            }
            "--pg-port" => pg_port = Some(value()?),
            "--tls-cert" => tls_cert = Some(PathBuf::from(value()?)),
            "--tls-key" => tls_key = Some(PathBuf::from(value()?)),
            "--cluster" => cluster = Some(value()?),
            "--cluster-learners" => learners = Some(value()?),
            "--node-id" => node_id = Some(value()?),
            "--cluster-secret" => cluster_secret = Some(value()?),
            "--read-only" => read_only = true,
            "--quiet" => quiet = true,
            "--memory" => memory = true,
            "--audit-log" => audit_log = Some(PathBuf::from(value()?)),
            "--audit-reads" => audit_reads = true,
            "--oidc-issuer" => oidc_issuer = Some(value()?),
            "--oidc-audience" => oidc_audience = Some(value()?),
            "--oidc-roles-claim" => oidc_roles = Some(value()?),
            "--oidc-role-map" => oidc_map = value()?,
            "--ldap-url" => ldap_url = Some(value()?),
            "--ldap-user-dn" => ldap_user_dn = value()?,
            "--ldap-group-base" => ldap_groups = Some(value()?),
            "--ldap-group-map" => ldap_map = value()?,
            "--ldap-default-roles" => ldap_roles = value()?,
            "--ldap-bind-dn" => ldap_bind_dn = Some(value()?),
            "--ldap-bind-password" => ldap_bind_pw = Some(value()?),
            "--cors" => cors = true,
            "--timeout" => {
                timeout = Some(
                    value()?
                        .parse::<u64>()
                        .map_err(|_| "--timeout expects seconds")?,
                )
            }
            "--default-repo" => default_repo = Some(value()?),
            "--service-cache" => {
                let v = value()?
                    .parse::<u64>()
                    .map_err(|_| "--service-cache expects seconds")?;
                graphrd::sparql::eval::SERVICE_CACHE_SECS
                    .store(v, std::sync::atomic::Ordering::Relaxed);
            }
            "--service-timeout" => {
                let v = value()?
                    .parse::<u64>()
                    .map_err(|_| "--service-timeout expects seconds")?;
                graphrd::sparql::eval::SERVICE_TIMEOUT_SECS
                    .store(v, std::sync::atomic::Ordering::Relaxed);
            }
            "--service-retries" => {
                let v = value()?
                    .parse::<u64>()
                    .map_err(|_| "--service-retries expects a number")?;
                graphrd::sparql::eval::SERVICE_RETRIES
                    .store(v, std::sync::atomic::Ordering::Relaxed);
            }
            "--slow-query" => {
                let ms = value()?
                    .parse::<u64>()
                    .map_err(|_| "--slow-query expects milliseconds")?;
                graphrd::metrics::SLOW_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
            }
            other => return Err(format!("unknown option {other}\n\n{USAGE}")),
        }
    }
    let auth = match auth.filter(|a| !a.is_empty()) {
        Some(a) => {
            let (u, p) = a.split_once(':').ok_or("--auth expects user:password")?;
            if u.is_empty() || p.is_empty() {
                return Err("--auth expects a non-empty user and password".into());
            }
            Some(server::Auth::new(u, p))
        }
        // OpenID Connect or LDAP alone also turn security on (with no local administrator)
        None if oidc_issuer.is_some() || ldap_url.is_some() => Some(server::Auth::new("", "")),
        None => {
            if host != "127.0.0.1" && host != "localhost" && host != "::1" {
                eprintln!(
                    "warning: listening on {host} without --auth; anyone who can reach it can read and change data"
                );
            }
            None
        }
    };
    let auth = match (auth, &oidc_issuer) {
        (Some(a), Some(issuer)) => {
            eprintln!("OpenID Connect: accepting access tokens of {issuer}");
            Some(a.with_oidc(graphrd::oidc::Provider::new(
                issuer,
                oidc_audience.clone(),
                oidc_roles.clone(),
                &oidc_map,
            )))
        }
        (a, _) => a,
    };
    let auth = match (auth, &ldap_url) {
        (Some(a), Some(url)) => {
            if !ldap_user_dn.contains("{user}") {
                return Err("--ldap-url needs --ldap-user-dn with {user}, e.g. uid={user},ou=people,dc=example,dc=org".into());
            }
            eprintln!("LDAP: users that are not local sign in with {url}");
            let search_as =
                match (&ldap_bind_dn, &ldap_bind_pw) {
                    (Some(dn), Some(pw)) => Some((dn.clone(), pw.clone())),
                    (Some(_), None) => return Err(
                        "--ldap-bind-dn needs --ldap-bind-password (or GRAPHRD_LDAP_BIND_PASSWORD)"
                            .into(),
                    ),
                    _ => None,
                };
            Some(a.with_ldap(graphrd::ldap::Directory::new(
                url,
                &ldap_user_dn,
                ldap_groups.clone(),
                search_as,
                &ldap_map,
                &ldap_roles,
            )))
        }
        (a, _) => a,
    };
    let secured = auth.is_some();
    if quiet {
        server::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if memory && cluster.is_some() {
        return Err(
            "--memory cannot be combined with --cluster (the Raft log lives on disk)".into(),
        );
    }
    let _lock = if memory {
        None
    } else {
        Some(cli::lock_data_dir(&data)?)
    };
    let mut app = if memory {
        server::App::open_memory(ui_dir, auth)
    } else {
        server::App::open(&data, ui_dir, auth)
            .map_err(|e| format!("opening {}: {e}", data.display()))?
    };
    app.read_only = read_only;
    app.cors = cors;
    app.default_timeout = timeout;
    if let Some(path) = &audit_log {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        app.audit = Some(std::sync::Mutex::new(f));
        app.audit_reads = audit_reads;
    }
    if let Some(line) = app.configure_llm(llm)? {
        eprintln!("questions in plain language: {line}");
    }
    graphrd::metrics::start();
    if let Some(id) = &default_repo {
        app.set_default_repo(id);
    }
    if read_only {
        eprintln!("read-only: every change is refused");
    }
    let app = Arc::new(app);
    app.start_maintenance();
    app.start_connectors();
    app.warm_up();
    http::prestart(4);
    if secured {
        eprintln!("authentication enabled");
    }
    if let Some(list) = cluster {
        let me = node_id.ok_or("--cluster needs --node-id")?;
        let secret = cluster_secret
            .filter(|s| !s.is_empty())
            .ok_or("--cluster needs --cluster-secret (or GRAPHRD_CLUSTER_SECRET)")?;
        let mut peers = Vec::new();
        let mut me_voter = None;
        let members = list.split(',').map(|m| (m, true)).chain(
            learners
                .as_deref()
                .unwrap_or("")
                .split(',')
                .map(|m| (m, false)),
        );
        for (member, voter) in members.filter(|(m, _)| !m.trim().is_empty()) {
            let (id, host) = member
                .trim()
                .split_once('=')
                .ok_or(format!("cluster member {member:?} must be id=host:port"))?;
            if id == me {
                me_voter = Some(voter);
            } else {
                peers.push(raft::Peer {
                    id: id.into(),
                    host: host.into(),
                    voter,
                });
            }
        }
        let Some(voter) = me_voter else {
            return Err(format!(
                "--node-id {me} is not listed in --cluster or --cluster-learners"
            ));
        };
        let n = peers.len() + 1;
        let raft = raft::Raft::open(&data.join("cluster"), &me, peers, voter, &secret)
            .map_err(|e| format!("cluster state: {e}"))?;
        app.join_cluster(raft);
        let role = if voter {
            "voting member"
        } else {
            "learner (read replica)"
        };
        eprintln!("cluster node {me}, {role}: {n} members; writes go through the elected leader");
    }
    if let Some(pg) = pg_port {
        let pg_addr = format!("{host}:{pg}");
        let backend: Arc<dyn pgwire::Backend> = app.clone();
        eprintln!("PostgreSQL protocol on {pg_addr} (database = repository id)");
        std::thread::spawn(move || {
            if let Err(e) = pgwire::serve(&pg_addr, backend) {
                eprintln!("PostgreSQL listener on {pg_addr}: {e}");
            }
        });
    }
    let addr = format!("{host}:{port}");
    let handler: http::Handler = Arc::new(move |req| http::compress(req, app.handle(req)));
    match (tls_cert, tls_key) {
        (None, None) => {
            let where_ = if memory {
                "in memory".to_string()
            } else {
                format!("data: {}", data.display())
            };
            eprintln!(
                "GraphRD {} listening on http://{addr}/ ({where_})",
                server::VERSION
            );
            http::serve(&addr, handler).map_err(|e| format!("{addr}: {e}"))
        }
        (Some(cert), Some(key)) => serve_https(&addr, handler, &cert, &key, &data),
        _ => Err("--tls-cert and --tls-key must be given together".into()),
    }
}

#[cfg(feature = "tls")]
fn serve_https(
    addr: &str,
    handler: http::Handler,
    cert: &std::path::Path,
    key: &std::path::Path,
    data: &std::path::Path,
) -> Result<(), String> {
    eprintln!(
        "GraphRD {} listening on https://{addr}/ (data: {})",
        server::VERSION,
        data.display()
    );
    http::serve_tls(addr, handler, cert, key).map_err(|e| format!("{addr}: {e}"))
}

#[cfg(not(feature = "tls"))]
fn serve_https(
    _: &str,
    _: http::Handler,
    _: &std::path::Path,
    _: &std::path::Path,
    _: &std::path::Path,
) -> Result<(), String> {
    Err("this build has no TLS support; rebuild with `cargo build --release --features tls`".into())
}
