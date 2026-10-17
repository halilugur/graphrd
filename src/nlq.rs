//! Questions in plain language, answered with SPARQL written by a language model.
//!
//! Off unless `serve` is given a model (`--llm-api anthropic|openai`). The model sees a summary of
//! the repository (classes, their properties, value kinds and an example of each, taken from a
//! sample of the data) and the question, and answers with one query. GraphRD parses the query
//! before handing it back; a query that does not parse goes back to the model once, with the
//! parser's message. Only read queries are accepted (no updates, no SERVICE), so a question can
//! never change data or make the server call other hosts.
//!
//! Two wire formats: the Anthropic Messages API (`POST {url}/v1/messages`) and OpenAI-style chat
//! completions (`POST {url}/chat/completions`), which local servers (Ollama, vLLM, LM Studio,
//! llama.cpp) and gateways (LiteLLM, OpenRouter) also speak. HTTPS needs the `tls` build feature.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use crate::http;
use crate::json::Json;
use crate::rdf::Term;
use crate::store::{Id, Store};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const ANTHROPIC_URL: &str = "https://api.anthropic.com";
const OPENAI_URL: &str = "https://api.openai.com/v1";
const ANTHROPIC_MODEL: &str = "claude-opus-5-5";
const OPENAI_MODEL: &str = "gpt-5";
/// Models that accept server-side fallback on a refusal (Anthropic's own API only).
const FALLBACK_MODELS: [&str; 4] = ["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];

#[derive(Clone, Copy, PartialEq, Debug)]
/// Which wire format the model speaks.
pub enum Api {
/// The Anthropic Messages API.
    Anthropic,
/// OpenAI-compatible chat completions (OpenAI, Ollama, vLLM, ...).
    OpenAi,
}

/// A configured language model.
pub struct Llm {
    api: Api,
    url: String,
    key: Option<String>,
/// The model to ask.
    pub model: String,
    effort: Option<String>,
    timeout: Duration,
    /// Where the settings came from, for the server log.
    source: String,
}

/// The model settings as given (command line options or the UI; each falls back to its
/// environment variable): all optional.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Settings {
    /// `--llm-api` / `GRAPHRD_LLM_API`: `anthropic` or `openai`.
    pub api: Option<String>,
    /// `--llm-url` / `GRAPHRD_LLM_URL`.
    pub url: Option<String>,
    /// `--llm-key` / `GRAPHRD_LLM_API_KEY`.
    pub key: Option<String>,
    /// `--llm-model` / `GRAPHRD_LLM_MODEL`.
    pub model: Option<String>,
    /// `--llm-effort` / `GRAPHRD_LLM_EFFORT`.
    pub effort: Option<String>,
/// How long to wait for the model, seconds.
    pub timeout_secs: u64,
}

impl Settings {
    /// From the JSON the UI sends (`api`, `url`, `key`, `model`, `effort`, `timeoutSecs`; an empty
    /// string is unset). A missing `key` keeps `previous_key`.
    pub fn from_json(j: &Json, previous_key: Option<String>) -> Settings {
        let field = |k: &str| j.get(k).and_then(Json::as_str).map(str::trim).filter(|v| !v.is_empty()).map(String::from);
        let api = field("api").filter(|a| a != "auto");
        let key = match j.get("key") {
            None | Some(Json::Null) => previous_key,
            Some(_) => field("key"),
        };
        let timeout_secs = j.get("timeoutSecs").and_then(Json::as_f64).map_or(0, |t| t.max(0.0) as u64);
        Settings { api, url: field("url"), key, model: field("model"), effort: field("effort"), timeout_secs }
    }

    /// For saving (`with_key`) or showing (only whether a key is set).
    pub fn to_json(&self, with_key: bool) -> Json {
        let mut j = Json::obj()
            .with("api", self.api.clone().unwrap_or_else(|| "auto".into()))
            .with("url", self.url.clone())
            .with("model", self.model.clone())
            .with("effort", self.effort.clone())
            .with("timeoutSecs", self.timeout_secs as usize)
            .with("keySet", self.key.is_some());
        if with_key {
            j.set("key", self.key.clone());
        }
        j
    }

    /// The model these settings describe (see `Llm::from_settings`), with the environment.
    pub fn resolve(&self) -> Result<Option<Llm>, String> {
        Llm::from_settings(self)
    }
}

