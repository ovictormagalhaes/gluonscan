//! # gluonscan-raydium
//!
//! Raydium concentrated-liquidity (CLMM) adapter, [`Source::OnChain`]: discovers position NFTs
//! from the wallet's token accounts, derives each position PDA, and decodes the on-chain position,
//! pool and mint accounts into a [`LiquidityPosition`] (principal amounts via the Q64.96 math,
//! uncollected fees from the position's owed fields, in-range from the pool tick).
//!
//! The account byte offsets match the on-chain Raydium CLMM `PersonalPositionState` and `PoolState`
//! layouts. Discovery queries both the classic SPL Token program and Token-2022 (Raydium's newer
//! position NFTs mint under Token-2022).

use alloy_primitives::U256;
use async_trait::async_trait;
use gluonscan_core::{
    scaled, Amount, Capability, Chain, ChainProvider, Complete, Ctx, Detail, Error, EventKind,
    History, HistoryEvent, LiquidityPosition, Position, PositionStatus, Protocol, ProtocolAdapter,
    Provenance, Reading, Source, Staleness, Timestamp, Token, Wallet,
};
use gluonscan_math::{
    get_amounts_for_liquidity, get_sqrt_ratio_at_tick, is_in_range, MAX_TICK, MIN_TICK,
};
use gluonscan_solana::{
    find_program_address, get_account_info, get_mint_decimals, get_token_accounts_by_owner,
    pubkey_bytes, pubkey_str, RAYDIUM_CLMM_PROGRAM,
};
use rust_decimal::Decimal;

const CAPABILITIES: &[Capability] = &[Capability::Positions, Capability::Fees, Capability::History];

/// Wrapped SOL mint — when a pool leg is WSOL and the SPL delta is zero, the real movement is the
/// wallet's native SOL balance delta (wrap/unwrap happens in the same tx).
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
/// Raydium CLMM program id (instruction `programId` filter for discriminator detection).
const CLMM_PROGRAM_ID: &str = RAYDIUM_CLMM_PROGRAM;
/// Signature pagination: up to `SIG_MAX_PAGES` pages of `SIG_PAGE_LIMIT`, stopping early once a page
/// reaches signatures older than the `since` cutoff.
const SIG_PAGE_LIMIT: usize = 1000;
const SIG_MAX_PAGES: usize = 5;
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Solana];

// PersonalPositionState layout (byte offsets): 8-byte discriminator, bump, nft_mint[9..41],
// pool_id[41..73], tick_lower[73..77], tick_upper[77..81], liquidity[81..97],
// fee_growth_inside_0/1[97..129], token_fees_owed_0[129..137], token_fees_owed_1[137..145].
const POS_MIN_LEN: usize = 145;
const POS_POOL: usize = 41;
const POS_TICK_LOWER: usize = 73;
const POS_TICK_UPPER: usize = 77;
const POS_LIQUIDITY: usize = 81;
const POS_FEE0: usize = 129;
const POS_FEE1: usize = 137;

// PoolState layout (byte offsets): token_mint_0[73..105], token_mint_1[105..137],
// mint_decimals_0[233], mint_decimals_1[234], tick_spacing[235..237], liquidity[237..253],
// sqrt_price_x64[253..269], tick_current[269..273].
const POOL_MIN_LEN: usize = 273;
const POOL_MINT0: usize = 73;
const POOL_MINT1: usize = 105;
const POOL_DEC0: usize = 233;
const POOL_DEC1: usize = 234;
const POOL_TICK_SPACING: usize = 235;
const POOL_SQRT_X64: usize = 253;
const POOL_TICK: usize = 269;

/// The Raydium CLMM adapter.
#[derive(Debug, Default, Clone)]
pub struct RaydiumClmm;

impl RaydiumClmm {
    /// Construct the adapter.
    pub fn new() -> Self {
        RaydiumClmm
    }
}

#[async_trait]
impl ProtocolAdapter for RaydiumClmm {
    fn protocol(&self) -> Protocol {
        Protocol::Raydium
    }

