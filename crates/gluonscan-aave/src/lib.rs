//! # gluonscan-aave
//!
//! Aave V3 adapter, **API backend** ([`Source::Api`]): reads a wallet's supplies, borrows and
//! account health factor from the official Aave V3 GraphQL API and normalizes them into a
//! [`LendingPosition`]. No RPC — this backend is HTTP-only.
//!
//! History reads the chain's Aave V3 subgraph: supplies, withdrawals, borrows and repays, each kind
//! paged by `id` ([`HISTORY_PAGE_SIZE`] per page) until a page comes back short, then merged
//! oldest-first. Any page that fails, carries GraphQL errors or lacks a requested kind fails the
//! whole read; so does a kind longer than [`MAX_HISTORY_PAGES`] pages.
//!
//! A future on-chain backend would live in a sibling crate and register for the same
//! [`Protocol::AaveV3`]; the engine routes capabilities between them.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::{
    Amount, BorrowedAsset, Capability, Chain, Complete, Ctx, Currency, Detail, Error, EventKind,
    History, HistoryEvent, LendingPosition, Money, Position, Protocol, ProtocolAdapter, Provenance,
    Rate, RateBasis, RateWindow, Rates, Reading, Source, Staleness, SuppliedAsset, Timestamp,
    Token, TokenAddress, Wallet,
};
use rust_decimal::Decimal;

const AAVE_V3_API: &str = "https://api.v3.aave.com/graphql";

const CAPABILITIES: &[Capability] = &[
    Capability::Positions,
    Capability::HealthFactor,
    Capability::RiskConfig,
    Capability::History,
];
const SUPPORTED_CHAINS: &[Chain] = &[
    Chain::Ethereum,
    Chain::Base,
    Chain::Arbitrum,
    Chain::Optimism,
    Chain::Polygon,
    Chain::Bnb,
];

/// The Aave V3 adapter backed by the official GraphQL API. Position reads use the API; event
/// history reads a per-chain subgraph (The Graph), injected via [`AaveApi::with_subgraphs`].
#[derive(Debug, Default, Clone)]
pub struct AaveApi {
    endpoint: Option<String>,
    subgraphs: HashMap<Chain, String>,
}

impl AaveApi {
    /// Construct with the default public endpoint and no history subgraphs.
    pub fn new() -> Self {
        AaveApi {
            endpoint: None,
            subgraphs: HashMap::new(),
        }
    }

    /// Override the endpoint (for a private gateway or tests).
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Some(url.into());
        self
    }

    /// Set the per-chain history subgraph URLs (The Graph gateway). Without these, `read_history`
    /// returns an empty history for the chain.
    pub fn with_subgraphs(mut self, subgraphs: HashMap<Chain, String>) -> Self {
        self.subgraphs = subgraphs;
        self
    }

    fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(AAVE_V3_API)
    }

    /// The Aave V3 Pool/market address for a supported chain.
    fn market(chain: Chain) -> Option<&'static str> {
        Some(match chain {
            Chain::Ethereum => "0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2",
            Chain::Base => "0xA238Dd80C259a72e81d7e4664a9801593F98d1c5",
            // Deterministic Aave V3 Pool address shared across these deployments.
            Chain::Arbitrum | Chain::Optimism | Chain::Polygon => {
                "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
            }
            Chain::Bnb => "0x6807dc923806fE8Fd134338EABCA509979a7e0cB",
            _ => return None,
        })
    }
}

#[async_trait]
impl ProtocolAdapter for AaveApi {
    fn protocol(&self) -> Protocol {
        Protocol::AaveV3
    }

    fn source(&self) -> Source {
        Source::Api
    }