/// What the settings resolve to: (api, url, key, model, where they came from), or `None` when
/// nothing names a model, so the feature stays off.
///
/// - The wire format is `--llm-api` if given; else Anthropic for a URL at anthropic.com or a key
///   starting `sk-ant-` (a URL with "anthropic" anywhere, so Anthropic-compatible endpoints such as
///   z.ai's count too), and OpenAI-style for any other URL or key (local servers, gateways).
/// - With no URL and no GraphRD key, `ANTHROPIC_API_KEY` (or else `OPENAI_API_KEY`) alone turns it
///   on, with that provider's own endpoint and default model.
/// - A provider's key from its environment variable only ever goes to that provider's own
///   endpoint, never to another URL.
fn resolve(given: &Settings, env: &dyn Fn(&str) -> Option<String>) -> Result<Option<(Api, String, Option<String>, String, String)>, String> {
    let api = given.api.clone().or_else(|| env("GRAPHRD_LLM_API"));
    let url = given.url.clone().or_else(|| env("GRAPHRD_LLM_URL")).map(|u| u.trim_end_matches('/').to_string());
    let mut key = given.key.clone().or_else(|| env("GRAPHRD_LLM_API_KEY"));
    let model = given.model.clone().or_else(|| env("GRAPHRD_LLM_MODEL"));
    let mut source = "--llm-* options / GRAPHRD_LLM_* variables".to_string();
    let api = match api.as_deref() {
        Some("anthropic") => Api::Anthropic,
        Some("openai") => Api::OpenAi,
        Some(other) => return Err(format!("--llm-api expects anthropic or openai, not {other}")),
        None => match (&url, &key) {
            (Some(u), _) => {
                // Anthropic's API, or an Anthropic-compatible one (e.g. https://api.z.ai/api/anthropic)
                if u.to_ascii_lowercase().contains("anthropic") { Api::Anthropic } else { Api::OpenAi }
            }
            (None, Some(k)) => {
                if k.starts_with("sk-ant-") { Api::Anthropic } else { Api::OpenAi }
            }
            (None, None) => {
                if let Some(k) = env("ANTHROPIC_API_KEY") {
                    key = Some(k);
                    source = "ANTHROPIC_API_KEY".into();
                    Api::Anthropic
                } else if let Some(k) = env("OPENAI_API_KEY") {
                    key = Some(k);
                    source = "OPENAI_API_KEY".into();
                    Api::OpenAi
                } else if model.is_some() {
                    return Err("--llm-model needs an endpoint (--llm-url) or a key (GRAPHRD_LLM_API_KEY, ANTHROPIC_API_KEY or OPENAI_API_KEY)".into());
                } else {
                    return Ok(None);
                }
            }
        },
    };
    let default_url = if api == Api::Anthropic { ANTHROPIC_URL } else { OPENAI_URL };
    let url = url.unwrap_or_else(|| default_url.to_string());
    let own = url == default_url;
    if key.is_none() && own {
        let var = if api == Api::Anthropic { "ANTHROPIC_API_KEY" } else { "OPENAI_API_KEY" };
        key = env(var);
        if key.is_none() {
            return Err(format!("{url} needs a key: set GRAPHRD_LLM_API_KEY (or {var})"));
        }
    }
    let model = match model {
        Some(m) => m,
        None if api == Api::Anthropic => ANTHROPIC_MODEL.to_string(),
        None if own => OPENAI_MODEL.to_string(),
        None => return Err(format!("{url} needs a model name, as that server knows it (the Model setting, --llm-model or GRAPHRD_LLM_MODEL)")),
    };
    Ok(Some((api, url, key, model, source)))
}

