use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use std::collections::VecDeque;
use tracing::{info, debug};

use crate::arbitrage::{self, ArbOpportunity, ArbPath, ArbitrageEngine, PairIndex, PoolState, wei_to_eth};
use crate::pools::Pool;

/// Tracks large swaps and identifies backrun opportunities.
/// When a big swap moves the price on one DEX, adjacent DEXes may be stale
/// → cross-DEX arb opportunity that we can capture in the next block.
pub struct BackrunDetector {
    /// Recent large swaps (ring buffer, last 50)
    recent_swaps: VecDeque<LargeSwapEvent>,
    /// Stats
    pub total_detected: u64,
    pub total_profitable: u64,
    pub total_profit_wei: u128,
}

#[derive(Debug, Clone)]
pub struct LargeSwapEvent {
    pub pool: Address,
    pub block: u64,
    pub amount0: U256,
    pub amount1: U256,
    pub is_v3: bool,
    pub timestamp: std::time::Instant,
}

impl BackrunDetector {
    pub fn new() -> Self {
        Self {
            recent_swaps: VecDeque::with_capacity(50),
            total_detected: 0,
            total_profitable: 0,
            total_profit_wei: 0,
        }
    }

    /// Record a large swap event from the realtime feed
    pub fn record_swap(&mut self, pool: Address, block: u64, amount0: U256, amount1: U256, is_v3: bool) {
        self.total_detected += 1;
        if self.recent_swaps.len() >= 50 {
            self.recent_swaps.pop_front();
        }
        self.recent_swaps.push_back(LargeSwapEvent {
            pool, block, amount0, amount1, is_v3,
            timestamp: std::time::Instant::now(),
        });
        debug!(
            "Large swap: pool={} a0={:.4}ETH a1={:.4}ETH v3={}",
            pool,
            wei_to_eth(amount0),
            wei_to_eth(amount1),
            is_v3,
        );
    }

    /// After a large swap, find backrun arb opportunities.
    /// The swap moved the price on one pool — check if adjacent pools
    /// on other DEXes still have the old price → arb.
    pub fn find_backrun_opportunities(
        &mut self,
        swap_pool: Address,
        pools: &DashMap<Address, Pool>,
        arb_engine: &ArbitrageEngine,
        pair_index: &PairIndex,
    ) -> Vec<ArbOpportunity> {
        let mut opportunities = Vec::new();

        let swap_pool_info = match pools.get(&swap_pool) {
            Some(p) => p.clone(),
            None => return opportunities,
        };

        let key = sorted_pair(swap_pool_info.token0, swap_pool_info.token1);

        // Find all other pools for the same token pair
        if let Some(peer_addrs) = pair_index.pair_to_pools.get(&key) {
            // For each peer pool on a DIFFERENT dex, check arb vs the swapped pool
            for peer_addr in peer_addrs {
                if *peer_addr == swap_pool { continue; }

                let peer = match pools.get(peer_addr) {
                    Some(p) => p.clone(),
                    None => continue,
                };
                if peer.dex_name == swap_pool_info.dex_name { continue; }

                // Check both directions: swap_pool→peer and peer→swap_pool
                if let Some(opp) = check_backrun_pair(arb_engine, &swap_pool_info, &peer) {
                    if opp.profit_eth > 0.000005 {
                        opportunities.push(opp);
                    }
                }
                if let Some(opp) = check_backrun_pair(arb_engine, &peer, &swap_pool_info) {
                    if opp.profit_eth > 0.000005 {
                        opportunities.push(opp);
                    }
                }
            }
        }

        // Also check triangular opportunities through the swapped token pair
        let weth: Address = "0x4200000000000000000000000000000000000006".parse().unwrap();
        let other_token = if swap_pool_info.token0 == weth {
            swap_pool_info.token1
        } else if swap_pool_info.token1 == weth {
            swap_pool_info.token0
        } else {
            // Not a WETH pair — check if either token connects to WETH
            Address::ZERO
        };

        if other_token != Address::ZERO {
            // The swap moved WETH/token price → check triangular through connected tokens
            if let Some(token_pools) = pair_index.token_to_pools.get(&other_token) {
                for pool2_addr in token_pools.iter().take(30) {
                    let pool2 = match pools.get(pool2_addr) {
                        Some(p) => p.clone(),
                        None => continue,
                    };
                    let token_b = if pool2.token0 == other_token { pool2.token1 } else { pool2.token0 };
                    if token_b == weth || token_b == other_token { continue; }

                    let key_bw = sorted_pair(token_b, weth);
                    if let Some(bw_addrs) = pair_index.pair_to_pools.get(&key_bw) {
                        for pool3_addr in bw_addrs.iter().take(5) {
                            let pool3 = match pools.get(pool3_addr) {
                                Some(p) => p.clone(),
                                None => continue,
                            };

                            if let Some(opp) = check_backrun_triangle(
                                arb_engine, &swap_pool_info, &pool2, &pool3,
                                other_token, token_b, weth,
                            ) {
                                if opp.profit_eth > 0.000005 {
                                    opportunities.push(opp);
                                }
                            }
                        }
                    }
                }
            }
        }

        if !opportunities.is_empty() {
            opportunities.sort_unstable_by(|a, b| b.profit_wei.cmp(&a.profit_wei));
            self.total_profitable += opportunities.len() as u64;
            for opp in &opportunities {
                self.total_profit_wei += opp.profit_wei.to::<u128>();
            }
            info!(
                "BACKRUN: {} opps from swap on {} ({} {}), best: {:.6} ETH",
                opportunities.len(),
                swap_pool_info.dex_name,
                swap_pool,
                if swap_pool_info.token0 == weth { "WETH/X" } else { "X/Y" },
                opportunities[0].profit_eth,
            );
        }

        opportunities
    }