    fn capabilities(&self) -> &'static [Capability] {
        CAPABILITIES
    }

    fn supported_chains(&self) -> &'static [Chain] {
        SUPPORTED_CHAINS
    }

    async fn read(
        &self,
        owner: &Wallet,
        chain: Chain,
        _detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        let owner = owner.evm()?;
        let chain_id = chain.evm_chain_id().ok_or_else(|| Error::Permanent {
            message: format!("Aave V3 is EVM-only; {chain:?} has no chain id"),
        })?;
        let market = AaveApi::market(chain).ok_or_else(|| Error::Permanent {
            message: format!("Aave V3 not configured for {chain:?}"),
        })?;

        let user = format!("{owner:#x}");
        let body = query_body(market, chain_id, &user);
        let raw = cx.http.post(self.endpoint(), body, &[]).await?;

        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Aave response was not valid JSON: {e}"),
        })?;
        check_graphql_errors(&json)?;
        let data = json.get("data").ok_or_else(|| Error::Integrity {
            message: "Aave response missing `data`".into(),
        })?;

        let now = cx.clock.now();
        let mut supplied = parse_supplied(data.get("userSupplies"), now)?;
        let borrowed = parse_borrowed(data.get("userBorrows"), now)?;

        let has_debt = !borrowed.is_empty();
        let health_factor = parse_health_factor(data.get("userMarketState"));
        if has_debt && health_factor.is_none() {
            // Never hide liquidation risk: a debt position without a health factor is incomplete.
            return Err(Error::Integrity {
                message: "Aave account has debt but no health factor was returned".into(),
            });
        }

        // Per-reserve risk config (liquidation threshold, max LTV) is a separate query — fetched at
        // Full detail so offline health-factor recompute has the parameters.
        if matches!(_detail, Detail::Full) && !supplied.is_empty() {
            enrich_risk(cx, self.endpoint(), market, chain_id, &mut supplied).await?;
        }

        // One Pool per chain holds the whole cross-collateralized account, so its address is the
        // position's stable identity (unlike the asset mix, it never changes for the account).
        let position = LendingPosition::new(supplied, borrowed)
            .with_health_factor(health_factor)
            .with_market_id(Some(market.to_lowercase()));
        let reading = Reading::new(
            Protocol::AaveV3,
            chain,
            Source::Api,
            vec![Position::Lending(position)],
            Provenance::new(Source::Api, chain, cx.clock.now(), Staleness::Live),
        );
        Ok(Complete::new(reading))
    }

    async fn read_history(
        &self,
        owner: &Wallet,
        chain: Chain,
        _position: Option<&str>,
        since: Option<Timestamp>,
        cx: &Ctx,
    ) -> Result<Complete<History>, Error> {
        let owner = owner.evm()?;
        let provenance = Provenance::new(Source::Subgraph, chain, cx.clock.now(), Staleness::Live);

        // No history subgraph configured for this chain → no events (not an error).
        let Some(subgraph) = self.subgraphs.get(&chain) else {
            return Ok(Complete::new(History::new(
                Protocol::AaveV3,
                chain,
                Vec::new(),
                provenance,
            )));
        };

        let since_ts = since.map(|t| t.0).unwrap_or(0);
        let user = format!("{owner:#x}");

        // Each kind is paged on its own `id` cursor until a page comes back short. A consumer
        // checkpoints on the newest event across kinds, so a kind cut at a page boundary would lose
        // its later events for good: every kind is read to its end or the read fails.
        let mut cursors: Vec<Option<String>> = vec![Some(String::new()); EVENT_KINDS.len()];
        let mut seen: Vec<HashSet<String>> = vec![HashSet::new(); EVENT_KINDS.len()];
        let mut events = Vec::new();
        for page in 0.. {
            let open: Vec<(usize, &str)> = cursors
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.as_deref().map(|c| (i, c)))
                .collect();
            if open.is_empty() {
                break;
            }
            if page == MAX_HISTORY_PAGES {
                return Err(Error::Integrity {
                    message: format!(
                        "Aave history exceeds {MAX_HISTORY_PAGES} pages of {HISTORY_PAGE_SIZE} events per kind"
                    ),
                });
            }
            let body = events_query(&user, since_ts, &open);
            let raw = cx.http.post(subgraph, body, &[]).await?;
            let json: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| Error::Integrity {
                    message: format!("Aave events response not JSON: {e}"),
                })?;
            check_graphql_errors(&json)?;
            let data = json.get("data").ok_or_else(|| Error::Integrity {
                message: "Aave events response missing `data`".into(),
            })?;

            let mut next = Vec::with_capacity(open.len());
            for &(i, _) in &open {
                let (field, kind) = EVENT_KINDS[i];
                // A requested kind that is absent is the query failing, not a user without events.
                let nodes =
                    data.get(field)
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| Error::Integrity {
                            message: format!("Aave events response has no `{field}` array"),
                        })?;
                let mut last = None;
                for node in nodes {
                    let id = node
                        .get("id")
                        .and_then(|v| v.as_str())
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| Error::Integrity {
                            message: format!("Aave `{field}` event missing `id`"),
                        })?;
                    // A repeated id means the cursor did not advance: paging would never end.
                    if !seen[i].insert(id.to_string()) {
                        return Err(Error::Integrity {
                            message: format!("Aave `{field}` page repeated event {id}"),
                        });
                    }
                    events.push(parse_event(node, kind)?);
                    last = Some(id);
                }
                let cursor = match last {
                    Some(id) if nodes.len() >= HISTORY_PAGE_SIZE => Some(id.to_string()),
                    _ => None,
                };
                next.push((i, cursor));
            }
            for (i, cursor) in next {
                cursors[i] = cursor;
            }
        }
        events.sort_by_key(|e| e.at.0);
        Ok(Complete::new(History::new(
            Protocol::AaveV3,
            chain,
            events,
            provenance,
        )))
    }
}

