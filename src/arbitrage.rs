use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use eyre::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;

use crate::multicall;
use crate::pools::{Pool, PoolTypeSerializable};
use crate::rpc::MultiRpcProvider;

#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    pub pool_a: Address,
    pub pool_b: Address,
    pub dex_a: String,
    pub dex_b: String,
    pub token_in: Address,
    pub token_bridge: Address,
    pub amount_in: U256,
    pub expected_out: U256,
    pub profit_wei: U256,
    pub profit_eth: f64,
    pub path: ArbPath,
}

#[derive(Debug, Clone)]
pub enum ArbPath {
    Direct {
        pool_buy: Address,
        pool_sell: Address,
    },
    Triangular {
        pool1: Address,
        pool2: Address,
        pool3: Address,
        token_a: Address,
        token_b: Address,
    },
}

/// Cached reserve state for a pool — avoids RPC per cycle
#[derive(Debug, Clone)]
pub enum PoolState {
    V2 { reserve0: U256, reserve1: U256 },
    V3 { sqrt_price_x96: U256, liquidity: u128 },
    Unknown,
}

/// Pre-computed pair index: built once, updated incrementally when new pools arrive
pub struct PairIndex {
    /// (token0, token1) sorted -> list of pool addresses for cross-DEX arb
    pub pair_to_pools: HashMap<(Address, Address), Vec<Address>>,
    /// token -> list of pool addresses (for triangular search)
    pub token_to_pools: HashMap<Address, Vec<Address>>,
    /// Cross-DEX arbable pairs: each entry is a vec of pool addrs with 2+ DEXes
    pub arbable_pairs: Vec<Vec<Address>>,
    /// WETH pools for triangular arb starting point
    pub weth_pools: Vec<Address>,
    /// Snapshot of how many pools were indexed
    pub indexed_count: usize,
}

impl PairIndex {
    pub fn build(pools: &DashMap<Address, Pool>, safe_addrs: &[Address], weth: Address) -> Self {
        let mut pair_to_pools: HashMap<(Address, Address), Vec<Address>> = HashMap::new();
        let mut token_to_pools: HashMap<Address, Vec<Address>> = HashMap::new();

        for addr in safe_addrs {
            if let Some(pool) = pools.get(addr) {
                let key = sorted_pair(pool.token0, pool.token1);
                pair_to_pools.entry(key).or_default().push(*addr);
                token_to_pools.entry(pool.token0).or_default().push(*addr);
                token_to_pools.entry(pool.token1).or_default().push(*addr);
            }
        }

        // Pre-compute arbable pairs: any pair with 2+ pools (cross-DEX or same-DEX different fee tiers)
        let mut arbable_pairs = Vec::new();
        for (_pair, addrs) in &pair_to_pools {
            if addrs.len() >= 2 {
                arbable_pairs.push(addrs.clone());
            }
        }

        let weth_pools = token_to_pools.get(&weth).cloned().unwrap_or_default();

        PairIndex {
            pair_to_pools,
            token_to_pools,
            arbable_pairs,
            weth_pools,
            indexed_count: safe_addrs.len(),
        }
    }
}

pub struct ArbitrageEngine {
    rpc: Arc<MultiRpcProvider>,
    weth: Address,
    /// Cached on-chain state per pool address
    pub reserve_cache: DashMap<Address, PoolState>,
}

// Coarse trade sizes for initial scan (find profitable direction fast)
const SCAN_SIZES: [u128; 6] = [
    500_000_000_000_000_000, // 0.5 ETH
    200_000_000_000_000_000, // 0.2 ETH
    50_000_000_000_000_000,  // 0.05 ETH
    10_000_000_000_000_000,  // 0.01 ETH
    3_000_000_000_000_000,   // 0.003 ETH
    1_000_000_000_000_000,   // 0.001 ETH
];

/// Minimum liquidity in reserve to consider a pool (avoid dust pools)
const MIN_RESERVE_WEI: u128 = 10_000_000_000_000_000; // 0.01 ETH — filter dust/honeypot pools