impl Llm {
    /// The configured model, or `None` when no setting (and no provider key in the environment)
    /// names one. A model turned on only by a provider key in the environment that this build
    /// cannot reach (HTTPS without the `tls` feature) stays off with a warning instead of failing.
    pub fn from_settings(given: &Settings) -> Result<Option<Llm>, String> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let Some((api, url, key, model, source)) = resolve(given, &env)? else { return Ok(None) };
        let effort = given.effort.clone().or_else(|| env("GRAPHRD_LLM_EFFORT"));
        if let Some(e) = &effort {
            if !["low", "medium", "high", "xhigh", "max"].contains(&e.as_str()) {
                return Err(format!("--llm-effort expects low, medium, high, xhigh or max, not {e}"));
            }
        }
        if url.starts_with("https://") && !cfg!(feature = "tls") {
            let msg = format!("{url} needs HTTPS, which this build leaves out: build with `cargo build --release --features tls`, or use an http:// endpoint (a local model server or a gateway)");
            if source.ends_with("_API_KEY") && !source.starts_with("--") {
                eprintln!("warning: {source} is set, but questions in plain language stay off: {msg}");
                return Ok(None);
            }
            return Err(msg);
        }
        let timeout = Duration::from_secs(if given.timeout_secs == 0 { 120 } else { given.timeout_secs });
        Ok(Some(Llm { api, url, key, model, effort, timeout, source }))
    }

    /// What is in use, for the UI (no key).
    pub fn info(&self) -> Json {
        Json::obj()
            .with("api", if self.api == Api::Anthropic { "anthropic" } else { "openai" })
            .with("url", self.url.as_str())
            .with("model", self.model.as_str())
            .with("effort", self.effort.clone())
            .with("timeoutSecs", self.timeout.as_secs() as usize)
            .with("keySet", self.key.is_some())
            .with("source", self.source.as_str())
    }

    /// Call the model once with a trivial request: whether the endpoint, key and model work.
    pub fn ping(&self) -> Result<String, String> {
        let turns = [Json::obj().with("role", "user").with("content", "Reply with the single word: ok")];
        self.chat("You check that a connection works.", "", &turns).map(|(t, _)| t.trim().chars().take(200).collect())
    }

    /// Where the settings came from, as shown in the log and the UI.
    pub fn with_source(mut self, source: &str) -> Llm {
        self.source = source.to_string();
        self
    }

    /// One line for the server log (no key).
    pub fn describe(&self) -> String {
        let api = if self.api == Api::Anthropic { "Anthropic Messages API" } else { "OpenAI-compatible chat completions" };
        format!("{} at {} ({api}; {}, from {})", self.model, self.url, if self.key.is_some() { "with a key" } else { "no key" }, self.source)
    }

    fn post(&self, url: &str, headers: Vec<(String, String)>, body: &Json) -> Result<Json, String> {
        let mut headers = headers;
        headers.push(("Content-Type".into(), "application/json".into()));
        let (status, _, resp) = http::request(url, "POST", &headers, body.to_string().as_bytes(), self.timeout)?;
        let text = String::from_utf8_lossy(&resp);
        let json = Json::parse(&text).map_err(|_| format!("the model's server answered HTTP {status} with no JSON: {}", text.chars().take(300).collect::<String>()))?;
        if status >= 400 {
            let msg = json.get("error").map(|e| e.get("message").and_then(Json::as_str).map(str::to_string).unwrap_or_else(|| e.to_string())).unwrap_or_else(|| text.chars().take(300).collect());
            return Err(format!("the model's server answered HTTP {status}: {msg}"));
        }
        Ok(json)
    }

    /// Send `system` and the conversation so far (alternating user/assistant turns, starting with
    /// the user; an assistant turn is the provider's own message content, replayed unchanged).
    /// Returns the answer's text and its content to replay in a following turn.
    fn chat(&self, system: &str, schema: &str, turns: &[Json]) -> Result<(String, Json), String> {
        match self.api {
            Api::Anthropic => {
                // the instructions and the schema stay the same for a repository: cacheable
                let mut blocks = vec![Json::obj().with("type", "text").with("text", system)];
                if !schema.is_empty() {
                    blocks.push(Json::obj().with("type", "text").with("text", schema).with("cache_control", Json::obj().with("type", "ephemeral")));
                }
                let system = Json::Arr(blocks);
                let mut body = Json::obj().with("model", self.model.as_str()).with("max_tokens", 16_000usize).with("system", system).with("messages", Json::Arr(turns.to_vec()));
                if let Some(e) = &self.effort {
                    body.set("output_config", Json::obj().with("effort", e.as_str()));
                }
                let mut headers = vec![("anthropic-version".to_string(), "2023-06-01".to_string())];
                if let Some(k) = &self.key {
                    headers.push(("x-api-key".into(), k.clone()));
                }
                // on Anthropic's own API, a refusal is retried on a suitable model in the same call
                if self.url == ANTHROPIC_URL && FALLBACK_MODELS.contains(&self.model.as_str()) {
                    body.set("fallbacks", "default");
                    headers.push(("anthropic-beta".into(), "server-side-fallback-2026-07-01".into()));
                }
                let resp = self.post(&format!("{}/v1/messages", self.url), headers, &body)?;
                match resp.get("stop_reason").and_then(Json::as_str) {
                    Some("refusal") => {
                        let why = resp.get("stop_details").and_then(|d| d.get("explanation")).and_then(Json::as_str).unwrap_or("no reason given");
                        return Err(format!("the model declined the question ({why})"));
                    }
                    Some("max_tokens") => return Err("the model's answer was cut off (max_tokens)".into()),
                    _ => {}
                }
                let content = resp.get("content").cloned().unwrap_or(Json::Arr(Vec::new()));
                let text: String = content.as_array().unwrap_or(&[]).iter().filter(|b| b.get("type").and_then(Json::as_str) == Some("text")).filter_map(|b| b.get("text").and_then(Json::as_str)).collect();
                Ok((text, content))
            }
            Api::OpenAi => {
                let mut messages = vec![Json::obj().with("role", "system").with("content", if schema.is_empty() { system.to_string() } else { format!("{system}\n\n{schema}") })];
                messages.extend(turns.iter().cloned());
                let mut body = Json::obj().with("model", self.model.as_str()).with("messages", Json::Arr(messages));
                if let Some(e) = &self.effort {
                    body.set("reasoning_effort", e.as_str());
                }
                let headers = self.key.iter().map(|k| ("Authorization".to_string(), format!("Bearer {k}"))).collect();
                let resp = self.post(&format!("{}/chat/completions", self.url), headers, &body)?;
                let choice = resp.get("choices").and_then(Json::as_array).and_then(|c| c.first()).ok_or("the model's server answered with no choices")?;
                if choice.get("finish_reason").and_then(Json::as_str) == Some("length") {
                    return Err("the model's answer was cut off (length)".into());
                }
                let text = choice.get("message").and_then(|m| m.get("content")).and_then(Json::as_str).unwrap_or("").to_string();
                Ok((text.clone(), Json::from(text)))
            }
        }
    }
}

