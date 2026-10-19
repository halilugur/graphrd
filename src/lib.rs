//! GraphRD: a dependency-free RDF database in Rust, usable as a library.
//!
//! ```no_run
//! use graphrd::store::Store;
//! use graphrd::sparql::{self, QueryOptions, QueryResult};
//! use std::collections::HashMap;
//!
//! let mut store = Store::open(std::path::Path::new("data")).unwrap();
//! let prefixes = HashMap::new();
//! sparql::update(&mut store, "INSERT DATA { <http://ex/a> <http://ex/b> 1 }", "", &prefixes).unwrap();
//! let opts = QueryOptions { base: "", prefixes: &prefixes, dataset: None, timeout_secs: None, infer: true, at_query: false };
//! if let QueryResult::Solutions { rows, .. } = sparql::query(&store, "SELECT * { ?s ?p ?o }", &opts).unwrap() {
//!     println!("{} rows", rows.len());
//! }
//! ```
#![warn(missing_docs)]

#[cfg(feature = "tls")]
pub mod acme;
pub mod bench;
pub mod cli;
pub mod compare;
pub mod connector;
pub mod dict;
pub mod fastmap;
pub mod fts;
pub mod geo;
pub mod graphql;
pub mod gzip;
pub mod http;
pub mod json;
pub mod ldap;
pub mod log;
pub mod lsm;
pub mod metrics;
pub mod nlq;
pub mod oidc;
pub mod pgwire;
pub mod raft;
pub mod rdf;
pub mod reason;
pub mod server;
pub mod shacl;
pub mod sparql;
pub mod store;
pub mod tar;
pub mod vector;
