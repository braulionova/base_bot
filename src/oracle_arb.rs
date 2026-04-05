use alloy::primitives::{Address, U256, address};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use std::sync::Arc;
use tracing::{info, warn, debug};

use crate::rpc::MultiRpcProvider;
use crate::arbitrage::ArbOpportunity;

sol! {
    #[allow(missing_docs)]
    function latestAnswer() external view returns (int256);
    function latestRoundData() external view returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
}

/// Oracle-guided arbitrage: uses Chainlink oracle rates as "true price" reference
/// to detect when DEX prices deviate, then arbs across DEXes.
pub struct OracleArbMonitor {
    rpc: Arc<MultiRpcProvider>,
    /// Tracked LST pairs: (token, oracle_feed, known_pools)
    pairs: Vec<OraclePair>,
    /// Last known oracle rates
    rates: Vec<Option<f64>>,
    /// Direct pool state cache: pool_addr -> (sqrtPriceX96, liquidity)
    pool_states: std::collections::HashMap<Address, (U256, u128)>,
}

struct OraclePair {
    name: &'static str,
    token: Address,
    oracle_feed: Address,
    /// Pools for this token paired with WETH, across DEXes
    weth_pools: Vec<Address>,
    /// Oracle rate decimals (18 for most Chainlink feeds)
    decimals: u32,
}

// Chainlink oracle feeds on Base
const WSTETH_STETH_FEED: Address = address!("B88BAc61a4Ca37C43a3725912B1f472c9A5bc061");
const CBETH_ETH_FEED: Address = address!("868a501e68F3D1E89CfC0D22F6b22E8dabce5F04");

// LST tokens
const WSTETH: Address = address!("c1CBa3fCea344f92D9239c08C0568f6F2F0ee452");
const CBETH: Address = address!("2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22");
const WETH: Address = address!("4200000000000000000000000000000000000006");

// Known deep liquidity pools for LSTs
const WSTETH_WETH_AERO: Address = address!("861A2922bE165a5Bd41b1E482B49216b465e1B5F");
const WSTETH_WETH_AERO_100: Address = address!("C5e47133b68c6c50298312829cB4d4f56eD43325");
const WSTETH_WETH_UNI_001: Address = address!("20E068D76f9E90b90604500B84c7e19dCB923e7e");
const WSTETH_WETH_UNI_005: Address = address!("6f4482cBF7b43599078fcb012732e20480015644");
const CBETH_WETH_AERO: Address = address!("47cA96Ea59C13F72745928887f84C9F52C3D7348");
const CBETH_WETH_UNI_001: Address = address!("A9DaFa443a02FBc907Cb0093276B3E6F4ef02A46");
const CBETH_WETH_UNI_005: Address = address!("10648BA41B8565907Cfa1496765fA4D95390aa0d");

impl OracleArbMonitor {
    pub fn new(rpc: Arc<MultiRpcProvider>) -> Self {
        let pairs = vec![
            OraclePair {
                name: "wstETH",
                token: WSTETH,
                oracle_feed: WSTETH_STETH_FEED,
                weth_pools: vec![
                    WSTETH_WETH_AERO,
                    WSTETH_WETH_AERO_100,
                    WSTETH_WETH_UNI_001,
                    WSTETH_WETH_UNI_005,
                ],
                decimals: 18,
            },
            OraclePair {
                name: "cbETH",
                token: CBETH,
                oracle_feed: CBETH_ETH_FEED,
                weth_pools: vec![
                    CBETH_WETH_AERO,
                    CBETH_WETH_UNI_001,
                    CBETH_WETH_UNI_005,
                ],
                decimals: 18,
            },
        ];
        let rates = vec![None; pairs.len()];
        Self { rpc, pairs, rates, pool_states: std::collections::HashMap::new() }
    }

    /// Refresh oracle rates from Chainlink
    pub async fn refresh_oracle_rates(&mut self) {
        for (i, pair) in self.pairs.iter().enumerate() {
            let provider = self.rpc.get();
            let call = latestAnswerCall {};
            let data = call.abi_encode();

            let tx = alloy::rpc::types::TransactionRequest::default()
                .to(pair.oracle_feed)
                .input(data.into());
            match provider.call(tx).await {
                Ok(result) => {
                    // Decode raw int256 from ABI response (32 bytes)
                    if result.len() >= 32 {
                        let raw = U256::from_be_slice(&result[..32]);
                        let rate_f64 = raw.saturating_to::<u128>() as f64 / 10f64.powi(pair.decimals as i32);
                        self.rates[i] = Some(rate_f64);
                        info!("Oracle {}: {:.6} ETH", pair.name, rate_f64);
                    }
                }
                Err(e) => {
                    warn!("Oracle {} feed failed: {}", pair.name, e);
                }
            }
        }
    }