const INSTRUCTIONS: &str = "You translate questions about an RDF knowledge graph into one SPARQL 1.1 query for the GraphRD database.

Answer with the query only, in a ```sparql code block, without explanations.
- Use only the classes and properties in the schema below, by their full IRIs or with PREFIX declarations for every prefix you use.
- Write a SELECT or ASK query (CONSTRUCT or DESCRIBE only when the question asks for a graph). Never an update, never SERVICE.
- Write literals the way the examples are written: the same datatype (for example \"2024-01-01\"^^xsd:date) and language tag.
- Give the variables readable names; for \"how many\" questions use COUNT.
- Unless the question asks for all results, a count or a single answer, end a SELECT with LIMIT 1000.
- If the question cannot be answered from this schema, answer with a query that returns nothing and a comment line saying why.";

/// The SPARQL a model wrote for a question.
pub struct Answer {
/// The SPARQL the model wrote.
    pub sparql: String,
    /// 1, or 2 when the first query did not parse.
    pub attempts: usize,
}

/// Ask `llm` for a query answering `question`, given the repository's `schema` (see
/// `cached_schema`; made before, so no lock on the store is held while the model writes).
pub fn ask(llm: &Llm, schema: &str, question: &str, prefixes: &HashMap<String, String>) -> Result<Answer, String> {
    let question = question.trim();
    if question.is_empty() {
        return Err("the question is empty".into());
    }
    let user = |text: String| Json::obj().with("role", "user").with("content", text);
    let mut turns = vec![user(question.to_string())];
    let mut last_err = String::new();
    for attempt in 1..=2 {
        let (text, content) = llm.chat(INSTRUCTIONS, schema, &turns)?;
        let sparql = extract_sparql(&text);
        match check(&sparql, prefixes) {
            Ok(()) => return Ok(Answer { sparql, attempts: attempt }),
            Err(e) => {
                last_err = e;
                turns.push(Json::obj().with("role", "assistant").with("content", content));
                turns.push(user(format!("GraphRD cannot run that query: {last_err}\nAnswer with a corrected query only.")));
            }
        }
    }
    Err(format!("the model did not write a usable query: {last_err}"))
}

/// The query in a model's answer: the first ```sparql (or plain ```) block, else the whole text.
pub fn extract_sparql(text: &str) -> String {
    let mut rest = text;
    while let Some(i) = rest.find("```") {
        let after = &rest[i + 3..];
        let (lang, body) = after.split_once('\n').unwrap_or(("", after));
        if let Some(end) = body.find("```") {
            let lang = lang.trim().to_ascii_lowercase();
            if lang.is_empty() || lang == "sparql" || lang == "rq" {
                return body[..end].trim().to_string();
            }
            rest = &body[end + 3..];
        } else {
            break;
        }
    }
    text.trim().to_string()
}