use alloy::primitives::address;

/// Anchor tokens for non-WETH arb (stablecoins + LSTs that are safe to trade)
fn stable_anchors() -> &'static [Address] {
    static ANCHORS: std::sync::OnceLock<Vec<Address>> = std::sync::OnceLock::new();
    ANCHORS.get_or_init(|| vec![
        address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"), // USDC
        address!("d9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA"), // USDbC
        address!("50c5725949A6F0c72E6C4a641F24049A917DB0Cb"), // DAI
        address!("2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22"), // cbETH
        address!("c1CBa3fCea344f92D9239c08C0568f6F2F0ee452"), // wstETH
        address!("04C0599Ae5A44757c0af6F9eC3b93da8976c150A"), // weETH
        address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf"), // cbBTC
    ])
}

impl ArbitrageEngine {
    pub fn new(rpc: Arc<MultiRpcProvider>) -> Self {
        let weth: Address = "0x4200000000000000000000000000000000000006".parse().unwrap();
        Self {
            rpc,
            weth,
            reserve_cache: DashMap::new(),
        }
    }

    /// Check if a token is a known stable/anchor token safe for non-WETH arb
    fn is_stable_anchor(&self, token: Address) -> bool {
        stable_anchors().contains(&token)
    }

    /// Refresh reserves via parallel multicall across all RPC providers.
    pub async fn refresh_reserves(&self, pools: &DashMap<Address, Pool>, addrs: &[Address]) {
        let mut v2_addrs = Vec::new();
        let mut v3_addrs = Vec::new();

        for addr in addrs {
            if let Some(pool) = pools.get(addr) {
                match pool.pool_type {
                    PoolTypeSerializable::V2 => v2_addrs.push(*addr),
                    PoolTypeSerializable::V3 => v3_addrs.push(*addr),
                }
            }
        }

        // Fetch V2 and V3 reserves concurrently (chunks parallelized inside)
        let (v2_results, v3_results) = tokio::join!(
            multicall::batch_v2_reserves(&self.rpc, &v2_addrs),
            multicall::batch_v3_state(&self.rpc, &v3_addrs),
        );

        for (addr, res) in v2_addrs.iter().zip(v2_results.iter()) {
            if let Some(r) = res {
                self.reserve_cache.insert(*addr, PoolState::V2 {
                    reserve0: r.reserve0,
                    reserve1: r.reserve1,
                });
            }
        }

        for (addr, res) in v3_addrs.iter().zip(v3_results.iter()) {
            if let Some(s) = res {
                self.reserve_cache.insert(*addr, PoolState::V3 {
                    sqrt_price_x96: s.sqrt_price_x96,
                    liquidity: s.liquidity,
                });
            }
        }
    }

    /// Priority refresh: only refresh stale pools (those with recent swaps).
    /// Falls back to full refresh every N cycles.
    pub async fn refresh_stale_only(
        &self,
        pools: &DashMap<Address, Pool>,
        all_addrs: &[Address],
        stale: &dashmap::DashMap<Address, ()>,
    ) {
        // Drain stale set
        let stale_addrs: Vec<Address> = stale.iter().map(|e| *e.key()).collect();
        stale.clear();

        if stale_addrs.is_empty() {
            // No swaps detected — skip refresh entirely (use cached data)
            return;
        }

        // Only refresh pools that had swaps + their pair partners
        let mut to_refresh: Vec<Address> = Vec::with_capacity(stale_addrs.len() * 3);
        let mut seen = std::collections::HashSet::new();

        for addr in &stale_addrs {
            if seen.insert(*addr) {
                to_refresh.push(*addr);
            }
        }

        // Also refresh related pools (same token pair on other DEXes)
        for addr in &stale_addrs {
            if let Some(pool) = pools.get(addr) {
                let key = sorted_pair(pool.token0, pool.token1);
                for a in all_addrs {
                    if seen.contains(a) { continue; }
                    if let Some(p) = pools.get(a) {
                        if sorted_pair(p.token0, p.token1) == key {
                            seen.insert(*a);
                            to_refresh.push(*a);
                        }
                    }
                }
            }
        }

        self.refresh_reserves(pools, &to_refresh).await;
    }

