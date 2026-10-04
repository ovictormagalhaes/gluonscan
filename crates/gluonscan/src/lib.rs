//! # gluonscan
//!
//! The facade: re-exports the [`gluonscan_core`] contract, bundles the protocol adapters, and
//! wires real transports (a `reqwest` HTTP client, a system clock). Configure backends, then call
//! the read methods — conversions, math and normalization stay hidden inside the adapters.
//!
//! ```no_run
//! # #[cfg(feature = "aave")]
//! # async fn demo() -> Result<(), gluonscan::Error> {
//! use gluonscan::{Gluonscan, AaveApi, Chain, Detail, Protocol, Address, Wallet};
//!
//! let engine = Gluonscan::builder().backend(AaveApi::new()).build();
//! let reading = engine
//!     .read(Protocol::AaveV3, Wallet::Evm(Address::ZERO), Chain::Ethereum, Detail::Full)
//!     .await?;
//! println!("{:?}", reading.get());
//! # Ok(()) }
//! ```

use std::sync::Arc;

use async_trait::async_trait;
pub use gluonscan_core::*;

/// Pure DeFi math (tick math, lending health-factor + liquidation-price). Re-exported so consumers
/// reach it through the one facade, without a second version pin.
pub use gluonscan_math as math;

/// Cost-basis chain reads (ERC-20 transfer history + receipt logs + latest block). The consumer owns
/// the cost-basis math; the chain reads live here.
pub use gluonscan_evm::transfers;

// Protocol adapters are re-exported only when their feature is enabled, so a consumer that opts out
// of an ecosystem never compiles its dependency stack.
#[cfg(feature = "aave")]
pub use gluonscan_aave::AaveApi;
#[cfg(feature = "kamino")]
pub use gluonscan_kamino::KaminoApi;
#[cfg(feature = "lido")]
pub use gluonscan_lido::Lido;
#[cfg(feature = "pendle")]
pub use gluonscan_pendle::PendleApi;
#[cfg(feature = "raydium")]
pub use gluonscan_raydium::RaydiumClmm;
#[cfg(feature = "prices")]
pub use gluonscan_sources::{CoinGecko, CoinMarketCap};
#[cfg(feature = "uniswap")]
pub use gluonscan_uniswap::UniswapV3;
#[cfg(feature = "wallet")]
pub use gluonscan_wallet::{
    AlchemySolanaWallet, AlchemyWallet, BitcoinWallet, EvmNativeBalance, EvmNfts, EvmWallet,
    SolanaNativeBalance, SolanaNfts, SolanaWallet,
};

/// The default `User-Agent` sent on every request. Some provider edges (CoinGecko's CloudFront,
/// for one) reject requests without a `User-Agent` with a 403, and `reqwest` sends none by default —
/// so a client with no `User-Agent` would silently 403 the whole integration.
const USER_AGENT: &str = concat!(
    "gluonscan/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ovictormagalhaes/gluonscan)"
);

/// A `reqwest`-backed [`Http`] client. Configuration (timeouts, keys, retries) lives here, not in
/// the adapters. Every request carries a default `User-Agent` so provider edges that block
/// agent-less traffic do not 403 the client.
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    /// A client with default settings and the default `User-Agent`.
    pub fn new() -> Self {
        ReqwestHttp::with_user_agent(USER_AGENT)
    }

    /// A client that sends a custom `User-Agent` on every request.
    pub fn with_user_agent(user_agent: &str) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .build()
            .expect("failed to build reqwest client");
        ReqwestHttp { client }
    }
}

impl Default for ReqwestHttp {
    fn default() -> Self {
        ReqwestHttp::new()
    }
}

#[async_trait]
impl Http for ReqwestHttp {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<String, Error> {
        let mut req = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        read_body(req).await
    }

    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, Error> {
        let mut req = self.client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        read_body(req).await
    }
}

/// An [`Http`] decorator that throttles an inner client to at most `max` requests per `window`
/// (a sliding window). Rate limits are per-provider, so give each source its own instance for its
/// own budget, or share one instance to share a budget (e.g. across chains on the same API key).
///
/// CoinGecko's free/demo tier is roughly 30 requests/minute:
/// `CoinGecko::new(Arc::new(RateLimitedHttp::per_minute(Arc::new(ReqwestHttp::new()), 30)))`.
pub struct RateLimitedHttp {
    inner: Arc<dyn Http>,
    max: usize,
    window: std::time::Duration,
    hits: std::sync::Mutex<std::collections::VecDeque<std::time::Instant>>,
}