/// The subgraph event entities read as history, with the kind each one maps to.
const EVENT_KINDS: [(&str, EventKind); 4] = [
    ("supplies", EventKind::Deposit),
    ("redeemUnderlyings", EventKind::Withdraw),
    ("borrows", EventKind::Borrow),
    ("repays", EventKind::Repay),
];

/// Events of one kind requested per page (The Graph's largest `first`). A page this full means the
/// kind may hold more, so the next page is read.
pub const HISTORY_PAGE_SIZE: usize = 1000;

/// Pages read per kind before a history is refused as too large to read completely.
pub const MAX_HISTORY_PAGES: usize = 100;

/// The GraphQL query for one page of a user's lending events after `since`: for each still-open
/// kind (`(index into EVENT_KINDS, id cursor)`), the next [`HISTORY_PAGE_SIZE`] events in `id`
/// order after the cursor. `id` is unique, so unlike a timestamp cursor no event sharing a block
/// with the page boundary is skipped.
fn events_query(user: &str, since: i64, open: &[(usize, &str)]) -> String {
    const FIELDS: &str = "id timestamp amount txHash reserve{symbol name underlyingAsset decimals}";
    let mut fields = String::new();
    for &(i, after) in open {
        let (field, _) = EVENT_KINDS[i];
        // A JSON string literal is a valid GraphQL string literal, so the cursor is escaped.
        let after = serde_json::Value::from(after);
        fields.push_str(&format!(
            "{field}(where:{{user:$user,timestamp_gt:$since,id_gt:{after}}},orderBy:id,orderDirection:asc,first:{HISTORY_PAGE_SIZE}){{{FIELDS}}} "
        ));
    }
    let q = format!("query($user:Bytes!,$since:Int!){{{fields}}}");
    serde_json::json!({ "query": q, "variables": { "user": user, "since": since } }).to_string()
}

/// Parse one subgraph event node (raw `amount`, `reserve{...}`) into a normalized [`HistoryEvent`].
fn parse_event(node: &serde_json::Value, kind: EventKind) -> Result<HistoryEvent, Error> {
    let symbol = node
        .pointer("/reserve/symbol")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = node
        .pointer("/reserve/name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let decimals = node
        .pointer("/reserve/decimals")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .and_then(|n| u8::try_from(n).ok())
        .ok_or_else(|| Error::Integrity {
            message: "Aave event reserve missing/invalid decimals".into(),
        })?;
    let address = node
        .pointer("/reserve/underlyingAsset")
        .and_then(|v| v.as_str())
        .and_then(|s| Address::from_str(s).ok());
    let raw = node
        .get("amount")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Integrity {
            message: "Aave event missing amount".into(),
        })?;
    let raw = U256::from_str(raw).map_err(|e| Error::Integrity {
        message: format!("Aave event amount not an integer: {e}"),
    })?;
    let at = node
        .get("timestamp")
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .ok_or_else(|| Error::Integrity {
            message: "Aave event missing/invalid timestamp".into(),
        })?;
    let tx = node
        .get("txHash")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            node.get("id")
                .and_then(|v| v.as_str())
                .map(|id| id.split(':').next().unwrap_or(id).to_string())
        })
        .unwrap_or_default();
    let amount = Amount::from_raw(Token::evm(symbol, address, decimals).with_name(name), raw)?;
    Ok(HistoryEvent::new(kind, amount, tx, Timestamp(at)))
}

