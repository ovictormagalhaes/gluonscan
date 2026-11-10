//! # gluonscan-raydium
//!
//! Raydium concentrated-liquidity (CLMM) adapter, [`Source::OnChain`]: discovers position NFTs
//! from the wallet's token accounts, derives each position PDA, and decodes the on-chain position,
//! pool and mint accounts into a [`LiquidityPosition`] (principal amounts via the Q64.96 math,
//! in-range from the pool tick).
//!
//! Uncollected fees are what a fee collect would pay now: the position's owed fields plus the fees
//! accrued since its last checkpoint. The program only moves `token_fees_owed_0/1` when the
//! position's liquidity changes, so the accrual is computed the way the program does it, from the
//! pool's global fee growth and the fee growth outside the position's two ticks (read from their
//! tick arrays). The position, its pool and those tick arrays are read in one `getMultipleAccounts`
//! call, so they all come from the same slot. A position with liquidity whose tick arrays cannot be
//! read, or whose accounts disagree, fails the read rather than reporting the owed fields alone.
//!
//! The account byte offsets match the on-chain Raydium CLMM `PersonalPositionState`, `PoolState`
//! and `TickArrayState` layouts. Discovery queries both the classic SPL Token program and
//! Token-2022 (Raydium's newer position NFTs mint under Token-2022).