    pub fn stats_string(&self) -> String {
        format!(
            "backrun: {} detected, {} profitable, {:.6}ETH total",
            self.total_detected,
            self.total_profitable,
            self.total_profit_wei as f64 / 1e18,
        )
    }
}

/// Check direct backrun arb between two pools using cached reserves
fn check_backrun_pair(
    engine: &ArbitrageEngine,
    pool_a: &Pool,
    pool_b: &Pool,
) -> Option<ArbOpportunity> {
    let weth: Address = "0x4200000000000000000000000000000000000006".parse().unwrap();

    let (token_in, token_bridge) = if pool_a.token0 == weth {
        (pool_a.token0, pool_a.token1)
    } else if pool_a.token1 == weth {
        (pool_a.token1, pool_a.token0)
    } else {
        return None;
    };

    let state_a = engine.reserve_cache.get(&pool_a.address)?;
    let state_b = engine.reserve_cache.get(&pool_b.address)?;

    // Scan a few sizes
    const SIZES: [u128; 5] = [
        50_000_000_000_000_000,  // 0.05 ETH
        20_000_000_000_000_000,  // 0.02 ETH
        10_000_000_000_000_000,  // 0.01 ETH
        5_000_000_000_000_000,   // 0.005 ETH
        2_000_000_000_000_000,   // 0.002 ETH
    ];

    let mut best: Option<ArbOpportunity> = None;

    for &size in &SIZES {
        let amount_in = U256::from(size);
        let out_a = get_output(&state_a, pool_a, token_in, token_bridge, amount_in)?;
        if out_a.is_zero() { continue; }
        let out_b = get_output(&state_b, pool_b, token_bridge, token_in, out_a)?;
        if out_b <= amount_in { continue; }

        let profit_wei = out_b - amount_in;
        let profit_eth = wei_to_eth(profit_wei);

        let opp = ArbOpportunity {
            pool_a: pool_a.address,
            pool_b: pool_b.address,
            dex_a: pool_a.dex_name.clone(),
            dex_b: pool_b.dex_name.clone(),
            token_in,
            token_bridge,
            amount_in,
            expected_out: out_b,
            profit_wei,
            profit_eth,
            path: ArbPath::Direct {
                pool_buy: pool_a.address,
                pool_sell: pool_b.address,
            },
        };

        if best.as_ref().map_or(true, |b| profit_wei > b.profit_wei) {
            best = Some(opp);
        }
    }

    best
}