    fn source(&self) -> Source {
        Source::OnChain
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
        if chain != Chain::Solana {
            return Err(Error::Permanent {
                message: format!("Raydium is Solana-only; got {chain:?}"),
            });
        }
        let wallet = owner.solana()?;
        let rpc = cx.rpc()?.as_ref();
        let program = pubkey_bytes(RAYDIUM_CLMM_PROGRAM)?;

        let mints = get_token_accounts_by_owner(rpc, wallet).await?;
        let mut positions = Vec::new();
        for mint in mints {
            let mint_bytes = pubkey_bytes(&mint)?;
            let (pda, _) = find_program_address(&[b"position", &mint_bytes], &program);
            let Some(data) = get_account_info(rpc, &pubkey_str(&pda)).await? else {
                continue; // no position PDA → this NFT is not a Raydium position
            };
            if data.len() < POS_MIN_LEN {
                continue;
            }
            positions.push(parse_position(rpc, &data, &mint).await?);
        }

        let reading = Reading::new(
            Protocol::Raydium,
            Chain::Solana,
            Source::OnChain,
            positions,
            Provenance::new(
                Source::OnChain,
                Chain::Solana,
                cx.clock.now(),
                Staleness::Live,
            ),
        );
        Ok(Complete::new(reading))
    }

    /// Deposit/withdraw/collect history for one CLMM position, reconstructed from on-chain
    /// transactions. `position` is `"nftMint|mintA|mintB"`. The position PDA (not the NFT mint) is
    /// scanned, since `collect_fee` references the PDA. For each signature newer than `since` the
    /// transaction is classified: an Anchor discriminator (`open_position*`/`increase_liquidity*` →
    /// Deposit, `decrease_liquidity*` → Withdraw, or a `decrease_liquidity` with `liquidity == 0` →
    /// CollectFees) takes precedence; otherwise a positive two-token delta is treated as a fee
    /// collect. Each two-token movement becomes paired single-token events. Fails closed (ADR-016):
    /// any transport/RPC failure aborts the whole read so the consumer keeps its checkpoint.
    async fn read_history(
        &self,
        owner: &Wallet,
        chain: Chain,
        position: Option<&str>,
        since: Option<Timestamp>,
        cx: &Ctx,
    ) -> Result<Complete<History>, Error> {
        if chain != Chain::Solana {
            return Err(Error::Permanent {
                message: format!("Raydium is Solana-only; got {chain:?}"),
            });
        }
        let selector = position.ok_or_else(|| Error::Permanent {
            message: "Raydium history needs a position selector \"nftMint|mintA|mintB\"".into(),
        })?;
        let mut parts = selector.split('|');
        let (nft_mint, mint_a, mint_b) = match (parts.next(), parts.next(), parts.next()) {
            (Some(n), Some(a), Some(b)) if !n.is_empty() && !a.is_empty() && !b.is_empty() => {
                (n, a, b)
            }
            _ => {
                return Err(Error::Permanent {
                    message: format!(
                        "Raydium selector must be \"nftMint|mintA|mintB\", got {selector:?}"
                    ),
                })
            }
        };
        let wallet = owner.solana()?;
        let rpc = cx.rpc()?.as_ref();
        let since_ts = since.map(|t| t.0).unwrap_or(0);

        // Scan the position PDA (referenced by open/increase/decrease/collect); the NFT mint is not
        // referenced by collect_fee, so scanning it would miss every fee collect.
        let scan_address = position_pda(nft_mint).unwrap_or_else(|| nft_mint.to_string());

        // Mint decimals once so each paired event's raw base-unit is exact.
        let token_a = Token::solana(
            "",
            Some(mint_a.to_string()),
            get_mint_decimals(rpc, mint_a).await?,
        );
        let token_b = Token::solana(
            "",
            Some(mint_b.to_string()),
            get_mint_decimals(rpc, mint_b).await?,
        );

        let signatures = fetch_signatures(rpc, &scan_address, since_ts).await?;

        let mut events: Vec<HistoryEvent> = Vec::new();
        for sig in &signatures {
            let tx = fetch_transaction(rpc, sig).await?;
            // Skip transactions that failed on-chain — they moved nothing.
            if tx
                .pointer("/meta/err")
                .map(|e| !e.is_null())
                .unwrap_or(false)
            {
                continue;
            }
            let at = tx.get("blockTime").and_then(|v| v.as_i64()).unwrap_or(0);
            // Discriminator classification wins; otherwise fall back to the positive-delta fee-collect
            // heuristic (so a decrease_liquidity(0) and a plain collect are both captured, once).
            if let Some((kind, a0, a1)) = classify_discriminator(&tx, wallet, mint_a, mint_b) {
                push_pair(&mut events, kind, &token_a, &token_b, a0, a1, sig, at)?;
            } else if let Some((a0, a1)) = classify_collect_heuristic(&tx, wallet, mint_a, mint_b) {
                push_pair(
                    &mut events,
                    EventKind::CollectFees,
                    &token_a,
                    &token_b,
                    a0,
                    a1,
                    sig,
                    at,
                )?;
            }
        }

        events.retain(|e| e.at.0 > since_ts);
        events.sort_by_key(|e| e.at.0);

        let history = History::new(
            Protocol::Raydium,
            Chain::Solana,
            events,
            Provenance::new(
                Source::OnChain,
                Chain::Solana,
                cx.clock.now(),
                Staleness::Live,
            ),
        );
        Ok(Complete::new(history))
    }
}