/// Fail closed if the GraphQL response carries an `errors` array — never return the empty `data`
/// that accompanies a failed query as if it were "no positions".
fn check_graphql_errors(json: &serde_json::Value) -> Result<(), Error> {
    if let Some(errs) = json.get("errors").and_then(|e| e.as_array()) {
        if !errs.is_empty() {
            let msg = errs
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Error::Integrity {
                message: format!("Aave GraphQL error: {msg}"),
            });
        }
    }
    Ok(())
}

/// Fetch per-reserve `maxLTV` / `liquidationThreshold` for the supplied assets in one batched query
/// (an alias per token) and set them on each [`SuppliedAsset`].
async fn enrich_risk(
    cx: &Ctx,
    endpoint: &str,
    market: &str,
    chain_id: u64,
    supplied: &mut [SuppliedAsset],
) -> Result<(), Error> {
    let addrs: Vec<Address> = {
        let mut seen = Vec::new();
        for s in supplied.iter() {
            if let Some(a) = s
                .amount
                .token
                .address
                .as_ref()
                .and_then(TokenAddress::as_evm)
            {
                if !seen.contains(&a) {
                    seen.push(a);
                }
            }
        }
        seen
    };
    if addrs.is_empty() {
        return Ok(());
    }

    let mut fields = String::new();
    for (i, a) in addrs.iter().enumerate() {
        fields.push_str(&format!(
            r#"r{i}:reserve(request:{{market:"{market}",chainId:{chain_id},underlyingToken:"{a:#x}"}}){{supplyInfo{{maxLTV{{value}} liquidationThreshold{{value}}}}}} "#
        ));
    }
    let body = serde_json::json!({ "query": format!("query{{{fields}}}") }).to_string();
    let raw = cx.http.post(endpoint, body, &[]).await?;
    let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("Aave reserve response not JSON: {e}"),
    })?;
    check_graphql_errors(&json)?;
    let data = json.get("data").ok_or_else(|| Error::Integrity {
        message: "Aave reserve response missing `data`".into(),
    })?;

    let mut risk: HashMap<Address, (Option<Decimal>, Option<Decimal>)> = HashMap::new();
    for (i, a) in addrs.iter().enumerate() {
        if let Some(node) = data.get(format!("r{i}")) {
            risk.insert(
                *a,
                (
                    node.pointer("/supplyInfo/maxLTV/value")
                        .and_then(json_decimal),
                    node.pointer("/supplyInfo/liquidationThreshold/value")
                        .and_then(json_decimal),
                ),
            );
        }
    }
    for s in supplied.iter_mut() {
        if let Some(a) = s
            .amount
            .token
            .address
            .as_ref()
            .and_then(TokenAddress::as_evm)
        {
            if let Some((mlt, lt)) = risk.get(&a) {
                s.max_ltv = *mlt;
                s.liquidation_threshold = *lt;
            }
        }
    }
    Ok(())
}

fn query_body(market: &str, chain_id: u64, user: &str) -> String {
    // Combined query: supplies + borrows + account state, in one round trip.
    let query = r#"query($market:String!,$chainId:Int!,$user:String!){
      userSupplies(request:{markets:[{address:$market,chainId:$chainId}],user:$user}){
        currency{symbol name address decimals} balance{amount{value} usd}
        apy{value} isCollateral canBeCollateral
      }
      userBorrows(request:{markets:[{address:$market,chainId:$chainId}],user:$user}){
        currency{symbol name address decimals} debt{amount{value} usd} apy{value}
      }
      userMarketState(request:{market:$market,chainId:$chainId,user:$user}){ healthFactor }
    }"#;
    let payload = serde_json::json!({
        "query": query,
        "variables": { "market": market, "chainId": chain_id, "user": user }
    });
    payload.to_string()
}

