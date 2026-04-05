use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol;
use dashmap::DashMap;
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};
use tokio::sync::Semaphore;

use crate::config::dex::{DexFactory, PoolType};
use crate::rpc::MultiRpcProvider;

sol! {
    #[derive(Debug)]
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
        function fee() external view returns (uint24);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }

    #[derive(Debug)]
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub dex_name: String,
    pub pool_type: PoolTypeSerializable,
    pub fee: u32,
    pub liquidity_usd: f64,
    pub competition_score: u64,
    pub last_bot_tx_count: u64,
    pub last_updated_block: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum PoolTypeSerializable {
    V2,
    V3,
}

impl From<PoolType> for PoolTypeSerializable {
    fn from(pt: PoolType) -> Self {
        match pt {
            PoolType::UniswapV2 => PoolTypeSerializable::V2,
            PoolType::UniswapV3 => PoolTypeSerializable::V3,
        }
    }
}

pub struct PoolDiscovery {
    pub pools: Arc<DashMap<Address, Pool>>,
    pub rpc: Arc<MultiRpcProvider>,
}

impl PoolDiscovery {
    pub fn new(rpc: Arc<MultiRpcProvider>) -> Self {
        Self {
            pools: Arc::new(DashMap::new()),
            rpc,
        }
    }

    /// Load pools from disk cache
    pub fn load_cache(&self, path: &str) -> Result<usize> {
        let p = Path::new(path);
        if !p.exists() {
            info!("No pool cache found at {}", path);
            return Ok(0);
        }
        let data = std::fs::read_to_string(p)?;
        let cached: Vec<Pool> = serde_json::from_str(&data)?;
        let count = cached.len();
        for pool in cached {
            self.pools.insert(pool.address, pool);
        }
        info!("Loaded {} pools from cache", count);
        Ok(count)
    }

    /// Save pools to disk cache
    pub fn save_cache(&self, path: &str) -> Result<()> {
        let pools: Vec<Pool> = self.pools.iter().map(|p| p.value().clone()).collect();
        let data = serde_json::to_string(&pools)?;
        std::fs::write(path, data)?;
        info!("Saved {} pools to cache", pools.len());
        Ok(())
    }

    /// Get the last block we scanned (from cache)
    pub fn last_cached_block(&self) -> u64 {
        self.pools
            .iter()
            .map(|p| p.value().last_updated_block)
            .max()
            .unwrap_or(0)
    }

    pub async fn discover_all(&self, factories: &[DexFactory]) -> Result<()> {
        let provider = self.rpc.get();
        let latest_block = provider.get_block_number().await?;
        let cached_block = self.last_cached_block();

        // If we have cached pools, only scan from where we left off
        let start_block = if cached_block > 0 {
            info!("Resuming scan from cached block {}", cached_block);
            cached_block + 1
        } else {
            // First run: scan last 1M blocks (~3 weeks on Base)
            let lookback = 1_000_000u64;
            info!("First run: scanning last {} blocks", lookback);
            latest_block.saturating_sub(lookback)
        };

        if start_block >= latest_block {
            info!("Cache is up to date, no new blocks to scan");
            return Ok(());
        }

        // Parallel discovery across all factories (up to 4 concurrent)
        info!("Discovering pools from {} factories in parallel, blocks {}-{}",
            factories.len(), start_block, latest_block);

        let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
        let mut handles = Vec::new();

        for factory in factories {
            let sem = semaphore.clone();
            let rpc = self.rpc.clone();
            let pools = self.pools.clone();
            let factory = factory.clone();
            let sb = start_block;
            let lb = latest_block;

            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let disc = PoolDiscovery { pools, rpc };
                match factory.pool_type {
                    PoolType::UniswapV3 => {
                        if let Err(e) = disc.discover_v3_pools(&factory, sb, lb).await {
                            warn!("Error discovering {} pools: {}", factory.name, e);
                        }
                    }
                    PoolType::UniswapV2 => {
                        if let Err(e) = disc.discover_v2_pools(&factory, sb, lb).await {
                            warn!("Error discovering {} pools: {}", factory.name, e);
                        }
                    }
                }
                info!("{}: discovery complete", factory.name);
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }

        info!("All factories scanned. Total pools: {}", self.pools.len());
        Ok(())
    }