use alloy_primitives::U256;
use async_trait::async_trait;
use gluonscan_core::{
    scaled, Amount, Capability, Chain, ChainProvider, CollectBasis, Complete, Ctx, Detail, Error,
    EventKind, History, HistoryEvent, LiquidityPosition, Position, PositionStatus, Protocol,
    ProtocolAdapter, Provenance, Reading, Source, Staleness, Timestamp, Token, Wallet,
};
use gluonscan_math::{
    get_amounts_for_liquidity, get_sqrt_ratio_at_tick, is_in_range, MAX_TICK, MIN_TICK,
};
use gluonscan_solana::{
    find_program_address, get_account_info, get_mint_decimals, get_multiple_accounts,
    get_token_accounts_by_owner, pubkey_bytes, pubkey_str, RAYDIUM_CLMM_PROGRAM,
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
const POS_FEE_GROWTH_INSIDE0: usize = 97;
const POS_FEE_GROWTH_INSIDE1: usize = 113;
const POS_FEE0: usize = 129;
const POS_FEE1: usize = 137;

// PoolState layout (byte offsets): token_mint_0[73..105], token_mint_1[105..137],
// mint_decimals_0[233], mint_decimals_1[234], tick_spacing[235..237], liquidity[237..253],
// sqrt_price_x64[253..269], tick_current[269..273], 4 padding bytes,
// fee_growth_global_0_x64[277..293], fee_growth_global_1_x64[293..309].
const POOL_MIN_LEN: usize = 309;
const POOL_MINT0: usize = 73;
const POOL_MINT1: usize = 105;
const POOL_DEC0: usize = 233;
const POOL_DEC1: usize = 234;
const POOL_TICK_SPACING: usize = 235;
const POOL_SQRT_X64: usize = 253;
const POOL_TICK: usize = 269;
const POOL_FEE_GROWTH_GLOBAL0: usize = 277;
const POOL_FEE_GROWTH_GLOBAL1: usize = 293;

// TickArrayState layout (byte offsets): 8-byte discriminator, pool_id[8..40],
// start_tick_index[40..44], then 60 TickState entries of 168 bytes each from 44. TickState:
// tick[0..4], liquidity_net[4..20], liquidity_gross[20..36], fee_growth_outside_0_x64[36..52],
// fee_growth_outside_1_x64[52..68]. PDA: ["tick_array", pool_id, start_tick_index big-endian].
const TICK_ARRAY_SIZE: i32 = 60;
const TICK_ARRAY_POOL: usize = 8;
const TICK_ARRAY_START: usize = 40;
const TICK_ARRAY_TICKS: usize = 44;
const TICK_STATE_LEN: usize = 168;
const TICK_ARRAY_MIN_LEN: usize = TICK_ARRAY_TICKS + TICK_ARRAY_SIZE as usize * TICK_STATE_LEN;
const TICK_INDEX: usize = 0;
const TICK_LIQUIDITY_GROSS: usize = 20;
const TICK_FEE_GROWTH_OUTSIDE0: usize = 36;
const TICK_FEE_GROWTH_OUTSIDE1: usize = 52;

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
            let address = pubkey_str(&pda);
            let Some(data) = get_account_info(rpc, &address).await? else {
                continue; // no position PDA → this NFT is not a Raydium position
            };
            if data.len() < POS_MIN_LEN {
                continue;
            }
            positions.push(parse_position(rpc, &address, &data, &mint).await?);
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
    /// collect. Each two-token movement becomes paired single-token events. A collect whose
    /// transaction makes no CLMM liquidity change but `decrease_liquidity(0)` released no principal
    /// and is [`CollectBasis::FeesOnly`]; any other collect (one the heuristic found, or one beside
    /// another liquidity change) is [`CollectBasis::Unknown`]. Fails closed (ADR-016):
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
                let only_fee_flushes = clmm_event_kinds(&tx)
                    .iter()
                    .all(|k| *k == ClmmEventKind::Collect);
                let basis = if kind == EventKind::CollectFees && only_fee_flushes {
                    CollectBasis::FeesOnly
                } else {
                    CollectBasis::Unknown
                };
                push_pair(
                    &mut events,
                    kind,
                    &token_a,
                    &token_b,
                    a0,
                    a1,
                    sig,
                    at,
                    basis,
                )?;
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
                    CollectBasis::Unknown,
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

/// Decode one position. The discovery copy of the position account only locates the accounts
/// (its pool and ticks never change); every value is then taken from one `getMultipleAccounts`
/// call holding the position, its pool and, when it has liquidity, the tick arrays of its two
/// ticks, so they all come from the same slot. Read apart, a swap crossing one of the ticks
/// between the reads flips that tick's fee growth outside while the pool keeps its old current
/// tick and global growth, and the fee growth inside comes out wrong. The position is part of the
/// same call because a fee collect or liquidity change between the reads moves its checkpoint,
/// owed fields and liquidity together with the pool and ticks. The tick array addresses depend on
/// the pool's tick spacing, so a position with liquidity reads its pool once before the snapshot,
/// for the spacing only.
async fn parse_position(
    rpc: &dyn gluonscan_core::ChainProvider,
    address: &str,
    discovered: &[u8],
    nft_mint: &str,
) -> Result<Position, Error> {
    let pool_id = pubkey_at(discovered, POS_POOL)?;
    let tick_lower = i32_at(discovered, POS_TICK_LOWER)?;
    let tick_upper = i32_at(discovered, POS_TICK_UPPER)?;
    if !(MIN_TICK..=MAX_TICK).contains(&tick_lower) || !(MIN_TICK..=MAX_TICK).contains(&tick_upper)
    {
        return Err(Error::Integrity {
            message: format!("Raydium ticks out of range: {tick_lower}..{tick_upper}"),
        });
    }
    let pool_address = pubkey_str(&pool_id);

    // Without liquidity nothing accrues and the change that emptied the position settled its owed
    // fields; its ticks may no longer be initialized, so no tick array is read.
    let tick_arrays = if u128_at(discovered, POS_LIQUIDITY)? == 0 {
        None
    } else {
        let spacing_pool = get_account_info(rpc, &pool_address)
            .await?
            .filter(|d| d.len() >= POOL_MIN_LEN)
            .ok_or_else(pool_missing)?;
        let tick_spacing = u16_at(&spacing_pool, POOL_TICK_SPACING)?;
        if tick_spacing == 0 {
            return Err(Error::Integrity {
                message: "Raydium pool tick spacing is 0".into(),
            });
        }
        let lower_start = tick_array_start_index(tick_lower, tick_spacing);
        let upper_start = tick_array_start_index(tick_upper, tick_spacing);
        Some(TickArrays {
            tick_spacing,
            lower: (lower_start, tick_array_address(&pool_id, lower_start)?),
            upper: (upper_start, tick_array_address(&pool_id, upper_start)?),
        })
    };

    let mut keys = vec![address, pool_address.as_str()];
    if let Some(arrays) = &tick_arrays {
        keys.push(&arrays.lower.1);
        if arrays.upper.1 != arrays.lower.1 {
            keys.push(&arrays.upper.1);
        }
    }
    let mut snapshot = get_multiple_accounts(rpc, &keys).await?.into_iter();
    let data = snapshot
        .next()
        .flatten()
        .filter(|d| d.len() >= POS_MIN_LEN)
        .ok_or_else(|| Error::Transient {
            message: format!("Raydium position {address} closed between reads"),
            retry_after: None,
        })?;
    if pubkey_at(&data, POS_POOL)? != pool_id
        || i32_at(&data, POS_TICK_LOWER)? != tick_lower
        || i32_at(&data, POS_TICK_UPPER)? != tick_upper
    {
        return Err(Error::Integrity {
            message: format!("Raydium position {address} changed its pool or range between reads"),
        });
    }
    let pool = snapshot
        .next()
        .flatten()
        .filter(|d| d.len() >= POOL_MIN_LEN)
        .ok_or_else(pool_missing)?;
    let liquidity = u128_at(&data, POS_LIQUIDITY)?;
    let owed0 = u64_at(&data, POS_FEE0)?;
    let owed1 = u64_at(&data, POS_FEE1)?;
    let tick_current = i32_at(&pool, POOL_TICK)?;
    let sqrt_x64 = u128_at(&pool, POOL_SQRT_X64)?;
    // The two mints and their decimals live in the pool account itself, so no extra mint reads.
    let mint0 = pubkey_str(&pubkey_at(&pool, POOL_MINT0)?);
    let mint1 = pubkey_str(&pubkey_at(&pool, POOL_MINT1)?);
    let dec0 = *pool.get(POOL_DEC0).ok_or_else(short)?;
    let dec1 = *pool.get(POOL_DEC1).ok_or_else(short)?;
    let token0 = Token::solana(String::new(), Some(mint0), dec0);
    let token1 = Token::solana(String::new(), Some(mint1), dec1);

    let (fee0, fee1) = match tick_arrays {
        _ if liquidity == 0 => (owed0, owed1),
        None => {
            return Err(Error::Transient {
                message: format!("Raydium position {address} gained liquidity between reads"),
                retry_after: None,
            })
        }
        Some(arrays) => {
            let tick_spacing = arrays.tick_spacing;
            if u16_at(&pool, POOL_TICK_SPACING)? != tick_spacing {
                return Err(Error::Integrity {
                    message: "Raydium pool tick spacing changed between reads".into(),
                });
            }
            let (lower_start, lower_address) = &arrays.lower;
            let (upper_start, upper_address) = &arrays.upper;
            let lower_array = checked_tick_array(
                snapshot.next().flatten(),
                lower_address,
                &pool_id,
                *lower_start,
            )?;
            let upper_array = if upper_address == lower_address {
                lower_array.clone()
            } else {
                checked_tick_array(
                    snapshot.next().flatten(),
                    upper_address,
                    &pool_id,
                    *upper_start,
                )?
            };
            let lower = tick_fee_growth(&lower_array, *lower_start, tick_lower, tick_spacing)?;
            let upper = tick_fee_growth(&upper_array, *upper_start, tick_upper, tick_spacing)?;
            let global = [
                u128_at(&pool, POOL_FEE_GROWTH_GLOBAL0)?,
                u128_at(&pool, POOL_FEE_GROWTH_GLOBAL1)?,
            ];
            let inside =
                fee_growth_inside(tick_current, tick_lower, tick_upper, lower, upper, global);
            (
                claimable_fees(
                    owed0,
                    u128_at(&data, POS_FEE_GROWTH_INSIDE0)?,
                    inside[0],
                    liquidity,
                )?,
                claimable_fees(
                    owed1,
                    u128_at(&data, POS_FEE_GROWTH_INSIDE1)?,
                    inside[1],
                    liquidity,
                )?,
            )
        }
    };

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

/// The start index of the tick array holding `tick` (`TickArrayState::get_array_start_index`):
/// arrays cover 60 initializable ticks, and negative ticks round down to the array below.
fn tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    let ticks_in_array = TICK_ARRAY_SIZE * i32::from(tick_spacing);
    tick.div_euclid(ticks_in_array) * ticks_in_array
}

/// The tick spacing a position's tick arrays were located with, and each array's start index and
/// address (`lower` and `upper` are the same array when both ticks fall in it).
struct TickArrays {
    tick_spacing: u16,
    lower: (i32, String),
    upper: (i32, String),
}

/// The address of the pool's tick array starting at `start`: PDA `["tick_array", pool, start
/// big-endian]` under the CLMM program.
fn tick_array_address(pool_id: &[u8; 32], start: i32) -> Result<String, Error> {
    let program = pubkey_bytes(RAYDIUM_CLMM_PROGRAM)?;
    let (pda, _) = find_program_address(&[b"tick_array", pool_id, &start.to_be_bytes()], &program);
    Ok(pubkey_str(&pda))
}

/// The pool's tick array starting at `start`, as read. A missing, short or foreign account fails
/// closed, since without it the accrued fees are unknown.
fn checked_tick_array(
    data: Option<Vec<u8>>,
    address: &str,
    pool_id: &[u8; 32],
    start: i32,
) -> Result<Vec<u8>, Error> {
    let data = data.ok_or_else(|| Error::Integrity {
        message: format!("Raydium tick array {address} (start {start}) missing"),
    })?;
    if data.len() < TICK_ARRAY_MIN_LEN
        || pubkey_at(&data, TICK_ARRAY_POOL)? != *pool_id
        || i32_at(&data, TICK_ARRAY_START)? != start
    {
        return Err(Error::Integrity {
            message: format!(
                "Raydium tick array {address} is not the pool's array starting at {start}"
            ),
        });
    }
    Ok(data)
}

/// `[fee_growth_outside_0_x64, fee_growth_outside_1_x64]` of one position tick. The tick must be
/// initialized: a position with liquidity always references its two ticks.
fn tick_fee_growth(
    array: &[u8],
    start: i32,
    tick: i32,
    tick_spacing: u16,
) -> Result<[u128; 2], Error> {
    let offset = (tick - start) / i32::from(tick_spacing);
    let base = TICK_ARRAY_TICKS + offset as usize * TICK_STATE_LEN;
    if i32_at(array, base + TICK_INDEX)? != tick
        || u128_at(array, base + TICK_LIQUIDITY_GROSS)? == 0
    {
        return Err(Error::Integrity {
            message: format!("Raydium tick {tick} is not initialized in its tick array"),
        });
    }
    Ok([
        u128_at(array, base + TICK_FEE_GROWTH_OUTSIDE0)?,
        u128_at(array, base + TICK_FEE_GROWTH_OUTSIDE1)?,
    ])
}

/// Fee growth per unit of liquidity inside `[tick_lower, tick_upper)`, both tokens, as the program's
/// `get_fee_growth_inside`: a tick's outside value counts below (lower) or above (upper) the range
/// when the current tick is on the range's side of it, else its complement from the global growth.
/// All Q64.64 growth values are modular, so every step wraps.
fn fee_growth_inside(
    tick_current: i32,
    tick_lower: i32,
    tick_upper: i32,
    lower_outside: [u128; 2],
    upper_outside: [u128; 2],
    global: [u128; 2],
) -> [u128; 2] {
    [0, 1].map(|i| {
        let below = if tick_current >= tick_lower {
            lower_outside[i]
        } else {
            global[i].wrapping_sub(lower_outside[i])
        };
        let above = if tick_current < tick_upper {
            upper_outside[i]
        } else {
            global[i].wrapping_sub(upper_outside[i])
        };
        global[i].wrapping_sub(below).wrapping_sub(above)
    })
}

/// What a fee collect pays now, as the program's `calculate_latest_token_fees`: the owed fields
/// plus `liquidity * (inside_now - inside_last) / 2^64`, the growth delta wrapping. The program
/// credits nothing for an accrual of `u64::MAX` or more (and refuses one above `u128`), but no SPL
/// token amount gets that large: such an accrual means the growth values disagree, so the read
/// fails rather than report the owed fields alone.
fn claimable_fees(
    owed: u64,
    inside_last: u128,
    inside_now: u128,
    liquidity: u128,
) -> Result<u64, Error> {
    let accrued: U256 =
        (U256::from(inside_now.wrapping_sub(inside_last)) * U256::from(liquidity)) >> 64usize;
    if accrued >= U256::from(u64::MAX) {
        return Err(Error::Integrity {
            message: format!(
                "Raydium accrued fee {accrued} is not below u64::MAX; the fee growth reads disagree"
            ),
        });
    }
    owed.checked_add(accrued.to::<u64>())
        .ok_or_else(|| Error::Integrity {
            message: "Raydium owed plus accrued fees overflow u64".into(),
        })
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

fn pool_missing() -> Error {
    Error::Integrity {
        message: "Raydium pool account missing or too short".into(),
    }
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

fn u16_at(d: &[u8], o: usize) -> Result<u16, Error> {
    d.get(o..o + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
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

/// Emit a two-token movement as paired single-token events (one per non-zero leg), sharing kind, tx,
/// timestamp and collect basis. The consumer re-pairs by token mint address.
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
    basis: CollectBasis,
) -> Result<(), Error> {
    if a > Decimal::ZERO {
        events.push(
            HistoryEvent::new(
                kind,
                Amount::from_decimal(token_a.clone(), a)?,
                tx,
                Timestamp(at),
            )
            .with_collect_basis(basis),
        );
    }
    if b > Decimal::ZERO {
        events.push(
            HistoryEvent::new(
                kind,
                Amount::from_decimal(token_b.clone(), b)?,
                tx,
                Timestamp(at),
            )
            .with_collect_basis(basis),
        );
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
/// Fees" flushes fees via decrease_liquidity(0)). The first such call, top-level before inner.
fn detect_clmm_event_kind(tx: &serde_json::Value) -> Option<ClmmEventKind> {
    clmm_event_kinds(tx).first().copied()
}

/// Every Raydium CLMM liquidity-changing call in a parsed transaction, top-level first, then inner,
/// classified as in [`detect_clmm_event_kind`].
fn clmm_event_kinds(tx: &serde_json::Value) -> Vec<ClmmEventKind> {
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

    let mut kinds = Vec::new();
    let mut scan = |arr: &[serde_json::Value]| {
        for ix in arr {
            if ix.get("programId").and_then(|p| p.as_str()) != Some(CLMM_PROGRAM_ID) {
                continue;
            }
            if let Some(kind) = ix.get("data").and_then(|d| d.as_str()).and_then(check) {
                kinds.push(kind);
            }
        }
    };

    if let Some(arr) = tx
        .pointer("/transaction/message/instructions")
        .and_then(|i| i.as_array())
    {
        scan(arr);
    }
    if let Some(inner) = tx
        .pointer("/meta/innerInstructions")
        .and_then(|i| i.as_array())
    {
        for entry in inner {
            if let Some(arr) = entry.get("instructions").and_then(|i| i.as_array()) {
                scan(arr);
            }
        }
    }
    kinds
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
        assert_eq!(
            collect.collect_basis,
            CollectBasis::Unknown,
            "a credit the heuristic read as a collect is not known to be fees only"
        );
        assert_eq!(collect.released_principal, None);
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

    fn decrease_ix(liquidity: u128) -> String {
        let mut data = anchor_discriminator("decrease_liquidity_v2").to_vec();
        data.extend_from_slice(&liquidity.to_le_bytes());
        data.extend_from_slice(&[0u8; 16]);
        format!(
            r#"{{"programId":"{CLMM_PROGRAM_ID}","data":"{}"}}"#,
            bs58::encode(data).into_string()
        )
    }

    // The wallet is credited 0.5 USDT and 2 WSOL by the given CLMM instructions.
    fn credited_tx(instructions: &[String]) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{
              "blockTime":3000,
              "transaction":{{"message":{{"instructions":[{}]}}}},
              "meta":{{"err":null,
                "preTokenBalances":[
                  {{"owner":"{WALLET}","mint":"{MINT_A}","uiTokenAmount":{{"uiAmountString":"0"}}}},
                  {{"owner":"{WALLET}","mint":"{MINT_B}","uiTokenAmount":{{"uiAmountString":"0"}}}}],
                "postTokenBalances":[
                  {{"owner":"{WALLET}","mint":"{MINT_A}","uiTokenAmount":{{"uiAmountString":"0.5"}}}},
                  {{"owner":"{WALLET}","mint":"{MINT_B}","uiTokenAmount":{{"uiAmountString":"2"}}}}]
              }}
            }}}}"#,
            instructions.join(",")
        )
    }

    async fn collects_of(tx: String) -> Vec<HistoryEvent> {
        let rpc = MockChainProvider::new()
            .on(
                Match::method("getSignaturesForAddress"),
                r#"{"jsonrpc":"2.0","id":1,"result":[{"signature":"sigX","blockTime":3000}]}"#,
            )
            .on(Match::body_contains("sigX"), tx)
            .on(Match::body_contains(MINT_A), mint_account(6))
            .on(Match::body_contains(MINT_B), mint_account(9));
        let cx =
            Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
        history(&cx, None).await.events
    }

    #[tokio::test]
    async fn a_fee_flush_releases_no_principal() {
        let events = collects_of(credited_tx(&[decrease_ix(0)])).await;

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.kind == EventKind::CollectFees
            && e.collect_basis == CollectBasis::FeesOnly
            && e.released_principal.is_none()));
    }

    #[tokio::test]
    async fn a_fee_flush_beside_a_liquidity_decrease_is_not_fees_only() {
        let events = collects_of(credited_tx(&[decrease_ix(0), decrease_ix(1_000)])).await;

        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|e| e.kind == EventKind::CollectFees && e.collect_basis == CollectBasis::Unknown));
    }

    #[tokio::test]
    async fn a_liquidity_decrease_is_a_withdraw_with_no_collect_basis() {
        let events = collects_of(credited_tx(&[decrease_ix(1_000)])).await;

        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|e| e.kind == EventKind::Withdraw && e.collect_basis == CollectBasis::Unknown));
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