impl RateLimitedHttp {
    /// Throttle `inner` to at most `max` requests per `window`.
    pub fn new(inner: Arc<dyn Http>, max: usize, window: std::time::Duration) -> Self {
        RateLimitedHttp {
            inner,
            max,
            window,
            hits: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// Throttle `inner` to at most `max` requests per minute.
    pub fn per_minute(inner: Arc<dyn Http>, max: usize) -> Self {
        RateLimitedHttp::new(inner, max, std::time::Duration::from_secs(60))
    }

    /// Block until a request slot is free, then reserve it.
    async fn gate(&self) {
        loop {
            let wait = {
                let now = std::time::Instant::now();
                let mut hits = self.hits.lock().unwrap();
                while hits
                    .front()
                    .is_some_and(|&t| now.duration_since(t) >= self.window)
                {
                    hits.pop_front();
                }
                if hits.len() < self.max {
                    hits.push_back(now);
                    None
                } else {
                    let oldest = *hits.front().expect("len >= max >= 1");
                    Some(self.window - now.duration_since(oldest))
                }
            };
            match wait {
                None => return,
                Some(delay) => tokio::time::sleep(delay).await,
            }
        }
    }
}

#[async_trait]
impl Http for RateLimitedHttp {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<String, Error> {
        self.gate().await;
        self.inner.post(url, body, headers).await
    }

    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, Error> {
        self.gate().await;
        self.inner.get(url, headers).await
    }
}

async fn read_body(req: reqwest::RequestBuilder) -> Result<String, Error> {
    let resp = req.send().await.map_err(|e| Error::Transient {
        message: e.to_string(),
        retry_after: None,
    })?;
    if resp.status().as_u16() == 429 {
        return Err(Error::Transient {
            message: "HTTP 429".into(),
            retry_after: None,
        });
    }
    resp.text().await.map_err(|e| Error::Transient {
        message: e.to_string(),
        retry_after: None,
    })
}

/// A clock backed by the system time.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Timestamp(secs)
    }
}

/// Builds a [`Gluonscan`] engine: register protocol backends, optionally override the transports.
pub struct Builder {
    adapters: Vec<Arc<dyn ProtocolAdapter>>,
    http: Option<Arc<dyn Http>>,
    rpc: Option<Arc<dyn ChainProvider>>,
    price: Option<Arc<dyn PriceSource>>,
    clock: Option<Arc<dyn Clock>>,
}

impl Builder {
    /// Register a protocol backend (e.g. [`AaveApi`]). Multiple backends per protocol are allowed.
    pub fn backend<A: ProtocolAdapter + 'static>(mut self, adapter: A) -> Self {
        self.adapters.push(Arc::new(adapter));
        self
    }

    /// Override the HTTP client (defaults to [`ReqwestHttp`]).
    pub fn http(mut self, http: Arc<dyn Http>) -> Self {
        self.http = Some(http);
        self
    }

    /// Override the clock (defaults to [`SystemClock`]).
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Attach an on-chain transport (required by on-chain protocols such as Uniswap fees).
    pub fn rpc(mut self, rpc: Arc<dyn ChainProvider>) -> Self {
        self.rpc = Some(rpc);
        self
    }

    /// Register a price source, enabling the separate [`Gluonscan::price`] operation. Pricing is
    /// never folded into `read` — reading positions and pricing tokens are two operations.
    pub fn price_source(mut self, price: Arc<dyn PriceSource>) -> Self {
        self.price = Some(price);
        self
    }

    /// Finish building.
    pub fn build(self) -> Gluonscan {
        let http = self.http.unwrap_or_else(|| Arc::new(ReqwestHttp::new()));
        let clock = self.clock.unwrap_or_else(|| Arc::new(SystemClock));
        let mut cx = Ctx::new(http, clock);
        if let Some(rpc) = self.rpc {
            cx = cx.with_rpc(rpc);
        }
        Gluonscan {
            adapters: self.adapters,
            price: self.price,
            cx,
        }
    }
}

/// The configured engine.
pub struct Gluonscan {
    adapters: Vec<Arc<dyn ProtocolAdapter>>,
    price: Option<Arc<dyn PriceSource>>,
    cx: Ctx,
}