/// A read query that parses, with no SERVICE (which would make the server call other hosts).
pub fn check(sparql: &str, prefixes: &HashMap<String, String>) -> Result<(), String> {
    if sparql.is_empty() {
        return Err("the answer holds no query".into());
    }
    if has_word(sparql, "SERVICE") {
        return Err("SERVICE is not allowed here; query this repository only".into());
    }
    match crate::sparql::parser::parse_query(sparql, "", prefixes) {
        Ok(_) => Ok(()),
        Err(e) if crate::sparql::parser::parse_update(sparql, "", prefixes).is_ok() => Err(format!("that is an update; only queries are allowed ({e})")),
        Err(e) => Err(e),
    }
}

/// Whether `word` appears as a keyword (outside IRIs, strings and comments).
fn has_word(text: &str, word: &str) -> bool {
    let b = text.as_bytes();
    let (mut i, mut quote, mut iri) = (0, None::<u8>, false);
    while i < b.len() {
        let c = b[i];
        match (quote, iri) {
            (Some(q), _) => {
                if c == b'\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
            }
            (None, true) => iri = c != b'>',
            (None, false) => match c {
                b'"' | b'\'' => quote = Some(c),
                b'<' if b.get(i + 1).is_some_and(|n| !n.is_ascii_whitespace() && *n != b'=') => iri = true,
                b'#' => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
                _ if text[i..].len() >= word.len()
                    && text.is_char_boundary(i + word.len())
                    && text[i..i + word.len()].eq_ignore_ascii_case(word)
                    && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b':' || b[i - 1] == b'?' || b[i - 1] == b'$'))
                    && !b.get(i + word.len()).is_some_and(|n| n.is_ascii_alphanumeric() || *n == b'_' || *n == b':') =>
                {
                    return true;
                }
                _ => {}
            },
        }
        i += 1;
    }
    false
}

// ------------------------------------------------------------------ schema summary

/// Classes in the summary at most, by size.
const MAX_CLASSES: usize = 60;
/// Instances of a class sampled for its properties.
const SAMPLE: usize = 25;

static SCHEMAS: Mutex<Option<HashMap<String, (usize, String)>>> = Mutex::new(None);

/// The summary of `repo` (see `schema`), made again when its size changes.
pub fn cached_schema(store: &Store, repo: &str, prefixes: &HashMap<String, String>) -> String {
    let size = store.len() + store.inferred_len();
    let mut cache = SCHEMAS.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);
    if let Some((n, s)) = map.get(repo) {
        if *n == size {
            return s.clone();
        }
    }
    let s = schema(store, prefixes);
    map.insert(repo.to_string(), (size, s.clone()));
    s
}

/// What a property's values are, from the sample.
#[derive(Default)]
struct Values {
    /// classes of linked resources
    classes: BTreeMap<String, ()>,
    resources: bool,
    /// datatype or `@lang` -> an example
    literals: BTreeMap<String, String>,
}