    /// Find opportunities using pre-computed index and cached reserves (zero RPC in hot path)
    pub fn find_opportunities_cached(
        &self,
        pools: &DashMap<Address, Pool>,
        index: &PairIndex,
    ) -> Vec<ArbOpportunity> {
        let mut opportunities = Vec::new();

        // 1. Direct cross-DEX arb from pre-computed pairs
        for pool_addrs in &index.arbable_pairs {
            for i in 0..pool_addrs.len() {
                for j in (i + 1)..pool_addrs.len() {
                    let addr_a = pool_addrs[i];
                    let addr_b = pool_addrs[j];

                    let (pool_a, pool_b) = match (pools.get(&addr_a), pools.get(&addr_b)) {
                        (Some(a), Some(b)) => (a, b),
                        _ => continue,
                    };

                    // Allow same-DEX pairs (different fee tiers, e.g. V3 0.05% vs 0.3%)
                    if pool_a.dex_name == pool_b.dex_name && pool_a.address == pool_b.address {
                        continue;
                    }

                    // Only arb pairs where at least one token is WETH or a known anchor
                    if pool_a.token0 != self.weth && pool_a.token1 != self.weth
                        && !self.is_stable_anchor(pool_a.token0) && !self.is_stable_anchor(pool_a.token1)
                    {
                        continue;
                    }

                    if let Some(opp) = self.check_pair_arb_cached(&pool_a, &pool_b) {
                        if opp.profit_eth > 0.000005 {
                            opportunities.push(opp);
                        }
                    }
                }
            }
        }

        // 2. Triangular arb: WETH -> A -> B -> WETH
        // Only scan pools that have cached reserves (skip uninitialized)
        for &pool1_addr in index.weth_pools.iter().take(5000) {
            if !self.reserve_cache.contains_key(&pool1_addr) { continue; }
            let pool1 = match pools.get(&pool1_addr) {
                Some(p) => p,
                None => continue,
            };
            let token_a = if pool1.token0 == self.weth { pool1.token1 } else { pool1.token0 };

            if let Some(a_pool_addrs) = index.token_to_pools.get(&token_a) {
                for &pool2_addr in a_pool_addrs.iter().take(200) {
                    let pool2 = match pools.get(&pool2_addr) {
                        Some(p) => p,
                        None => continue,
                    };
                    let token_b = if pool2.token0 == token_a { pool2.token1 } else { pool2.token0 };
                    if token_b == self.weth || token_b == token_a { continue; }

                    let key_bw = sorted_pair(token_b, self.weth);
                    if let Some(bw_addrs) = index.pair_to_pools.get(&key_bw) {
                        for &pool3_addr in bw_addrs.iter().take(10) {
                            let pool3 = match pools.get(&pool3_addr) {
                                Some(p) => p,
                                None => continue,
                            };
                            if let Some(opp) = self.check_triangle_cached(
                                &pool1, &pool2, &pool3, token_a, token_b,
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

        opportunities.sort_unstable_by(|a, b| b.profit_wei.cmp(&a.profit_wei));

        // Dedup: keep only the best opportunity per pool pair
        {
            let mut seen = std::collections::HashSet::new();
            opportunities.retain(|opp| {
                let key = if opp.pool_a < opp.pool_b {
                    (opp.pool_a, opp.pool_b)
                } else {
                    (opp.pool_b, opp.pool_a)
                };
                seen.insert(key)
            });
        }

        if !opportunities.is_empty() {
            info!("Found {} arbitrage opportunities", opportunities.len());
            for opp in opportunities.iter().take(5) {
                info!(
                    "  {} -> {}: {:.6} ETH profit ({} <-> {})",
                    opp.dex_a, opp.dex_b, opp.profit_eth, opp.pool_a, opp.pool_b
                );
            }
        }

        opportunities
    }

    /// Check direct arb using cached reserves — NO RPC calls.
    /// Uses coarse scan + binary search to find optimal input amount.
    fn check_pair_arb_cached(&self, pool_a: &Pool, pool_b: &Pool) -> Option<ArbOpportunity> {
        // Determine token_in and token_bridge — prefer WETH as token_in
        let (token_in, token_bridge) = if pool_a.token0 == self.weth {
            (pool_a.token0, pool_a.token1)
        } else if pool_a.token1 == self.weth {
            (pool_a.token1, pool_a.token0)
        } else if self.is_stable_anchor(pool_a.token0)
            && (pool_a.token0 == pool_b.token0 || pool_a.token0 == pool_b.token1)
        {
            // Non-WETH pair — only allow if token_in is a known stable/anchor token
            (pool_a.token0, pool_a.token1)
        } else if self.is_stable_anchor(pool_a.token1)
            && (pool_a.token1 == pool_b.token0 || pool_a.token1 == pool_b.token1)
        {
            (pool_a.token1, pool_a.token0)
        } else {
            return None;
        };

        let state_a = self.reserve_cache.get(&pool_a.address)?;
        let state_b = self.reserve_cache.get(&pool_b.address)?;

        // Liquidity floor check — skip dust pools
        if !has_min_liquidity(&state_a) || !has_min_liquidity(&state_b) {
            return None;
        }

        // Phase 1: Coarse scan to find profitable range
        let mut best_profit = U256::ZERO;
        let mut best_size: u128 = 0;

        for &size in &SCAN_SIZES {
            let amount_in = U256::from(size);
            let out_a = get_output_cached(&state_a, pool_a, token_in, token_bridge, amount_in)?;
            if out_a.is_zero() { continue; }
            let out_b = get_output_cached(&state_b, pool_b, token_bridge, token_in, out_a)?;
            if out_b <= amount_in { continue; }

            let profit = out_b - amount_in;
            if profit > best_profit {
                best_profit = profit;
                best_size = size;
            }
        }

        if best_size == 0 {
            return None;
        }

        // Phase 2: Binary search around best_size to maximize profit (8 iterations)
        let mut lo = best_size / 3;
        let mut hi = (best_size * 3).min(1_000_000_000_000_000_000); // cap at 1.0 ETH
        for _ in 0..8 {
            if hi - lo < 500_000_000_000_000 { break; } // 0.0005 ETH precision
            let mid1 = lo + (hi - lo) / 3;
            let mid2 = hi - (hi - lo) / 3;

            let p1 = calc_direct_profit(&state_a, pool_a, &state_b, pool_b, token_in, token_bridge, U256::from(mid1));
            let p2 = calc_direct_profit(&state_a, pool_a, &state_b, pool_b, token_in, token_bridge, U256::from(mid2));

            if p1 < p2 {
                lo = mid1;
            } else {
                hi = mid2;
            }
        }

        let optimal_in = U256::from((lo + hi) / 2);
        let out_a = get_output_cached(&state_a, pool_a, token_in, token_bridge, optimal_in)?;
        let out_b = get_output_cached(&state_b, pool_b, token_bridge, token_in, out_a)?;
        if out_b <= optimal_in { return None; }

        let profit_wei = out_b - optimal_in;
        let profit_eth = wei_to_eth(profit_wei);

        // Sanity check: profit > 10% of input is likely a honeypot/stale data
        if profit_wei > optimal_in / U256::from(10) {
            return None;
        }

        Some(ArbOpportunity {
            pool_a: pool_a.address,
            pool_b: pool_b.address,
            dex_a: pool_a.dex_name.clone(),
            dex_b: pool_b.dex_name.clone(),
            token_in,
            token_bridge,
            amount_in: optimal_in,
            expected_out: out_b,
            profit_wei,
            profit_eth,
            path: ArbPath::Direct {
                pool_buy: pool_a.address,
                pool_sell: pool_b.address,
            },
        })
    }

    /// Check triangular arb using cached reserves + ternary search optimization.
    fn check_triangle_cached(
        &self,
        pool1: &Pool,
        pool2: &Pool,
        pool3: &Pool,
        token_a: Address,
        token_b: Address,
    ) -> Option<ArbOpportunity> {
        let state1 = self.reserve_cache.get(&pool1.address)?;
        let state2 = self.reserve_cache.get(&pool2.address)?;
        let state3 = self.reserve_cache.get(&pool3.address)?;

        // Liquidity floor
        if !has_min_liquidity(&state1) || !has_min_liquidity(&state2) || !has_min_liquidity(&state3) {
            return None;
        }

        // Coarse scan for triangular (smaller sizes due to 3 legs)
        const TRI_SCAN: [u128; 5] = [
            300_000_000_000_000_000, // 0.3 ETH
            100_000_000_000_000_000, // 0.1 ETH
            30_000_000_000_000_000,  // 0.03 ETH
            5_000_000_000_000_000,   // 0.005 ETH
            1_500_000_000_000_000,   // 0.0015 ETH
        ];

        let weth = self.weth;
        let mut best_profit = U256::ZERO;
        let mut best_size: u128 = 0;

        for &size in &TRI_SCAN {
            let amount_in = U256::from(size);
            let profit = calc_triangle_profit(
                &state1, pool1, &state2, pool2, &state3, pool3,
                weth, token_a, token_b, amount_in,
            );
            if profit > best_profit {
                best_profit = profit;
                best_size = size;
            }
        }

        if best_size == 0 { return None; }

        // Ternary search optimization (6 iterations for triangular)
        let mut lo = best_size / 3;
        let mut hi = (best_size * 3).min(500_000_000_000_000_000); // cap at 0.5 ETH
        for _ in 0..6 {
            if hi - lo < 500_000_000_000_000 { break; }
            let mid1 = lo + (hi - lo) / 3;
            let mid2 = hi - (hi - lo) / 3;

            let p1 = calc_triangle_profit(&state1, pool1, &state2, pool2, &state3, pool3, weth, token_a, token_b, U256::from(mid1));
            let p2 = calc_triangle_profit(&state1, pool1, &state2, pool2, &state3, pool3, weth, token_a, token_b, U256::from(mid2));

            if p1 < p2 { lo = mid1; } else { hi = mid2; }
        }

        let optimal_in = U256::from((lo + hi) / 2);
        let out1 = get_output_cached(&state1, pool1, weth, token_a, optimal_in)?;
        if out1.is_zero() { return None; }
        let out2 = get_output_cached(&state2, pool2, token_a, token_b, out1)?;
        if out2.is_zero() { return None; }
        let out3 = get_output_cached(&state3, pool3, token_b, weth, out2)?;
        if out3 <= optimal_in { return None; }

        let profit_wei = out3 - optimal_in;
        let profit_eth = wei_to_eth(profit_wei);

        // Sanity check: profit > 10% of input is likely a honeypot/stale data
        if profit_wei > optimal_in / U256::from(10) {
            return None;
        }

        Some(ArbOpportunity {
            pool_a: pool1.address,
            pool_b: pool3.address,
            dex_a: pool1.dex_name.clone(),
            dex_b: pool3.dex_name.clone(),
            token_in: weth,
            token_bridge: token_a,
            amount_in: optimal_in,
            expected_out: out3,
            profit_wei,
            profit_eth,
            path: ArbPath::Triangular {
                pool1: pool1.address,
                pool2: pool2.address,
                pool3: pool3.address,
                token_a,
                token_b,
            },
        })
    }

    // Legacy: RPC-based find_opportunities for backwards compat
    pub async fn find_opportunities(
        &self,
        pools: &DashMap<Address, Pool>,
        priority_addrs: &[Address],
    ) -> Result<Vec<ArbOpportunity>> {
        // Refresh reserves via multicall, then use cached path
        self.refresh_reserves(pools, priority_addrs).await;

        // Build a temporary index (in hot loop, caller should use pre-computed PairIndex)
        let index = PairIndex::build(pools, priority_addrs, self.weth);
        Ok(self.find_opportunities_cached(pools, &index))
    }
}

/// Compute output from cached state — pure math, no I/O
fn get_output_cached(
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
            // Use pool.fee (millionths) instead of hardcoded 0.3%
            let fee_factor = U256::from(1_000_000u32 - pool.fee);
            let amount_after_fee = amount_in * fee_factor;
            let num = amount_after_fee * res_out;
            let den = res_in * U256::from(1_000_000u32) + amount_after_fee;
            if den.is_zero() { return None; }
            Some(num / den)
        }
        PoolState::V3 { sqrt_price_x96, liquidity } => {
            if *liquidity == 0 || sqrt_price_x96.is_zero() { return None; }

            // Derive virtual reserves from sqrtPriceX96 and liquidity
            // x (token0) = L * Q96 / sqrtPrice
            // y (token1) = L * sqrtPrice / Q96
            // Then apply constant product with fee for accurate single-tick pricing
            let liq = U256::from(*liquidity);
            let q96 = U256::from(1u128 << 96);
            let sp = *sqrt_price_x96;

            let virtual_reserve0 = liq * q96 / sp;
            let virtual_reserve1 = liq * sp / q96;

            if virtual_reserve0.is_zero() || virtual_reserve1.is_zero() { return None; }

            let zero_for_one = token_in == pool.token0;
            let (res_in, res_out) = if zero_for_one {
                (virtual_reserve0, virtual_reserve1)
            } else {
                (virtual_reserve1, virtual_reserve0)
            };

            // Apply fee (pool.fee in millionths, e.g., 3000 = 0.3%)
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

/// Check if pool has minimum liquidity to avoid dust pools
#[inline]
fn has_min_liquidity(state: &PoolState) -> bool {
    match state {
        PoolState::V2 { reserve0, reserve1 } => {
            *reserve0 >= U256::from(MIN_RESERVE_WEI) || *reserve1 >= U256::from(MIN_RESERVE_WEI)
        }
        PoolState::V3 { liquidity, .. } => *liquidity > 1000,
        PoolState::Unknown => false,
    }
}

/// Calculate direct arb profit for a given input amount (pure math)
#[inline]
fn calc_direct_profit(
    state_a: &PoolState, pool_a: &Pool,
    state_b: &PoolState, pool_b: &Pool,
    token_in: Address, token_bridge: Address,
    amount_in: U256,
) -> U256 {
    let out_a = match get_output_cached(state_a, pool_a, token_in, token_bridge, amount_in) {
        Some(v) if !v.is_zero() => v,
        _ => return U256::ZERO,
    };
    let out_b = match get_output_cached(state_b, pool_b, token_bridge, token_in, out_a) {
        Some(v) if v > amount_in => v,
        _ => return U256::ZERO,
    };
    out_b - amount_in
}

/// Calculate triangular arb profit for a given input amount (pure math)
#[inline]
fn calc_triangle_profit(
    state1: &PoolState, pool1: &Pool,
    state2: &PoolState, pool2: &Pool,
    state3: &PoolState, pool3: &Pool,
    weth: Address, token_a: Address, token_b: Address,
    amount_in: U256,
) -> U256 {
    let out1 = match get_output_cached(state1, pool1, weth, token_a, amount_in) {
        Some(v) if !v.is_zero() => v,
        _ => return U256::ZERO,
    };
    let out2 = match get_output_cached(state2, pool2, token_a, token_b, out1) {
        Some(v) if !v.is_zero() => v,
        _ => return U256::ZERO,
    };
    let out3 = match get_output_cached(state3, pool3, token_b, weth, out2) {
        Some(v) if v > amount_in => v,
        _ => return U256::ZERO,
    };
    out3 - amount_in
}

#[inline]
fn sorted_pair(a: Address, b: Address) -> (Address, Address) {
    if a < b { (a, b) } else { (b, a) }
}

/// Convert wei to ETH without string allocation (hot path optimization)
#[inline]
pub fn wei_to_eth(wei: U256) -> f64 {
    let lo: u128 = wei.saturating_to::<u128>();
    lo as f64 / 1e18
}