    /// Refresh DEX prices for LST pools using multicall (same as arb engine)
    pub async fn refresh_dex_prices(&mut self) {
        let mut all_v3: Vec<Address> = Vec::new();
        for pair in &self.pairs {
            all_v3.extend_from_slice(&pair.weth_pools);
        }
        let results = crate::multicall::batch_v3_state(&self.rpc, &all_v3).await;
        let mut count = 0;
        for (addr, res) in all_v3.iter().zip(results.iter()) {
            if let Some(state) = res {
                self.pool_states.insert(*addr, (state.sqrt_price_x96, state.liquidity));
                count += 1;
            }
        }
        info!("Oracle DEX: {}/{} LST pools refreshed", count, all_v3.len());
    }

    /// Scan for oracle-guided cross-DEX arb opportunities.
    pub fn find_oracle_arb_opportunities(&self) -> Vec<OracleArbOpp> {
        let mut opps = Vec::new();

        for (i, pair) in self.pairs.iter().enumerate() {
            let oracle_rate = match self.rates[i] {
                Some(r) => r,
                None => continue,
            };

            // Get DEX implied prices from our own cached states
            let mut pool_prices: Vec<(Address, f64)> = Vec::new();

            for &pool_addr in &pair.weth_pools {
                if let Some((sqrt_price, liquidity)) = self.pool_states.get(&pool_addr) {
                    if *liquidity > 0 {
                        let price = self.calc_v3_price(*sqrt_price, pair.token, pool_addr);
                        if price > 0.0 {
                            pool_prices.push((pool_addr, price));
                        }
                    }
                }
            }

            // Log all prices for debugging
            if !pool_prices.is_empty() {
                let prices_str: Vec<String> = pool_prices.iter()
                    .map(|(addr, p)| format!("{:.6}@{}", p, &format!("{:?}", addr)[..8]))
                    .collect();
                info!("Oracle {} rate={:.6} | DEX prices: [{}]", pair.name, oracle_rate, prices_str.join(", "));
            }

            if pool_prices.len() < 2 { continue; }

            // Find cheapest and most expensive DEX prices
            pool_prices.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            let (cheapest_pool, cheapest_price) = pool_prices[0];
            let (expensive_pool, expensive_price) = pool_prices[pool_prices.len() - 1];

            // Cross-DEX spread
            let spread_pct = (expensive_price - cheapest_price) / cheapest_price * 100.0;

            // Oracle deviation (how far each is from "truth")
            let cheap_dev = (cheapest_price - oracle_rate) / oracle_rate * 100.0;
            let expensive_dev = (expensive_price - oracle_rate) / oracle_rate * 100.0;

            // Log significant deviations
            if spread_pct.abs() > 0.01 {
                info!(
                    "Oracle {} rate={:.6} | cheap={:.6} ({:.4}%) on {} | expensive={:.6} ({:.4}%) on {} | spread={:.4}%",
                    pair.name, oracle_rate, cheapest_price, cheap_dev, cheapest_pool,
                    expensive_price, expensive_dev, expensive_pool, spread_pct
                );
            }

            // Opportunity: buy cheap, sell expensive (oracle confirms direction)
            // Threshold: spread > 0.02% (accounts for V3 fees on low-fee pools)
            if spread_pct > 0.02 && cheap_dev < 0.0 {
                // Cheap pool is below oracle → underpriced → BUY here
                // Expensive pool is closer to oracle → SELL here
                opps.push(OracleArbOpp {
                    token: pair.token,
                    name: pair.name,
                    oracle_rate,
                    buy_pool: cheapest_pool,
                    buy_price: cheapest_price,
                    sell_pool: expensive_pool,
                    sell_price: expensive_price,
                    spread_pct,
                });
            }
        }

        opps
    }

    /// Calculate V3 pool price of LST in WETH terms from sqrtPriceX96
    fn calc_v3_price(&self, sqrt_price_x96: U256, lst_token: Address, pool_addr: Address) -> f64 {
        if sqrt_price_x96.is_zero() { return 0.0; }
        let sqrt_f = sqrt_price_x96.saturating_to::<u128>() as f64;
        let q96_f = (1u128 << 96) as f64;
        let price_f64 = (sqrt_f / q96_f) * (sqrt_f / q96_f);
        // V3: price = token1 per token0
        // token0 is the lower address
        // WETH = 0x4200...0006, all LSTs have addresses > 0x4200
        // So token0 = WETH, token1 = LST for all our pools
        // price = LST per WETH → we want WETH per LST = 1/price
        // BUT: for cbETH (0x2Ae3) < WETH (0x4200), so token0 = cbETH, token1 = WETH
        // price = WETH per cbETH (what we want directly)
        if lst_token < WETH {
            // LST is token0, price = WETH per LST (direct)
            price_f64
        } else {
            // WETH is token0, price = LST per WETH, invert
            if price_f64 > 0.0 { 1.0 / price_f64 } else { 0.0 }
        }
    }
}

#[derive(Debug, Clone)]
pub struct OracleArbOpp {
    pub token: Address,
    pub name: &'static str,
    pub oracle_rate: f64,
    pub buy_pool: Address,
    pub buy_price: f64,
    pub sell_pool: Address,
    pub sell_price: f64,
    pub spread_pct: f64,
}