/// Build the [`Amount`] from an entry's `currency` + the `balance`/`debt` sub-object.
fn parse_amount_entry(item: &serde_json::Value, balance_key: &str) -> Result<Amount, Error> {
    let currency = item.get("currency").ok_or_else(|| Error::Integrity {
        message: "Aave entry missing `currency`".into(),
    })?;
    let symbol = currency
        .get("symbol")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = currency
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let decimals = currency
        .get("decimals")
        .and_then(|v| v.as_u64())
        .and_then(|n| u8::try_from(n).ok())
        .ok_or_else(|| Error::Integrity {
            message: format!("Aave currency `{symbol}` missing or out-of-range decimals"),
        })?;
    let address = currency
        .get("address")
        .and_then(|v| v.as_str())
        .and_then(|s| Address::from_str(s).ok());

    let balance = item.get(balance_key).ok_or_else(|| Error::Integrity {
        message: format!("Aave entry missing `{balance_key}`"),
    })?;
    let amount = balance
        .get("amount")
        .and_then(|a| a.get("value"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Integrity {
            message: format!("Aave `{symbol}` missing amount value"),
        })?;
    let amount = Decimal::from_str(amount).map_err(|e| Error::Integrity {
        message: format!("Aave `{symbol}` amount not a decimal: {e}"),
    })?;

    // A present-but-null usd is "not priced" (kept as None); we never fabricate a value.
    let usd = balance
        .get("usd")
        .and_then(json_decimal)
        .map(|amount| Money {
            amount,
            currency: Currency::Usd,
        });

    Ok(Amount::from_decimal(
        Token::evm(symbol, address, decimals).with_name(name),
        amount,
    )?
    .with_usd(usd))
}

/// The entry's `apy { value }` as an instantaneous APY fraction. The API stamps no time on it, so
/// it is dated at the read.
fn parse_rates(item: &serde_json::Value, now: Timestamp) -> Rates {
    item.pointer("/apy/value")
        .and_then(json_decimal)
        .map(|apy| Rate::new(apy, RateBasis::Apy, RateWindow::Instant, Source::Api, now))
        .into_iter()
        .collect()
}

/// The positions array for a GraphQL field. An absent or null field means "no positions" (`Ok`
/// empty); a field that is present but not an array is a malformed response and fails closed rather
/// than being read as empty.
fn as_position_array<'a>(
    field: Option<&'a serde_json::Value>,
    name: &str,
) -> Result<&'a [serde_json::Value], Error> {
    match field {
        None | Some(serde_json::Value::Null) => Ok(&[]),
        Some(v) => v
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| Error::Integrity {
                message: format!("Aave {name} present but not an array"),
            }),
    }
}

fn parse_supplied(
    list: Option<&serde_json::Value>,
    now: Timestamp,
) -> Result<Vec<SuppliedAsset>, Error> {
    let items = as_position_array(list, "userSupplies")?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let amount = parse_amount_entry(item, "balance")?;
        // liquidation_threshold / max_ltv are filled by the separate reserve() query in
        // enrich_risk (Full detail).
        let is_collateral = item
            .get("isCollateral")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let can_be_collateral = item
            .get("canBeCollateral")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        out.push(
            SuppliedAsset::new(amount)
                .with_collateral(is_collateral, can_be_collateral)
                .with_rates(parse_rates(item, now)),
        );
    }
    Ok(out)
}

fn parse_borrowed(
    list: Option<&serde_json::Value>,
    now: Timestamp,
) -> Result<Vec<BorrowedAsset>, Error> {
    let items = as_position_array(list, "userBorrows")?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let amount = parse_amount_entry(item, "debt")?;
        // Aave V3 pins the borrow factor to 1.0 (no borrow-factor haircut).
        out.push(
            BorrowedAsset::new(amount)
                .with_borrow_factor(Some(Decimal::ONE))
                .with_rates(parse_rates(item, now)),
        );
    }
    Ok(out)
}

/// Read a decimal from a JSON string or number literal (never via `f64`).
fn json_decimal(v: &serde_json::Value) -> Option<Decimal> {
    if let Some(s) = v.as_str() {
        Decimal::from_str(s).ok()
    } else if v.is_number() {
        Decimal::from_str(&v.to_string()).ok()
    } else {
        None
    }
}

fn parse_health_factor(state: Option<&serde_json::Value>) -> Option<Decimal> {
    state
        .and_then(|s| s.get("healthFactor"))
        .and_then(|v| v.as_str())
        .and_then(|s| Decimal::from_str(s).ok())
}