#[cfg(test)]
mod fee_tests {
    use super::*;

    const Q64: u128 = 1 << 64;

    #[test]
    fn tick_array_start_rounds_negative_ticks_down() {
        assert_eq!(tick_array_start_index(0, 1), 0);
        assert_eq!(tick_array_start_index(59, 1), 0);
        assert_eq!(tick_array_start_index(60, 1), 60);
        assert_eq!(tick_array_start_index(-1, 1), -60);
        assert_eq!(tick_array_start_index(-60, 1), -60);
        assert_eq!(tick_array_start_index(-61, 1), -120);
        assert_eq!(tick_array_start_index(-30161, 1), -30180);
        assert_eq!(tick_array_start_index(-17721, 1), -17760);
        assert_eq!(tick_array_start_index(-600, 10), -600);
        assert_eq!(tick_array_start_index(-610, 10), -1200);
        // Raydium's MIN_TICK
        assert_eq!(tick_array_start_index(-443636, 1), -443640);
    }

    #[test]
    fn inside_growth_in_range_uses_both_outside_values() {
        let inside = fee_growth_inside(0, -10, 10, [100, 7], [50, 3], [1000, 20]);
        assert_eq!(inside, [1000 - 100 - 50, 20 - 7 - 3]);
    }

    #[test]
    fn inside_growth_below_range_takes_the_lower_tick_complement() {
        // below(lower) = global - outside(lower); above(upper) = outside(upper)
        let inside = fee_growth_inside(-20, -10, 10, [900, 15], [50, 2], [1000, 20]);
        assert_eq!(inside, [1000 - (1000 - 900) - 50, 20 - (20 - 15) - 2]);
        assert_eq!(inside, [850, 13]);
    }