impl Gluonscan {
    /// Start configuring an engine.
    pub fn builder() -> Builder {
        Builder {
            adapters: Vec::new(),
            http: None,
            rpc: None,
            price: None,
            clock: None,
        }
    }

    /// Read one protocol for `owner` on `chain`. Fails with [`Error::Permanent`] if no registered
    /// backend for that protocol supports the chain (a protocol binds only to chains it supports).
    pub async fn read(
        &self,
        protocol: Protocol,
        owner: Wallet,
        chain: Chain,
        detail: Detail,
    ) -> Result<Complete<Reading>, Error> {
        let adapter = self
            .adapters
            .iter()
            .find(|a| a.protocol() == protocol && a.supported_chains().contains(&chain))
            .ok_or_else(|| Error::Permanent {
                message: format!("no registered {protocol:?} backend supports {chain:?}"),
            })?;
        adapter.read(&owner, chain, detail, &self.cx).await
    }

    /// Read one protocol's event history for `owner` on `chain`, limited to events after `since`
    /// (`None` = from the beginning). Fails with [`Error::Permanent`] if no registered backend for
    /// that protocol supports the chain, or [`Error::Unsupported`] if the backend has no history.
    pub async fn history(
        &self,
        protocol: Protocol,
        owner: Wallet,
        chain: Chain,
        position: Option<&str>,
        since: Option<Timestamp>,
    ) -> Result<Complete<History>, Error> {
        let adapter = self
            .adapters
            .iter()
            .find(|a| a.protocol() == protocol && a.supported_chains().contains(&chain))
            .ok_or_else(|| Error::Permanent {
                message: format!("no registered {protocol:?} backend supports {chain:?}"),
            })?;
        adapter
            .read_history(&owner, chain, position, since, &self.cx)
            .await
    }

    /// Price an [`Asset`] in USD via the configured price source — a **separate** operation from
    /// [`read`](Gluonscan::read). Reading positions and pricing assets are two distinct calls. The
    /// [`Asset`] key is chain-agnostic, so native coins (BTC, ETH, SOL) and SPL mints price too.
    /// Errors with [`Error::Permanent`] if no price source was registered.
    pub async fn price(&self, chain: Chain, asset: Asset) -> Result<rust_decimal::Decimal, Error> {
        self.price
            .as_ref()
            .ok_or_else(|| Error::Permanent {
                message: "no price source configured".into(),
            })?
            .price_usd(chain, asset)
            .await
    }

    /// Price several assets on one chain in one call (batched by the source when possible), one
    /// result per asset in input order. Errors with [`Error::Permanent`] if no price source is set.
    pub async fn prices(
        &self,
        chain: Chain,
        assets: &[Asset],
    ) -> Result<Vec<Result<rust_decimal::Decimal, Error>>, Error> {
        let source = self.price.as_ref().ok_or_else(|| Error::Permanent {
            message: "no price source configured".into(),
        })?;
        Ok(source.prices_usd(chain, assets).await)
    }

    /// Inbound ERC-20 transfers of `token` to `wallet` at/after `from_block` — the purchase legs a
    /// consumer needs to build a cost basis. A separate chain read (like [`price`](Gluonscan::price)),
    /// over the engine's injected RPC.
    pub async fn asset_transfers_in(
        &self,
        chain: Chain,
        wallet: &str,
        token: &str,
        from_block: Option<u64>,
    ) -> Result<Vec<transfers::AssetTransfer>, Error> {
        transfers::asset_transfers_in(self.cx.rpc()?.as_ref(), chain, wallet, token, from_block)
            .await
    }

    /// The ERC-20 `Transfer` logs of transaction `tx` — e.g. to find what a wallet paid in the same
    /// transaction that delivered a position token.
    pub async fn receipt_transfers(
        &self,
        chain: Chain,
        tx: &str,
    ) -> Result<Vec<transfers::ReceiptTransfer>, Error> {
        transfers::receipt_transfers(self.cx.rpc()?.as_ref(), chain, tx).await
    }

    /// The latest block number on `chain`.
    pub async fn latest_block(&self, chain: Chain) -> Result<u64, Error> {
        transfers::latest_block(self.cx.rpc()?.as_ref(), chain).await
    }
}