async fn parse_position(
    rpc: &dyn gluonscan_core::ChainProvider,
    data: &[u8],
    nft_mint: &str,
) -> Result<Position, Error> {
    let pool_id = pubkey_at(data, POS_POOL)?;
    let tick_lower = i32_at(data, POS_TICK_LOWER)?;
    let tick_upper = i32_at(data, POS_TICK_UPPER)?;
    let liquidity = u128_at(data, POS_LIQUIDITY)?;
    let fee0 = u64_at(data, POS_FEE0)?;
    let fee1 = u64_at(data, POS_FEE1)?;

    // Pool: current tick, sqrt price (Q64.64), and the two mints.
    let pool = get_account_info(rpc, &pubkey_str(&pool_id))
        .await?
        .filter(|d| d.len() >= POOL_MIN_LEN)
        .ok_or_else(|| Error::Integrity {
            message: "Raydium pool account missing or too short".into(),
        })?;
    if !(MIN_TICK..=MAX_TICK).contains(&tick_lower) || !(MIN_TICK..=MAX_TICK).contains(&tick_upper)
    {
        return Err(Error::Integrity {
            message: format!("Raydium ticks out of range: {tick_lower}..{tick_upper}"),
        });
    }
    let tick_current = i32_at(&pool, POOL_TICK)?;
    let sqrt_x64 = u128_at(&pool, POOL_SQRT_X64)?;
    // The two mints and their decimals live in the pool account itself, so no extra mint reads.
    let mint0 = pubkey_str(&pubkey_at(&pool, POOL_MINT0)?);
    let mint1 = pubkey_str(&pubkey_at(&pool, POOL_MINT1)?);
    let dec0 = *pool.get(POOL_DEC0).ok_or_else(short)?;
    let dec1 = *pool.get(POOL_DEC1).ok_or_else(short)?;
    let token0 = Token::solana(String::new(), Some(mint0), dec0);
    let token1 = Token::solana(String::new(), Some(mint1), dec1);

    // Principal amounts via the shared Q64.96 math (Raydium's sqrt price is Q64.64 → shift up 32).
    let sqrt_price = U256::from(sqrt_x64) << 32;
    let (amt0, amt1) = get_amounts_for_liquidity(
        sqrt_price,
        get_sqrt_ratio_at_tick(tick_lower),
        get_sqrt_ratio_at_tick(tick_upper),
        U256::from(liquidity),
    );

    let status = if liquidity == 0 {
        PositionStatus::Inactive
    } else {
        PositionStatus::Active
    };
    let assets = vec![
        raw_amount(token0.clone(), amt0)?,
        raw_amount(token1.clone(), amt1)?,
    ];
    let uncollected_fees = vec![
        raw_amount(token0.clone(), U256::from(fee0))?,
        raw_amount(token1.clone(), U256::from(fee1))?,
    ];
    // Pool APR would come from the Raydium API, not the on-chain position; left to the consumer.
    Ok(Position::Liquidity(
        LiquidityPosition::new(
            token0,
            token1,
            tick_lower,
            tick_upper,
            tick_current,
            is_in_range(tick_current, tick_lower, tick_upper),
        )
        .with_id(Some(nft_mint.to_string()))
        .with_pool(Some(pubkey_str(&pool_id)))
        .with_sqrt_price(Some(sqrt_price.to_string()))
        .with_tick_spacing(
            pool.get(POOL_TICK_SPACING..POOL_TICK_SPACING + 2)
                .and_then(|s| s.try_into().ok())
                .map(|b| u16::from_le_bytes(b) as i32),
        )
        .with_assets(assets)
        .with_uncollected_fees(uncollected_fees)
        .with_status(status),
    ))
}

