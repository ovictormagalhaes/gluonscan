//! # gluonscan-testing
//!
//! Contract-based mock transports. A **contract** is a recorded pair: a [`Match`] (a filter over
//! the outgoing request) and the response to reply with. You save the contracts (in code or as
//! JSON files), declare the filters, and get deterministic, offline, testable results — the same
//! model for HTTP/GraphQL ([`MockHttp`]), on-chain RPC ([`MockChainProvider`]), and, later, gRPC.
//!
//! A request that matches no contract is an **error**, so tests fail loudly on unexpected calls.
//!
//! ```
//! use gluonscan_testing::{MockHttp, Match};
//! let http = MockHttp::new()
//!     .on(Match::body_contains("userSupplies"), r#"{"data":{"userSupplies":[]}}"#);
//! ```

use std::sync::Mutex;

use async_trait::async_trait;
use gluonscan_core::{Chain, ChainProvider, Clock, Error, Http, Timestamp};
use serde::Deserialize;

/// A filter over an outgoing request. `primary` is the URL (HTTP) or the method (RPC); `body` is
/// the request body (HTTP) or the params (RPC). Compose with [`Match::all`] / [`Match::any_of`].
#[derive(Debug, Clone)]
pub enum Match {
    /// Matches anything.
    Any,
    /// `primary` contains this substring (URL substring, or partial method).
    PrimaryContains(String),
    /// `primary` equals this exactly (e.g. an RPC method name).
    PrimaryIs(String),
    /// The body contains this substring (e.g. a GraphQL field name).
    BodyContains(String),
    /// The body parses as JSON and the value at `pointer` equals `value`.
    JsonEq {
        /// A JSON Pointer (RFC 6901), e.g. `/variables/user`.
        pointer: String,
        /// The expected value at that pointer.
        value: serde_json::Value,
    },
    /// All of the inner matches hold.
    All(Vec<Match>),
    /// At least one of the inner matches holds.
    AnyOf(Vec<Match>),
}

impl Match {
    /// `primary` contains `s`.
    pub fn primary_contains(s: impl Into<String>) -> Match {
        Match::PrimaryContains(s.into())
    }
    /// `primary` equals `s` (e.g. an RPC method).
    pub fn method(s: impl Into<String>) -> Match {
        Match::PrimaryIs(s.into())
    }
    /// Body contains `s`.
    pub fn body_contains(s: impl Into<String>) -> Match {
        Match::BodyContains(s.into())
    }
    /// Body JSON at `pointer` equals `value`.
    pub fn json_eq(pointer: impl Into<String>, value: serde_json::Value) -> Match {
        Match::JsonEq {
            pointer: pointer.into(),
            value,
        }
    }
    /// All must hold.
    pub fn all(m: impl IntoIterator<Item = Match>) -> Match {
        Match::All(m.into_iter().collect())
    }
    /// Any may hold.
    pub fn any_of(m: impl IntoIterator<Item = Match>) -> Match {
        Match::AnyOf(m.into_iter().collect())
    }

    /// Whether this filter matches the given request.
    pub fn matches(&self, primary: &str, body: &str) -> bool {
        match self {
            Match::Any => true,
            Match::PrimaryContains(s) => primary.contains(s.as_str()),
            Match::PrimaryIs(s) => primary == s,
            Match::BodyContains(s) => body.contains(s.as_str()),
            Match::JsonEq { pointer, value } => serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.pointer(pointer).cloned())
                .is_some_and(|found| &found == value),
            Match::All(ms) => ms.iter().all(|m| m.matches(primary, body)),
            Match::AnyOf(ms) => ms.iter().any(|m| m.matches(primary, body)),
        }
    }
}

/// A single recorded contract: when the filter matches, reply with `reply`.
#[derive(Debug, Clone)]
pub struct Contract {
    /// The request filter.
    pub when: Match,
    /// The recorded response body.
    pub reply: String,
}

/// A JSON-authorable contract file (`{ "body_contains": "...", "reply": "..." }`). Present fields
/// are combined with AND. Use `reply` inline or `reply_path` to point at a sibling response file.
#[derive(Debug, Clone, Deserialize)]
pub struct ContractSpec {
    /// `primary` (URL/method) contains this.
    #[serde(default)]
    pub primary_contains: Option<String>,
    /// `primary` equals this.
    #[serde(default)]
    pub method: Option<String>,
    /// Body contains this.
    #[serde(default)]
    pub body_contains: Option<String>,
    /// Inline recorded response.
    #[serde(default)]
    pub reply: Option<String>,
    /// Path (relative to the spec file) of the recorded response.
    #[serde(default)]
    pub reply_path: Option<String>,
}

