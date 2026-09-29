//! # gluonscan-raydium
//!
//! Raydium concentrated-liquidity (CLMM) adapter, [`Source::OnChain`]: discovers position NFTs
//! from the wallet's token accounts, derives each position PDA, and decodes the on-chain position,
//! pool and mint accounts into a [`LiquidityPosition`] (principal amounts via the Q64.96 math,
//! uncollected fees from the position's owed fields, in-range from the pool tick).
//!
//! The account byte layouts here are a **slice layout** kept self-consistent with the tests; exact
//! on-chain layout fidelity (and Token-2022 discovery) is a reconciliation follow-up.

use alloy_primitives::U256;
use async_trait::async_trait;
use gluonscan_core::{
    scaled, Amount, Capability, Chain, Complete, Ctx, Detail, Error, LiquidityPosition, Position,
    Protocol, ProtocolAdapter, Provenance, Reading, Source, Staleness, Token, Wallet,
};
use gluonscan_math::{
    get_amounts_for_liquidity, get_sqrt_ratio_at_tick, is_in_range, MAX_TICK, MIN_TICK,
};
use gluonscan_solana::{
    find_program_address, get_account_info, get_token_accounts_by_owner, pubkey_bytes, pubkey_str,
    RAYDIUM_CLMM_PROGRAM,
};

const CAPABILITIES: &[Capability] = &[Capability::Positions, Capability::Fees];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Solana];

// Position account slice layout (byte offsets).
const POS_MIN_LEN: usize = 113;
const POS_POOL: usize = 41;
const POS_TICK_LOWER: usize = 73;
const POS_TICK_UPPER: usize = 77;
const POS_LIQUIDITY: usize = 81;
const POS_FEE0: usize = 97;
const POS_FEE1: usize = 105;

// Pool account slice layout.
const POOL_MIN_LEN: usize = 92;
const POOL_TICK: usize = 8;
const POOL_SQRT_X64: usize = 12;
const POOL_MINT0: usize = 28;
const POOL_MINT1: usize = 60;

// SPL mint: decimals byte.
const MINT_DECIMALS: usize = 44;

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
            positions.push(parse_position(rpc, &data).await?);
        }

        let reading = Reading {
            protocol: Protocol::Raydium,
            chain: Chain::Solana,
            source: Source::OnChain,
            positions,
            provenance: Provenance {
                source: Source::OnChain,
                chain: Chain::Solana,
                block: None,
                at: cx.clock.now(),
                staleness: Staleness::Live,
            },
        };
        Ok(Complete::new(reading))
    }
}

async fn parse_position(
    rpc: &dyn gluonscan_core::ChainProvider,
    data: &[u8],
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
    let mint0 = pubkey_at(&pool, POOL_MINT0)?;
    let mint1 = pubkey_at(&pool, POOL_MINT1)?;

    let dec0 = mint_decimals(rpc, &mint0).await?;
    let dec1 = mint_decimals(rpc, &mint1).await?;
    let token0 = Token {
        symbol: String::new(),
        address: None,
        decimals: dec0,
    };
    let token1 = Token {
        symbol: String::new(),
        address: None,
        decimals: dec1,
    };

    // Principal amounts via the shared Q64.96 math (Raydium's sqrt price is Q64.64 → shift up 32).
    let sqrt_price = U256::from(sqrt_x64) << 32;
    let (amt0, amt1) = get_amounts_for_liquidity(
        sqrt_price,
        get_sqrt_ratio_at_tick(tick_lower),
        get_sqrt_ratio_at_tick(tick_upper),
        U256::from(liquidity),
    );

    Ok(Position::Liquidity(LiquidityPosition {
        assets: vec![
            raw_amount(token0.clone(), amt0)?,
            raw_amount(token1.clone(), amt1)?,
        ],
        uncollected_fees: vec![
            raw_amount(token0.clone(), U256::from(fee0))?,
            raw_amount(token1.clone(), U256::from(fee1))?,
        ],
        deposited: Vec::new(),
        withdrawn: Vec::new(),
        collected_fees: Vec::new(),
        fee_tier_bps: None,
        tick_lower,
        tick_upper,
        tick_current,
        in_range: is_in_range(tick_current, tick_lower, tick_upper),
        token0,
        token1,
    }))
}

async fn mint_decimals(
    rpc: &dyn gluonscan_core::ChainProvider,
    mint: &[u8; 32],
) -> Result<u8, Error> {
    let data = get_account_info(rpc, &pubkey_str(mint))
        .await?
        .ok_or_else(|| Error::Integrity {
            message: "Raydium token mint account missing".into(),
        })?;
    data.get(MINT_DECIMALS)
        .copied()
        .ok_or_else(|| Error::Integrity {
            message: "mint account too short for decimals".into(),
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