fn raw_amount(token: Token, raw: U256) -> Result<Amount, Error> {
    let amount = scaled(raw, token.decimals)?;
    Ok(Amount {
        token,
        raw,
        amount,
        usd: None,
    })
}

fn short() -> Error {
    Error::Integrity {
        message: "Solana account data too short".into(),
    }
}

fn i32_at(d: &[u8], o: usize) -> Result<i32, Error> {
    d.get(o..o + 4)
        .and_then(|s| s.try_into().ok())
        .map(i32::from_le_bytes)
        .ok_or_else(short)
}

fn u64_at(d: &[u8], o: usize) -> Result<u64, Error> {
    d.get(o..o + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(short)
}

fn u128_at(d: &[u8], o: usize) -> Result<u128, Error> {
    d.get(o..o + 16)
        .and_then(|s| s.try_into().ok())
        .map(u128::from_le_bytes)
        .ok_or_else(short)
}

fn pubkey_at(d: &[u8], o: usize) -> Result<[u8; 32], Error> {
    d.get(o..o + 32)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(short)
}

// ---- history helpers -------------------------------------------------------

/// Derive the Raydium position PDA (`["position", nftMint]`) as a base58 string.
fn position_pda(nft_mint: &str) -> Option<String> {
    let mint_bytes = pubkey_bytes(nft_mint).ok()?;
    let program = pubkey_bytes(RAYDIUM_CLMM_PROGRAM).ok()?;
    let (pda, _) = find_program_address(&[b"position", &mint_bytes], &program);
    Some(pubkey_str(&pda))
}

/// All signatures touching `address` newer than `since_ts`, newest-first, paginated. Stops early once
/// a page reaches the cutoff. A transport failure propagates (fail-closed).
async fn fetch_signatures(
    rpc: &dyn ChainProvider,
    address: &str,
    since_ts: i64,
) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    let mut before: Option<String> = None;
    for _ in 0..SIG_MAX_PAGES {
        let mut opts = serde_json::json!({ "limit": SIG_PAGE_LIMIT, "commitment": "finalized" });
        if let Some(b) = &before {
            opts["before"] = serde_json::json!(b);
        }
        let params = serde_json::json!([address, opts]).to_string();
        let raw = rpc
            .call(Chain::Solana, "getSignaturesForAddress", params)
            .await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("getSignaturesForAddress not JSON: {e}"),
        })?;
        let arr = json
            .pointer("/result")
            .and_then(|r| r.as_array())
            .ok_or_else(|| Error::Integrity {
                message: "getSignaturesForAddress missing result".into(),
            })?;
        if arr.is_empty() {
            break;
        }
        let page_len = arr.len();
        let mut last_sig = None;
        let mut reached_cutoff = false;
        for item in arr {
            let Some(sig) = item.get("signature").and_then(|s| s.as_str()) else {
                continue;
            };
            last_sig = Some(sig.to_string());
            // Signatures are newest-first; a blockTime at/under the cutoff ends the walk.
            if item
                .get("blockTime")
                .and_then(|b| b.as_i64())
                .map(|bt| bt <= since_ts)
                .unwrap_or(false)
            {
                reached_cutoff = true;
                continue;
            }
            out.push(sig.to_string());
        }
        if page_len < SIG_PAGE_LIMIT || reached_cutoff {
            break;
        }
        before = last_sig;
    }
    Ok(out)
}