    #[test]
    fn inside_growth_above_range_takes_the_upper_tick_complement() {
        // the upper bound is exclusive: tick_current == tick_upper is above the range
        let inside = fee_growth_inside(10, -10, 10, [100, 1], [700, 12], [1000, 20]);
        assert_eq!(inside, [1000 - 100 - (1000 - 700), 20 - 1 - (20 - 12)]);
        assert_eq!(inside, [600, 11]);
    }

    #[test]
    fn inside_growth_wraps_when_outside_values_exceed_global() {
        // Outside values are relative to when a tick was initialized and may exceed the global
        // growth; the program's arithmetic is modular.
        let inside = fee_growth_inside(0, -10, 10, [30, 0], [5, 0], [20, 0]);
        assert_eq!(inside[0], 20u128.wrapping_sub(30).wrapping_sub(5));
        assert_eq!(inside[0], u128::MAX - 14);
    }

    #[test]
    fn claimable_fees_at_the_checkpoint_are_exactly_owed() {
        assert_eq!(
            claimable_fees(1234, 99 * Q64, 99 * Q64, 10u128.pow(12)).unwrap(),
            1234
        );
    }

    #[test]
    fn claimable_fees_add_liquidity_times_growth_delta() {
        // 3.5 units of growth (Q64.64) over liquidity 1_000 accrue 3_500, floored.
        let last = 2 * Q64;
        let now = last + 3 * Q64 + Q64 / 2;
        assert_eq!(claimable_fees(10, last, now, 1_000).unwrap(), 10 + 3_500);
        assert_eq!(claimable_fees(0, 0, 1, Q64 - 1).unwrap(), 0, "floors");
    }