/// Check triangular backrun opportunity
fn check_backrun_triangle(
    engine: &ArbitrageEngine,
    pool1: &Pool,
    pool2: &Pool,
    pool3: &Pool,
    token_a: Address,
    token_b: Address,
    weth: Address,
) -> Option<ArbOpportunity> {
    let state1 = engine.reserve_cache.get(&pool1.address)?;
    let state2 = engine.reserve_cache.get(&pool2.address)?;
    let state3 = engine.reserve_cache.get(&pool3.address)?;

    const SIZES: [u128; 3] = [
        15_000_000_000_000_000,
        5_000_000_000_000_000,
        2_000_000_000_000_000,
    ];

    for &size in &SIZES {
        let amount_in = U256::from(size);
        let out1 = get_output(&state1, pool1, weth, token_a, amount_in)?;
        if out1.is_zero() { continue; }
        let out2 = get_output(&state2, pool2, token_a, token_b, out1)?;
        if out2.is_zero() { continue; }
        let out3 = get_output(&state3, pool3, token_b, weth, out2)?;
        if out3 <= amount_in { continue; }

        let profit_wei = out3 - amount_in;
        return Some(ArbOpportunity {
            pool_a: pool1.address,
            pool_b: pool3.address,
            dex_a: pool1.dex_name.clone(),
            dex_b: pool3.dex_name.clone(),
            token_in: weth,
            token_bridge: token_a,
            amount_in,
            expected_out: out3,
            profit_wei,
            profit_eth: wei_to_eth(profit_wei),
            path: ArbPath::Triangular {
                pool1: pool1.address,
                pool2: pool2.address,
                pool3: pool3.address,
                token_a,
                token_b,
            },
        });
    }

    None
}

/// Reuse the same output calculation from arbitrage module
fn get_output(
    state: &PoolState,
    pool: &Pool,
    token_in: Address,
    _token_out: Address,
    amount_in: U256,
) -> Option<U256> {
    match state {
        PoolState::V2 { reserve0, reserve1 } => {
            let (res_in, res_out) = if token_in == pool.token0 {
                (*reserve0, *reserve1)
            } else {
                (*reserve1, *reserve0)
            };
            if res_in.is_zero() || res_out.is_zero() { return None; }
            let fee_factor = U256::from(1_000_000u32 - pool.fee);
            let amount_after_fee = amount_in * fee_factor;
            let num = amount_after_fee * res_out;
            let den = res_in * U256::from(1_000_000u32) + amount_after_fee;
            if den.is_zero() { return None; }
            Some(num / den)
        }
        PoolState::V3 { sqrt_price_x96, liquidity } => {
            if *liquidity == 0 || sqrt_price_x96.is_zero() { return None; }
            let liq = U256::from(*liquidity);
            let q96 = U256::from(1u128 << 96);
            let sp = *sqrt_price_x96;
            let vr0 = liq * q96 / sp;
            let vr1 = liq * sp / q96;
            if vr0.is_zero() || vr1.is_zero() { return None; }
            let zero_for_one = token_in == pool.token0;
            let (res_in, res_out) = if zero_for_one { (vr0, vr1) } else { (vr1, vr0) };
            let fee_factor = U256::from(1_000_000u32 - pool.fee);
            let amount_after_fee = amount_in * fee_factor;
            let num = amount_after_fee * res_out;
            let den = res_in * U256::from(1_000_000u32) + amount_after_fee;
            if den.is_zero() { return None; }
            Some(num / den)
        }
        PoolState::Unknown => None,
    }
}

#[inline]
fn sorted_pair(a: Address, b: Address) -> (Address, Address) {
    if a < b { (a, b) } else { (b, a) }
}