/// Fetch one parsed transaction's `result` object. A transport failure or a null result (a finalized
/// signature whose transaction the node can't yet return) propagates so the whole read fails closed.
async fn fetch_transaction(rpc: &dyn ChainProvider, sig: &str) -> Result<serde_json::Value, Error> {
    let params = serde_json::json!([
        sig,
        { "encoding": "jsonParsed", "maxSupportedTransactionVersion": 0 }
    ])
    .to_string();
    let raw = rpc.call(Chain::Solana, "getTransaction", params).await?;
    let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("getTransaction not JSON: {e}"),
    })?;
    match json.get("result") {
        Some(r) if !r.is_null() => Ok(r.clone()),
        _ => Err(Error::Transient {
            message: format!("getTransaction {sig} returned a null result"),
            retry_after: None,
        }),
    }
}

/// Signed (`post - pre`) wallet-owned token deltas for both pool legs, with a native-SOL fallback
/// when a WSOL leg's SPL delta is zero (wrap/unwrap in the same tx).
fn signed_token_deltas(
    tx: &serde_json::Value,
    wallet: &str,
    mint_a: &str,
    mint_b: &str,
) -> (Decimal, Decimal) {
    let meta = tx.get("meta");
    let pre = meta
        .and_then(|m| m.get("preTokenBalances"))
        .and_then(|v| v.as_array());
    let post = meta
        .and_then(|m| m.get("postTokenBalances"))
        .and_then(|v| v.as_array());
    let (Some(pre), Some(post)) = (pre, post) else {
        return (Decimal::ZERO, Decimal::ZERO);
    };
    let delta = |mint: &str| sum_ui_amount(post, wallet, mint) - sum_ui_amount(pre, wallet, mint);
    let mut da = delta(mint_a);
    let mut db = delta(mint_b);
    if mint_a == WSOL_MINT && da.is_zero() {
        da = native_sol_delta(tx, wallet).unwrap_or(Decimal::ZERO);
    }
    if mint_b == WSOL_MINT && db.is_zero() {
        db = native_sol_delta(tx, wallet).unwrap_or(Decimal::ZERO);
    }
    (da, db)
}

/// Signed native-SOL delta for `wallet` (post + fee - pre), in SOL.
fn native_sol_delta(tx: &serde_json::Value, wallet: &str) -> Option<Decimal> {
    let keys = tx.pointer("/transaction/message/accountKeys")?.as_array()?;
    let meta = tx.get("meta")?;
    let pre = meta.get("preBalances")?.as_array()?;
    let post = meta.get("postBalances")?.as_array()?;
    let fee = meta.get("fee").and_then(|f| f.as_u64()).unwrap_or(0);
    let idx = keys.iter().position(|k| {
        let pk = if k.is_string() {
            k.as_str().unwrap_or("")
        } else {
            k.get("pubkey").and_then(|p| p.as_str()).unwrap_or("")
        };
        pk == wallet
    })?;
    let pre_l = pre.get(idx)?.as_u64()?;
    let post_l = post.get(idx)?.as_u64()?;
    let delta = post_l as i128 + fee as i128 - pre_l as i128;
    Decimal::try_from_i128_with_scale(delta, 9).ok()
}

/// Sum `uiAmountString` across all balance entries owned by `wallet` for `mint`.
fn sum_ui_amount(balances: &[serde_json::Value], wallet: &str, mint: &str) -> Decimal {
    balances
        .iter()
        .filter(|b| {
            b.get("owner").and_then(|o| o.as_str()) == Some(wallet)
                && b.get("mint").and_then(|m| m.as_str()) == Some(mint)
        })
        .filter_map(|b| {
            b.pointer("/uiTokenAmount/uiAmountString")
                .and_then(|a| a.as_str())
                .and_then(|s| Decimal::from_str_exact(s).ok())
        })
        .sum()
}