    #[test]
    fn claimable_fees_wrap_the_growth_delta_past_u128() {
        // The checkpoint sits just below 2^128 and the growth has since wrapped past zero.
        let last = u128::MAX - Q64 + 1; // -1.0 in Q64.64
        let now = 2 * Q64; // +2.0
        assert_eq!(claimable_fees(0, last, now, 100).unwrap(), 300);
    }

    #[test]
    fn claimable_fees_fail_closed_on_an_accrual_no_token_supply_can_hold() {
        // The program credits an accrual of u64::MAX or more as nothing (`to_underflow_u64`), but
        // no SPL amount reaches it: it only comes from growth values read out of step, so the
        // read fails instead of reporting the owed fields alone.
        let err = claimable_fees(5, 0, Q64, u64::MAX as u128).unwrap_err();
        assert!(matches!(err, Error::Integrity { .. }));
        assert_eq!(
            claimable_fees(0, 0, Q64, u64::MAX as u128 - 1).unwrap(),
            u64::MAX - 1
        );
    }

    #[test]
    fn claimable_fees_fail_closed_when_the_program_would_refuse() {
        // liquidity * delta / 2^64 above u128::MAX: the program errors with CalculateOverflow.
        let err = claimable_fees(0, 0, u128::MAX, u128::MAX).unwrap_err();
        assert!(matches!(err, Error::Integrity { .. }));
        // owed + accrued above u64::MAX: same.
        let err = claimable_fees(u64::MAX, 0, Q64, 1).unwrap_err();
        assert!(matches!(err, Error::Integrity { .. }));
    }

    #[test]
    fn uninitialized_or_misplaced_tick_fails_closed() {
        let mut array = vec![0u8; TICK_ARRAY_MIN_LEN];
        let base = TICK_ARRAY_TICKS + 3 * TICK_STATE_LEN;
        array[base..base + 4].copy_from_slice(&(-57i32).to_le_bytes());
        array[base + 36..base + 52].copy_from_slice(&42u128.to_le_bytes());
        // liquidity_gross still 0
        assert!(tick_fee_growth(&array, -60, -57, 1).is_err());
        array[base + 20..base + 36].copy_from_slice(&1u128.to_le_bytes());
        assert_eq!(tick_fee_growth(&array, -60, -57, 1).unwrap(), [42, 0]);
        // a different tick stored at that slot
        assert!(tick_fee_growth(&array, -60, -56, 1).is_err());
    }
}