/// A text summary of the data for the model: per class, its size and its properties with the
/// kinds of their values and an example literal.
pub fn schema(store: &Store, prefixes: &HashMap<String, String>) -> String {
    let mut ns: Vec<(String, String)> = prefixes.iter().map(|(p, n)| (p.clone(), n.clone())).collect();
    for (p, n) in [("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"), ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"), ("xsd", "http://www.w3.org/2001/XMLSchema#"), ("owl", "http://www.w3.org/2002/07/owl#")] {
        if !ns.iter().any(|(q, _)| q == p) {
            ns.push((p.into(), n.into()));
        }
    }
    // longest namespace first, so the most specific prefix wins
    ns.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    let mut used: BTreeMap<String, String> = BTreeMap::new();
    let mut short = |iri: &str| -> String {
        for (p, n) in &ns {
            if let Some(local) = iri.strip_prefix(n.as_str()) {
                if !local.is_empty() && local.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.') && !local.ends_with('.') {
                    used.insert(p.clone(), n.clone());
                    return format!("{p}:{local}");
                }
            }
        }
        format!("<{iri}>")
    };
    let iri_of = |id: Id| match store.dict.term(id) {
        Term::Iri(i) => Some(i),
        _ => None,
    };
    let mut body = String::new();
    let ty = store.dict.lookup(&Term::iri(RDF_TYPE));
    let mut classes: Vec<(usize, Id)> = match ty {
        // the index estimate is coarse for small classes: those are counted
        Some(ty) => store
            .distinct_objects(ty)
            .into_iter()
            .take(5_000)
            .map(|c| match store.estimate(None, Some(ty), Some(c), None) {
                n if n <= 100_000 => (store.quads_matching(None, Some(ty), Some(c), None).count(), c),
                n => (n, c),
            })
            .collect(),
        None => Vec::new(),
    };
    classes.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let more = classes.len().saturating_sub(MAX_CLASSES);
    classes.truncate(MAX_CLASSES);
    let describe = |props: &BTreeMap<Id, Values>, short: &mut dyn FnMut(&str) -> String, body: &mut String| {
        for (p, v) in props {
            let Some(piri) = iri_of(*p) else { continue };
            let mut kinds: Vec<String> = v.classes.keys().map(|c| short(c)).collect();
            if kinds.is_empty() && v.resources {
                kinds.push("resource".into());
            }
            for (k, example) in &v.literals {
                let k = if k.starts_with('@') { format!("literal{k}") } else { format!("literal {}", short(k)) };
                kinds.push(format!("{k}, e.g. \"{example}\""));
            }
            body.push_str(&format!("  {} -> {}\n", short(&piri), kinds.join(" | ")));
        }
    };
    let sample_props = |subjects: &mut dyn Iterator<Item = Id>| -> BTreeMap<Id, Values> {
        let mut props: BTreeMap<Id, Values> = BTreeMap::new();
        for s in subjects {
            for q in store.quads_matching(Some(s), None, None, None).take(200) {
                if Some(q[1]) == ty {
                    continue;
                }
                let v = props.entry(q[1]).or_default();
                match store.dict.term(q[2]) {
                    Term::Literal(l) => {
                        let key = match &l.lang {
                            Some(lang) => format!("@{lang}"),
                            None => l.datatype.clone(),
                        };
                        v.literals.entry(key).or_insert_with(|| {
                            let mut e: String = l.value.chars().take(40).collect();
                            if l.value.chars().count() > 40 {
                                e.push('…');
                            }
                            e.replace('"', "'").replace('\n', " ")
                        });
                    }
                    _ => {
                        v.resources = true;
                        if let Some(ty) = ty {
                            for t in store.quads_matching(Some(q[2]), Some(ty), None, None).take(2) {
                                if let Some(c) = iri_of(t[2]) {
                                    v.classes.insert(c, ());
                                }
                            }
                        }
                    }
                }
            }
        }
        props
    };
    if let Some(ty) = ty {
        for &(n, c) in &classes {
            let Some(ciri) = iri_of(c) else { continue };
            body.push_str(&format!("class {} ({} instances)\n", short(&ciri), if n > 100_000 { format!("about {n}") } else { n.to_string() }));
            let props = sample_props(&mut store.quads_matching(None, Some(ty), Some(c), None).take(SAMPLE).map(|q| q[0]));
            describe(&props, &mut short, &mut body);
        }
        if more > 0 {
            body.push_str(&format!("({more} smaller classes not listed)\n"));
        }
    }
    if classes.is_empty() {
        // untyped data: properties of the first subjects
        body.push_str("(no rdf:type statements; properties of a sample of subjects)\n");
        let mut seen = Vec::new();
        for q in store.quads_matching(None, None, None, None).take(5_000) {
            if !seen.contains(&q[0]) {
                seen.push(q[0]);
            }
            if seen.len() >= 200 {
                break;
            }
        }
        let props = sample_props(&mut seen.into_iter());
        describe(&props, &mut short, &mut body);
    }
    let mut out = String::from("Schema of the repository (from a sample of the data):\n\n");
    for (p, n) in &used {
        out.push_str(&format!("PREFIX {p}: <{n}>\n"));
    }
    out.push('\n');
    out.push_str(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(url: Option<&str>, key: Option<&str>, model: Option<&str>, api: Option<&str>, vars: &[(&str, &str)]) -> Result<Option<(Api, String, Option<String>, String)>, String> {
        let given = Settings { api: api.map(Into::into), url: url.map(Into::into), key: key.map(Into::into), model: model.map(Into::into), ..Default::default() };
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let env = move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        resolve(&given, &env).map(|r| r.map(|(a, u, k, m, _)| (a, u, k, m)))
    }

    #[test]
    fn settings_resolve_to_an_endpoint() {
        let some = |a, u: &str, k: Option<&str>, m: &str| Ok(Some((a, u.to_string(), k.map(String::from), m.to_string())));
        // nothing set: off
        assert_eq!(settings(None, None, None, None, &[]), Ok(None));
        // a provider's key alone: its own endpoint and default model (Anthropic first when both)
        assert_eq!(settings(None, None, None, None, &[("ANTHROPIC_API_KEY", "sk-ant-1")]), some(Api::Anthropic, ANTHROPIC_URL, Some("sk-ant-1"), ANTHROPIC_MODEL));
        assert_eq!(settings(None, None, None, None, &[("OPENAI_API_KEY", "sk-o")]), some(Api::OpenAi, OPENAI_URL, Some("sk-o"), OPENAI_MODEL));
        assert_eq!(settings(None, None, None, None, &[("OPENAI_API_KEY", "sk-o"), ("ANTHROPIC_API_KEY", "sk-ant-1")]).unwrap().unwrap().0, Api::Anthropic);
        assert_eq!(settings(None, None, Some("o3"), None, &[("OPENAI_API_KEY", "sk-o")]), some(Api::OpenAi, OPENAI_URL, Some("sk-o"), "o3"));
        // GraphRD's own key: the provider from its form
        assert_eq!(settings(None, None, None, None, &[("GRAPHRD_LLM_API_KEY", "sk-ant-2")]), some(Api::Anthropic, ANTHROPIC_URL, Some("sk-ant-2"), ANTHROPIC_MODEL));
        assert_eq!(settings(None, Some("sk-x"), None, None, &[]), some(Api::OpenAi, OPENAI_URL, Some("sk-x"), OPENAI_MODEL));
        // an endpoint: OpenAI-style unless at anthropic.com; another server needs a model name
        let local = "http://127.0.0.1:11434/v1";
        assert_eq!(settings(Some("http://127.0.0.1:11434/v1/"), None, Some("qwen3"), None, &[]), some(Api::OpenAi, local, None, "qwen3"));
        assert!(settings(Some(local), None, None, None, &[]).unwrap_err().contains("model name"));
        assert_eq!(settings(Some("https://api.anthropic.com"), None, None, None, &[("ANTHROPIC_API_KEY", "sk-ant-1")]), some(Api::Anthropic, ANTHROPIC_URL, Some("sk-ant-1"), ANTHROPIC_MODEL));
        assert_eq!(settings(Some("http://gw:4000"), None, Some("claude-x"), Some("anthropic"), &[]), some(Api::Anthropic, "http://gw:4000", None, "claude-x"));
        // an Anthropic-compatible endpoint elsewhere: Anthropic's format, its own key, a model name
        assert_eq!(settings(Some("https://api.z.ai/api/anthropic"), Some("zk"), Some("glm-4.6"), None, &[("ANTHROPIC_API_KEY", "sk-ant-1")]), some(Api::Anthropic, "https://api.z.ai/api/anthropic", Some("zk"), "glm-4.6"));
        // the environment works like the options
        let vars = [("GRAPHRD_LLM_URL", local), ("GRAPHRD_LLM_MODEL", "qwen3"), ("GRAPHRD_LLM_API_KEY", "k")];
        assert_eq!(settings(None, None, None, None, &vars), some(Api::OpenAi, local, Some("k"), "qwen3"));
        // a provider's key never goes to another server
        assert_eq!(settings(Some(local), None, Some("qwen3"), None, &[("OPENAI_API_KEY", "sk-o"), ("ANTHROPIC_API_KEY", "sk-ant-1")]), some(Api::OpenAi, local, None, "qwen3"));
        // incomplete or wrong
        assert!(settings(None, None, None, Some("anthropic"), &[("OPENAI_API_KEY", "sk-o")]).unwrap_err().contains("needs a key"));
        assert!(settings(None, None, Some("qwen3"), None, &[]).unwrap_err().contains("endpoint"));
        assert!(settings(None, None, None, Some("gemini"), &[]).is_err());
    }

    #[test]
    fn settings_from_the_ui() {
        let j = Json::parse(r#"{"api": "auto", "url": " http://h:1/v1 ", "model": "m", "effort": "", "timeoutSecs": 30}"#).unwrap();
        let s = Settings::from_json(&j, Some("old".into()));
        assert_eq!(s, Settings { api: None, url: Some("http://h:1/v1".into()), key: Some("old".into()), model: Some("m".into()), effort: None, timeout_secs: 30 });
        // an empty key removes it; a new one replaces it
        assert_eq!(Settings::from_json(&Json::parse(r#"{"key": ""}"#).unwrap(), Some("old".into())).key, None);
        assert_eq!(Settings::from_json(&Json::parse(r#"{"key": "new"}"#).unwrap(), Some("old".into())).key, Some("new".into()));
        // shown without the key, saved with it
        let shown = s.to_json(false).to_string();
        assert!(!shown.contains("old") && shown.contains("\"keySet\":true"), "{shown}");
        assert_eq!(Settings::from_json(&s.to_json(true), None), s);
    }

    #[test]
    fn finds_the_query_in_an_answer() {
        assert_eq!(extract_sparql("Here:\n```sparql\nSELECT * { ?s ?p ?o }\n```\nDone."), "SELECT * { ?s ?p ?o }");
        assert_eq!(extract_sparql("```python\nx = 1\n```\n```\nASK {}\n```"), "ASK {}");
        assert_eq!(extract_sparql("  SELECT * {}  "), "SELECT * {}");
    }

    #[test]
    fn accepts_read_queries_only() {
        let pf = HashMap::new();
        assert!(check("SELECT * { ?s ?p ?o } LIMIT 3", &pf).is_ok());
        assert!(check("SELECT * { ?s <http://e/service> \"SERVICE\" } # SERVICE", &pf).is_ok());
        assert!(check("SELECT ?service { ?service ?p ?o }", &pf).is_ok());
        assert!(check("SELECT * { SERVICE <http://169.254.169.254/> { ?s ?p ?o } }", &pf).unwrap_err().contains("SERVICE"));
        assert!(check("select * { service <http://x/> { ?s ?p ?o } }", &pf).is_err());
        assert!(check("DELETE WHERE { ?s ?p ?o }", &pf).unwrap_err().contains("update"));
        assert!(check("SELECT * { ?s ?p }", &pf).is_err());
        assert!(check("", &pf).is_err());
    }

    #[test]
    fn summarizes_classes_and_properties() {
        let mut st = Store::memory();
        crate::sparql::update(
            &mut st,
            "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
             INSERT DATA { ex:p1 a ex:Person ; ex:name \"Ayse\" ; ex:born \"1990-01-02\"^^xsd:date ; ex:worksFor ex:c1 .
                           ex:p2 a ex:Person ; ex:name \"Can\"@tr ; ex:worksFor ex:c1 .
                           ex:c1 a ex:Company ; ex:label \"ACME\" }",
            "",
            &HashMap::new(),
        )
        .unwrap();
        let pf: HashMap<String, String> = [("ex".to_string(), "http://example.org/".to_string())].into();
        let s = schema(&st, &pf);
        assert!(s.contains("PREFIX ex: <http://example.org/>"), "{s}");
        assert!(s.contains("class ex:Person (2 instances)"), "{s}");
        assert!(s.contains("ex:worksFor -> ex:Company"), "{s}");
        assert!(s.contains("ex:born -> literal xsd:date, e.g. \"1990-01-02\""), "{s}");
        assert!(s.contains("literal@tr, e.g. \"Can\""), "{s}");
        assert!(s.find("ex:Person").unwrap() < s.find("class ex:Company").unwrap(), "larger classes first: {s}");
    }
}