/// Classify a transaction by its Raydium CLMM instruction discriminator, returning the event kind and
/// the positive two-token amounts in the direction the kind implies. `None` when no discriminator
/// matches or the delta is zero.
fn classify_discriminator(
    tx: &serde_json::Value,
    wallet: &str,
    mint_a: &str,
    mint_b: &str,
) -> Option<(EventKind, Decimal, Decimal)> {
    let kind = detect_clmm_event_kind(tx)?;
    let (da, db) = signed_token_deltas(tx, wallet, mint_a, mint_b);
    let (a, b) = match kind {
        // A deposit is the user paying in (negative delta); withdraw/collect is receiving (positive).
        ClmmEventKind::Deposit => ((-da).max(Decimal::ZERO), (-db).max(Decimal::ZERO)),
        ClmmEventKind::Withdraw | ClmmEventKind::Collect => {
            (da.max(Decimal::ZERO), db.max(Decimal::ZERO))
        }
    };
    if a.is_zero() && b.is_zero() {
        return None;
    }
    let ek = match kind {
        ClmmEventKind::Deposit => EventKind::Deposit,
        ClmmEventKind::Withdraw => EventKind::Withdraw,
        ClmmEventKind::Collect => EventKind::CollectFees,
    };
    Some((ek, a, b))
}

/// Fee-collect heuristic: a transaction that isn't a classified liquidity change but credits the
/// wallet in either pool token is treated as a fee collect. `None` when nothing was received.
fn classify_collect_heuristic(
    tx: &serde_json::Value,
    wallet: &str,
    mint_a: &str,
    mint_b: &str,
) -> Option<(Decimal, Decimal)> {
    let (da, db) = signed_token_deltas(tx, wallet, mint_a, mint_b);
    let a = da.max(Decimal::ZERO);
    let b = db.max(Decimal::ZERO);
    if a.is_zero() && b.is_zero() {
        None
    } else {
        Some((a, b))
    }
}

/// Emit a two-token movement as paired single-token events (one per non-zero leg), sharing kind, tx
/// and timestamp. The consumer re-pairs by token mint address.
#[allow(
    clippy::too_many_arguments,
    reason = "internal helper appending a token-pair event to the output sink; a mix of event data and the accumulator, not a cohesive struct"
)]
fn push_pair(
    events: &mut Vec<HistoryEvent>,
    kind: EventKind,
    token_a: &Token,
    token_b: &Token,
    a: Decimal,
    b: Decimal,
    tx: &str,
    at: i64,
) -> Result<(), Error> {
    if a > Decimal::ZERO {
        events.push(HistoryEvent::new(
            kind,
            Amount::from_decimal(token_a.clone(), a)?,
            tx,
            Timestamp(at),
        ));
    }
    if b > Decimal::ZERO {
        events.push(HistoryEvent::new(
            kind,
            Amount::from_decimal(token_b.clone(), b)?,
            tx,
            Timestamp(at),
        ));
    }
    Ok(())
}

/// Raydium CLMM liquidity-changing instruction kinds, by Anchor discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClmmEventKind {
    Deposit,
    Withdraw,
    Collect,
}

/// The 8-byte Anchor instruction discriminator for a method name (`sha256("global:<method>")[..8]`).
fn anchor_discriminator(method: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!("global:{method}").as_bytes());
    let hash = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash[..8]);
    out
}