    async fn discover_v3_pools(&self, factory: &DexFactory, start_block: u64, latest_block: u64) -> Result<()> {
        // PoolCreated event signature
        let event_sig: FixedBytes<32> = "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118"
            .parse()?;

        // Public RPCs typically limit to 2k-10k blocks per request
        let mut chunk_size = 5_000u64;
        let mut from_block = start_block;
        let mut consecutive_errors = 0u32;

        while from_block < latest_block {
            let to_block = (from_block + chunk_size).min(latest_block);
            let provider = self.rpc.get();

            let filter = Filter::new()
                .address(factory.factory)
                .event_signature(event_sig)
                .from_block(from_block)
                .to_block(to_block);

            match provider.get_logs(&filter).await {
                Ok(logs) => {
                    consecutive_errors = 0;
                    // Grow chunk on success (up to 10k which most public RPCs allow)
                    if chunk_size < 10_000 {
                        chunk_size = (chunk_size * 2).min(10_000);
                    }

                    for log in logs {
                        if log.topics().len() >= 4 && log.data().data.len() >= 64 {
                            let token0 = Address::from_word(log.topics()[1]);
                            let token1 = Address::from_word(log.topics()[2]);
                            let fee_bytes = log.topics()[3];
                            let fee = u32::from_be_bytes(fee_bytes.0[28..32].try_into().unwrap_or([0; 4]));
                            let pool_addr = Address::from_slice(&log.data().data[44..64]);

                            let pool = Pool {
                                address: pool_addr,
                                token0,
                                token1,
                                dex_name: factory.name.to_string(),
                                pool_type: factory.pool_type.into(),
                                fee,
                                liquidity_usd: 0.0,
                                competition_score: u64::MAX,
                                last_bot_tx_count: u64::MAX,
                                last_updated_block: to_block,
                            };

                            self.pools.insert(pool_addr, pool);
                        }
                    }
                    from_block = to_block + 1;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    let err_str = format!("{}", e);
                    // RPC doesn't support getLogs or rate limiting -> rotate
                    if err_str.contains("not supported") || err_str.contains("timed out") {
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        continue;
                    }
                    // Range too large -> reduce chunk
                    if err_str.contains("range") || err_str.contains("block") || err_str.contains("10000") || err_str.contains("freetier") {
                        chunk_size = (chunk_size / 2).max(1_000);
                    } else {
                        chunk_size = (chunk_size / 2).max(2_000);
                    }
                    if consecutive_errors % 10 == 0 {
                        warn!(
                            "{}: logs {}-{} err (chunk -> {}): {}",
                            factory.name, from_block, to_block, chunk_size, e
                        );
                    }
                    if consecutive_errors > 30 {
                        warn!("Too many errors on {}, skipping", factory.name);
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        Ok(())
    }

    async fn discover_v2_pools(&self, factory: &DexFactory, start_block: u64, latest_block: u64) -> Result<()> {
        let event_sig: FixedBytes<32> = "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9"
            .parse()?;

        let mut chunk_size = 5_000u64;
        let mut from_block = start_block;
        let mut consecutive_errors = 0u32;

        while from_block < latest_block {
            let to_block = (from_block + chunk_size).min(latest_block);
            let provider = self.rpc.get();

            let filter = Filter::new()
                .address(factory.factory)
                .event_signature(event_sig)
                .from_block(from_block)
                .to_block(to_block);

            match provider.get_logs(&filter).await {
                Ok(logs) => {
                    consecutive_errors = 0;
                    if chunk_size < 10_000 {
                        chunk_size = (chunk_size * 2).min(10_000);
                    }

                    for log in logs {
                        if log.topics().len() >= 3 && log.data().data.len() >= 64 {
                            let token0 = Address::from_word(log.topics()[1]);
                            let token1 = Address::from_word(log.topics()[2]);
                            let pair_addr = Address::from_slice(&log.data().data[12..32]);

                            let pool = Pool {
                                address: pair_addr,
                                token0,
                                token1,
                                dex_name: factory.name.to_string(),
                                pool_type: factory.pool_type.into(),
                                fee: 3000,
                                liquidity_usd: 0.0,
                                competition_score: u64::MAX,
                                last_bot_tx_count: u64::MAX,
                                last_updated_block: to_block,
                            };

                            self.pools.insert(pair_addr, pool);
                        }
                    }
                    from_block = to_block + 1;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    let err_str = format!("{}", e);
                    if err_str.contains("not supported") || err_str.contains("timed out") {
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        continue;
                    }
                    if err_str.contains("range") || err_str.contains("block") || err_str.contains("freetier") {
                        chunk_size = (chunk_size / 2).max(1_000);
                    } else {
                        chunk_size = (chunk_size / 2).max(2_000);
                    }
                    if consecutive_errors % 10 == 0 {
                        warn!(
                            "{}: V2 logs {}-{} err (chunk -> {}): {}",
                            factory.name, from_block, to_block, chunk_size, e
                        );
                    }
                    if consecutive_errors > 30 {
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        Ok(())
    }

    /// Monitor for new pools in real-time
    pub async fn watch_new_pools(&self, factories: &[DexFactory]) -> Result<()> {
        let provider = self.rpc.get();
        let mut last_block = provider.get_block_number().await?;

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(4)).await;

            let current_block = match provider.get_block_number().await {
                Ok(b) => b,
                Err(_) => continue,
            };

            if current_block <= last_block {
                continue;
            }

            for factory in factories {
                let event_sig: FixedBytes<32> = match factory.pool_type {
                    PoolType::UniswapV3 => "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118".parse().unwrap(),
                    PoolType::UniswapV2 => "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9".parse().unwrap(),
                };

                let filter = Filter::new()
                    .address(factory.factory)
                    .event_signature(event_sig)
                    .from_block(last_block + 1)
                    .to_block(current_block);

                if let Ok(logs) = provider.get_logs(&filter).await {
                    for log in &logs {
                        match factory.pool_type {
                            PoolType::UniswapV3 if log.topics().len() >= 4 && log.data().data.len() >= 64 => {
                                let token0 = Address::from_word(log.topics()[1]);
                                let token1 = Address::from_word(log.topics()[2]);
                                let fee_bytes = log.topics()[3];
                                let fee = u32::from_be_bytes(fee_bytes.0[28..32].try_into().unwrap_or([0; 4]));
                                let pool_addr = Address::from_slice(&log.data().data[44..64]);

                                let pool = Pool {
                                    address: pool_addr,
                                    token0,
                                    token1,
                                    dex_name: factory.name.to_string(),
                                    pool_type: PoolTypeSerializable::V3,
                                    fee,
                                    liquidity_usd: 0.0,
                                    competition_score: 0, // new pool = no competition!
                                    last_bot_tx_count: 0,
                                    last_updated_block: current_block,
                                };

                                info!("NEW V3 pool: {} on {} ({}/{})", pool_addr, factory.name, token0, token1);
                                self.pools.insert(pool_addr, pool);
                            }
                            PoolType::UniswapV2 if log.topics().len() >= 3 && log.data().data.len() >= 64 => {
                                let token0 = Address::from_word(log.topics()[1]);
                                let token1 = Address::from_word(log.topics()[2]);
                                let pair_addr = Address::from_slice(&log.data().data[12..32]);

                                let pool = Pool {
                                    address: pair_addr,
                                    token0,
                                    token1,
                                    dex_name: factory.name.to_string(),
                                    pool_type: PoolTypeSerializable::V2,
                                    fee: 3000,
                                    liquidity_usd: 0.0,
                                    competition_score: 0,
                                    last_bot_tx_count: 0,
                                    last_updated_block: current_block,
                                };

                                info!("NEW V2 pair: {} on {} ({}/{})", pair_addr, factory.name, token0, token1);
                                self.pools.insert(pair_addr, pool);
                            }
                            _ => {}
                        }
                    }
                }
            }

            last_block = current_block;
        }
    }
}