impl ContractSpec {
    /// Resolve into a [`Contract`], reading `reply_path` relative to `base_dir` if used.
    pub fn into_contract(self, base_dir: &std::path::Path) -> std::io::Result<Contract> {
        let mut ms = Vec::new();
        if let Some(s) = self.primary_contains {
            ms.push(Match::PrimaryContains(s));
        }
        if let Some(s) = self.method {
            ms.push(Match::PrimaryIs(s));
        }
        if let Some(s) = self.body_contains {
            ms.push(Match::BodyContains(s));
        }
        let when = if ms.is_empty() {
            Match::Any
        } else {
            Match::All(ms)
        };
        let reply = match (self.reply, self.reply_path) {
            (Some(r), _) => r,
            (None, Some(p)) => std::fs::read_to_string(base_dir.join(p))?,
            (None, None) => String::new(),
        };
        Ok(Contract { when, reply })
    }
}

/// Load every `*.json` contract spec in a directory into contracts.
pub fn load_contracts(dir: impl AsRef<std::path::Path>) -> std::io::Result<Vec<Contract>> {
    let dir = dir.as_ref();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let spec: ContractSpec = serde_json::from_str(&std::fs::read_to_string(&path)?)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            out.push(spec.into_contract(dir)?);
        }
    }
    Ok(out)
}

fn no_match(primary: &str, body: &str) -> Error {
    let preview: String = body.chars().take(200).collect();
    Error::Permanent {
        message: format!("no contract matched request to `{primary}` with body: {preview}"),
    }
}

/// A [`Http`] mock that replays contracts and records the calls it received.
#[derive(Default)]
pub struct MockHttp {
    contracts: Vec<Contract>,
    calls: Mutex<Vec<(String, String)>>,
}

impl MockHttp {
    /// An empty mock (every request will error until you add contracts).
    pub fn new() -> Self {
        MockHttp::default()
    }
    /// Add a contract (builder style).
    pub fn on(mut self, when: Match, reply: impl Into<String>) -> Self {
        self.contracts.push(Contract {
            when,
            reply: reply.into(),
        });
        self
    }
    /// Build from a pre-assembled contract set.
    pub fn from_contracts(contracts: Vec<Contract>) -> Self {
        MockHttp {
            contracts,
            calls: Mutex::new(Vec::new()),
        }
    }
    /// The (url, body) pairs seen so far — for assertions.
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Http for MockHttp {
    async fn post(&self, url: &str, body: String) -> Result<String, Error> {
        self.calls
            .lock()
            .unwrap()
            .push((url.to_string(), body.clone()));
        self.contracts
            .iter()
            .find(|c| c.when.matches(url, &body))
            .map(|c| c.reply.clone())
            .ok_or_else(|| no_match(url, &body))
    }
}

/// A [`ChainProvider`] mock that replays contracts (matched on method + params) for on-chain reads.
#[derive(Default)]
pub struct MockChainProvider {
    contracts: Vec<Contract>,
    calls: Mutex<Vec<(String, String)>>,
}

impl MockChainProvider {
    /// An empty mock.
    pub fn new() -> Self {
        MockChainProvider::default()
    }
    /// Add a contract; `when` matches against `(method, params)`.
    pub fn on(mut self, when: Match, reply: impl Into<String>) -> Self {
        self.contracts.push(Contract {
            when,
            reply: reply.into(),
        });
        self
    }
    /// The (method, params) pairs seen so far.
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChainProvider for MockChainProvider {
    async fn call(&self, _chain: Chain, method: &str, params: String) -> Result<String, Error> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        self.contracts
            .iter()
            .find(|c| c.when.matches(method, &params))
            .map(|c| c.reply.clone())
            .ok_or_else(|| no_match(method, &params))
    }
}

/// A fixed clock, so reads carry a deterministic timestamp in tests.
#[derive(Debug, Clone, Copy)]
pub struct MockClock(pub i64);

impl Clock for MockClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0)
    }
}
