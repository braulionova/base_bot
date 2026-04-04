use alloy::primitives::{Address, U256};
use alloy::sol;
use dashmap::DashMap;
use eyre::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

use crate::pools::{Pool, PoolTypeSerializable};
use crate::rpc::MultiRpcProvider;

sol! {
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }

    #[sol(rpc)]
    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
    }
}

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
    /// Simple: WETH -> Token -> WETH across two DEXes
    Direct {
        pool_buy: Address,
        pool_sell: Address,
    },
    /// Triangular: WETH -> A -> B -> WETH
    Triangular {
        pool1: Address,
        pool2: Address,
        pool3: Address,
        token_a: Address,
        token_b: Address,
    },
}

pub struct ArbitrageEngine {
    rpc: Arc<MultiRpcProvider>,
    weth: Address,
}

impl ArbitrageEngine {
    pub fn new(rpc: Arc<MultiRpcProvider>) -> Self {
        let weth: Address = "0x4200000000000000000000000000000000000006".parse().unwrap();
        Self { rpc, weth }
    }

    /// Find cross-DEX arbitrage opportunities on priority pools
    pub async fn find_opportunities(
        &self,
        pools: &DashMap<Address, Pool>,
        priority_addrs: &[Address],
    ) -> Result<Vec<ArbOpportunity>> {
        let mut opportunities = Vec::new();

        // Build a map of token pairs -> pools
        let mut pair_map: HashMap<(Address, Address), Vec<Pool>> = HashMap::new();
        // Also build token -> pools map for triangular arb
        let mut token_pools: HashMap<Address, Vec<Pool>> = HashMap::new();

        for addr in priority_addrs {
            if let Some(pool) = pools.get(addr) {
                let key = if pool.token0 < pool.token1 {
                    (pool.token0, pool.token1)
                } else {
                    (pool.token1, pool.token0)
                };
                pair_map.entry(key).or_default().push(pool.clone());
                token_pools.entry(pool.token0).or_default().push(pool.clone());
                token_pools.entry(pool.token1).or_default().push(pool.clone());
            }
        }

        // 1. Direct cross-DEX arb: same pair on different DEXes
        for ((_t0, _t1), pool_list) in &pair_map {
            if pool_list.len() < 2 {
                continue;
            }

            for i in 0..pool_list.len() {
                for j in (i + 1)..pool_list.len() {
                    let pool_a = &pool_list[i];
                    let pool_b = &pool_list[j];

                    if pool_a.dex_name == pool_b.dex_name {
                        continue;
                    }

                    if let Some(opp) = self.check_pair_arb(pool_a, pool_b).await {
                        if opp.profit_eth > 0.00002 { // ~$0.05 minimum
                            opportunities.push(opp);
                        }
                    }
                }
            }
        }

        // 2. Triangular arb: WETH -> A -> B -> WETH
        if let Some(weth_pools) = token_pools.get(&self.weth) {
            // Find pools that share a non-WETH token (potential triangle)
            for pool1 in weth_pools.iter().take(200) {
                let token_a = if pool1.token0 == self.weth { pool1.token1 } else { pool1.token0 };

                if let Some(a_pools) = token_pools.get(&token_a) {
                    for pool2 in a_pools.iter().take(50) {
                        let token_b = if pool2.token0 == token_a { pool2.token1 } else { pool2.token0 };
                        if token_b == self.weth || token_b == token_a {
                            continue;
                        }

                        // Find a pool B -> WETH
                        let key_bw = if token_b < self.weth {
                            (token_b, self.weth)
                        } else {
                            (self.weth, token_b)
                        };

                        if let Some(bw_pools) = pair_map.get(&key_bw) {
                            for pool3 in bw_pools.iter().take(10) {
                                if let Some(opp) = self.check_triangle(pool1, pool2, pool3, token_a, token_b).await {
                                    if opp.profit_eth > 0.00002 { // ~$0.05 minimum
                                        opportunities.push(opp);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        opportunities.sort_by(|a, b| b.profit_wei.cmp(&a.profit_wei));

        if !opportunities.is_empty() {
            info!("Found {} arbitrage opportunities", opportunities.len());
            for opp in &opportunities[..opportunities.len().min(5)] {
                info!(
                    "  {} -> {}: {:.6} ETH profit ({} <-> {})",
                    opp.dex_a, opp.dex_b, opp.profit_eth, opp.pool_a, opp.pool_b
                );
            }
        }

        Ok(opportunities)
    }

    async fn check_pair_arb(&self, pool_a: &Pool, pool_b: &Pool) -> Option<ArbOpportunity> {
        let provider = self.rpc.get();

        let (token_in, token_bridge) = if pool_a.token0 == self.weth {
            (pool_a.token0, pool_a.token1)
        } else if pool_a.token1 == self.weth {
            (pool_a.token1, pool_a.token0)
        } else {
            return None;
        };

        // Trade sizes optimized for $0.05+ profit on longtail pools
        // Smaller amounts = less slippage on thin pools = better profit ratio
        let trade_sizes = [
            U256::from(50_000_000_000_000_000u128),  // 0.05 ETH
            U256::from(20_000_000_000_000_000u128),  // 0.02 ETH
            U256::from(10_000_000_000_000_000u128),  // 0.01 ETH
            U256::from(5_000_000_000_000_000u128),   // 0.005 ETH
            U256::from(2_000_000_000_000_000u128),   // 0.002 ETH
            U256::from(1_000_000_000_000_000u128),   // 0.001 ETH
        ];

        for amount_in in trade_sizes {
            let out_a = match self.get_output(provider, pool_a, token_in, token_bridge, amount_in).await {
                Some(v) if v > U256::ZERO => v,
                _ => continue,
            };

            let out_b = match self.get_output(provider, pool_b, token_bridge, token_in, out_a).await {
                Some(v) if v > U256::ZERO => v,
                _ => continue,
            };

            if out_b > amount_in {
                let profit_wei = out_b - amount_in;
                let profit_eth = wei_to_eth(profit_wei);

                return Some(ArbOpportunity {
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
                });
            }
        }

        None
    }

    async fn check_triangle(
        &self,
        pool1: &Pool,  // WETH -> A
        pool2: &Pool,  // A -> B
        pool3: &Pool,  // B -> WETH
        token_a: Address,
        token_b: Address,
    ) -> Option<ArbOpportunity> {
        let provider = self.rpc.get();
        let amount_in = U256::from(10_000_000_000_000_000u128); // 0.01 ETH

        // Step 1: WETH -> A
        let out1 = self.get_output(provider, pool1, self.weth, token_a, amount_in).await?;
        if out1.is_zero() { return None; }

        // Step 2: A -> B
        let out2 = self.get_output(provider, pool2, token_a, token_b, out1).await?;
        if out2.is_zero() { return None; }

        // Step 3: B -> WETH
        let out3 = self.get_output(provider, pool3, token_b, self.weth, out2).await?;
        if out3.is_zero() { return None; }

        if out3 > amount_in {
            let profit_wei = out3 - amount_in;
            let profit_eth = wei_to_eth(profit_wei);

            return Some(ArbOpportunity {
                pool_a: pool1.address,
                pool_b: pool3.address,
                dex_a: pool1.dex_name.clone(),
                dex_b: pool3.dex_name.clone(),
                token_in: self.weth,
                token_bridge: token_a,
                amount_in,
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
            });
        }

        None
    }

    async fn get_output(
        &self,
        provider: &crate::rpc::BoxProvider,
        pool: &Pool,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Option<U256> {
        match pool.pool_type {
            PoolTypeSerializable::V2 => {
                self.get_v2_output(provider, pool.address, token_in, token_out, amount_in).await
            }
            PoolTypeSerializable::V3 => {
                self.get_v3_output(provider, pool.address, token_in, token_out, amount_in, pool.fee).await
            }
        }
    }

    async fn get_v2_output(
        &self,
        provider: &crate::rpc::BoxProvider,
        pair: Address,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Option<U256> {
        let contract = IUniswapV2Pair::new(pair, provider);
        let reserves = match contract.getReserves().call().await {
            Ok(r) => r,
            Err(_) => return None,
        };

        let (reserve_in, reserve_out) = if token_in < token_out {
            (U256::from(reserves.reserve0), U256::from(reserves.reserve1))
        } else {
            (U256::from(reserves.reserve1), U256::from(reserves.reserve0))
        };

        if reserve_in.is_zero() || reserve_out.is_zero() {
            return None;
        }

        // UniswapV2: amountOut = (amountIn * 997 * reserveOut) / (reserveIn * 1000 + amountIn * 997)
        let amount_in_with_fee = amount_in * U256::from(997);
        let numerator = amount_in_with_fee * reserve_out;
        let denominator = reserve_in * U256::from(1000) + amount_in_with_fee;

        if denominator.is_zero() {
            return None;
        }

        Some(numerator / denominator)
    }

    async fn get_v3_output(
        &self,
        provider: &crate::rpc::BoxProvider,
        pool_addr: Address,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        fee: u32,
    ) -> Option<U256> {
        let pool = IUniswapV3Pool::new(pool_addr, provider);

        let slot0 = match pool.slot0().call().await {
            Ok(s) => s,
            Err(_) => return None,
        };
        let liquidity = match pool.liquidity().call().await {
            Ok(l) => l,
            Err(_) => return None,
        };

        let sqrt_price = slot0.sqrtPriceX96;
        let liq = liquidity;

        if liq == 0 || sqrt_price.is_zero() {
            return None;
        }

        let zero_for_one = token_in < token_out;
        let price_x96 = U256::from(sqrt_price);
        let q96 = U256::from(1u128 << 96);

        let amount_out = if zero_for_one {
            let price_num = price_x96 * price_x96;
            let price_denom = q96 * q96;
            if price_denom.is_zero() { return None; }
            let raw_out = amount_in * price_num / price_denom;
            raw_out * U256::from(1_000_000 - fee) / U256::from(1_000_000)
        } else {
            let price_num = q96 * q96;
            let price_denom = price_x96 * price_x96;
            if price_denom.is_zero() { return None; }
            let raw_out = amount_in * price_num / price_denom;
            raw_out * U256::from(1_000_000 - fee) / U256::from(1_000_000)
        };

        Some(amount_out)
    }
}

pub fn wei_to_eth(wei: U256) -> f64 {
    let s = format!("{}", wei);
    let val: f64 = s.parse().unwrap_or(0.0);
    val / 1e18
}