/// Inspect a parsed transaction's instructions (top-level + inner) for a Raydium CLMM
/// liquidity-changing call. `open_position*`/`increase_liquidity*` → Deposit, `decrease_liquidity*` →
/// Withdraw, and a `decrease_liquidity` whose `liquidity_amount == 0` → Collect (the UI's "Claim
/// Fees" flushes fees via decrease_liquidity(0)).
fn detect_clmm_event_kind(tx: &serde_json::Value) -> Option<ClmmEventKind> {
    let deposit_discs: [[u8; 8]; 6] = [
        anchor_discriminator("open_position"),
        anchor_discriminator("open_position_v2"),
        anchor_discriminator("open_position_with_metadata"),
        anchor_discriminator("open_position_with_token22_nft"),
        anchor_discriminator("increase_liquidity"),
        anchor_discriminator("increase_liquidity_v2"),
    ];
    let withdraw_discs: [[u8; 8]; 2] = [
        anchor_discriminator("decrease_liquidity"),
        anchor_discriminator("decrease_liquidity_v2"),
    ];

    let check = |data_b58: &str| -> Option<ClmmEventKind> {
        let bytes = bs58::decode(data_b58).into_vec().ok()?;
        if bytes.len() < 8 {
            return None;
        }
        let disc = &bytes[..8];
        if deposit_discs.iter().any(|d| disc == d) {
            return Some(ClmmEventKind::Deposit);
        }
        if withdraw_discs.iter().any(|d| disc == d) {
            // decrease_liquidity data after the discriminator: liquidity_amount u128 (16 bytes LE).
            // A zero liquidity_amount is a fee flush, not a withdraw.
            if bytes.len() >= 24 {
                let mut lq = [0u8; 16];
                lq.copy_from_slice(&bytes[8..24]);
                if u128::from_le_bytes(lq) == 0 {
                    return Some(ClmmEventKind::Collect);
                }
            }
            return Some(ClmmEventKind::Withdraw);
        }
        None
    };

    let scan = |arr: &[serde_json::Value]| -> Option<ClmmEventKind> {
        for ix in arr {
            if ix.get("programId").and_then(|p| p.as_str()) != Some(CLMM_PROGRAM_ID) {
                continue;
            }
            if let Some(kind) = ix.get("data").and_then(|d| d.as_str()).and_then(check) {
                return Some(kind);
            }
        }
        None
    };

    if let Some(arr) = tx
        .pointer("/transaction/message/instructions")
        .and_then(|i| i.as_array())
    {
        if let Some(kind) = scan(arr) {
            return Some(kind);
        }
    }
    if let Some(inner) = tx
        .pointer("/meta/innerInstructions")
        .and_then(|i| i.as_array())
    {
        for entry in inner {
            if let Some(arr) = entry.get("instructions").and_then(|i| i.as_array()) {
                if let Some(kind) = scan(arr) {
                    return Some(kind);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use base64::Engine;
    use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
    use std::sync::Arc;

    const WALLET: &str = "Wa11etPubkey1111111111111111111111111111111";
    const NFT: &str = "NftMintPubkey111111111111111111111111111111";
    const MINT_A: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"; // 6 decimals (USDT)
    const MINT_B: &str = "So11111111111111111111111111111111111111112"; // 9 decimals (WSOL)

    fn mint_account(decimals: u8) -> String {
        let mut data = vec![0u8; 82];
        data[44] = decimals; // SPL mint layout: decimals at byte 44
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"value":{{"data":["{b64}","base64"]}}}}}}"#)
    }

    fn disc_b58(method: &str) -> String {
        bs58::encode(anchor_discriminator(method)).into_string()
    }

    // An increase_liquidity deposit: the user pays both tokens in (post < pre).
    fn deposit_tx() -> String {
        let data = disc_b58("increase_liquidity");
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{
              "blockTime":2000,
              "transaction":{{"message":{{"instructions":[
                {{"programId":"{CLMM_PROGRAM_ID}","data":"{data}"}}
              ]}}}},
              "meta":{{"err":null,
                "preTokenBalances":[
                  {{"owner":"{WALLET}","mint":"{MINT_A}","uiTokenAmount":{{"uiAmountString":"10"}}}},
                  {{"owner":"{WALLET}","mint":"{MINT_B}","uiTokenAmount":{{"uiAmountString":"1000"}}}}],
                "postTokenBalances":[
                  {{"owner":"{WALLET}","mint":"{MINT_A}","uiTokenAmount":{{"uiAmountString":"8"}}}},
                  {{"owner":"{WALLET}","mint":"{MINT_B}","uiTokenAmount":{{"uiAmountString":"500"}}}}]
              }}
            }}}}"#
        )
    }

    // A fee collect detected by the heuristic: no CLMM discriminator instruction, wallet is credited.
    fn collect_tx() -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{
              "blockTime":3000,
              "transaction":{{"message":{{"instructions":[
                {{"programId":"SomeOtherProgram1111111111111111111111111","data":"AA"}}
              ]}}}},
              "meta":{{"err":null,
                "preTokenBalances":[
                  {{"owner":"{WALLET}","mint":"{MINT_A}","uiTokenAmount":{{"uiAmountString":"0"}}}}],
                "postTokenBalances":[
                  {{"owner":"{WALLET}","mint":"{MINT_A}","uiTokenAmount":{{"uiAmountString":"0.5"}}}}]
              }}
            }}}}"#
        )
    }

    const SIGS: &str = r#"{"jsonrpc":"2.0","id":1,"result":[
      {"signature":"sigCol","blockTime":3000},
      {"signature":"sigDep","blockTime":2000}
    ]}"#;

    fn ctx() -> Ctx {
        let rpc = MockChainProvider::new()
            .on(Match::method("getSignaturesForAddress"), SIGS)
            .on(Match::body_contains("sigDep"), deposit_tx())
            .on(Match::body_contains("sigCol"), collect_tx())
            .on(Match::body_contains(MINT_A), mint_account(6))
            .on(Match::body_contains(MINT_B), mint_account(9));
        Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc))
    }

    async fn history(cx: &Ctx, since: Option<Timestamp>) -> History {
        RaydiumClmm::new()
            .read_history(
                &Wallet::Solana(WALLET.to_string()),
                Chain::Solana,
                Some(&format!("{NFT}|{MINT_A}|{MINT_B}")),
                since,
                cx,
            )
            .await
            .expect("read_history")
            .into_inner()
    }

    #[tokio::test]
    async fn reconstructs_deposit_and_collect_from_chain() {
        let h = history(&ctx(), None).await;
        assert_eq!(h.protocol, Protocol::Raydium);
        // deposit: two legs (MINT_A, MINT_B); collect: one leg (MINT_A) → 3 events, oldest first.
        assert_eq!(h.events.len(), 3);

        let find = |kind: EventKind, mint: &str| {
            h.events
                .iter()
                .find(|e| {
                    e.kind == kind
                        && e.amount
                            .token
                            .address
                            .as_ref()
                            .map(|a| a.to_string())
                            .as_deref()
                            == Some(mint)
                })
                .unwrap_or_else(|| panic!("missing {kind:?} {mint}"))
                .clone()
        };

        // Deposit = tokens paid in (pre - post): 10-8=2 MINT_A, 1000-500=500 MINT_B.
        assert_eq!(
            find(EventKind::Deposit, MINT_A).amount.amount,
            Decimal::from_str_exact("2").unwrap()
        );
        assert_eq!(
            find(EventKind::Deposit, MINT_B).amount.amount,
            Decimal::from_str_exact("500").unwrap()
        );
        let collect = find(EventKind::CollectFees, MINT_A);
        assert_eq!(
            collect.amount.amount,
            Decimal::from_str_exact("0.5").unwrap()
        );
        assert_eq!(collect.tx, "sigCol");
        assert_eq!(collect.at, Timestamp(3000));
        // sorted oldest-first: the 2000 deposit legs precede the 3000 collect.
        assert_eq!(h.events.last().unwrap().at, Timestamp(3000));
    }

    #[tokio::test]
    async fn since_filters_out_older_events() {
        // since = 2500 drops the 2000 deposit, keeps only the 3000 collect.
        let h = history(&ctx(), Some(Timestamp(2500))).await;
        assert_eq!(h.events.len(), 1);
        assert_eq!(h.events[0].kind, EventKind::CollectFees);
    }

    #[tokio::test]
    async fn history_requires_a_full_selector() {
        let err = RaydiumClmm::new()
            .read_history(
                &Wallet::Solana(WALLET.to_string()),
                Chain::Solana,
                Some("onlyNft"),
                None,
                &ctx(),
            )
            .await
            .expect_err("needs nftMint|mintA|mintB");
        assert!(!err.is_retryable());
    }
}
