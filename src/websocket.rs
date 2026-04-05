use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, error, debug};

use crate::pools::{Pool, PoolTypeSerializable};
use crate::rpc::MultiRpcProvider;

/// Events emitted by the WebSocket feed
#[derive(Debug, Clone)]
pub enum ChainEvent {
    /// New block detected
    NewBlock { number: u64 },
    /// Swap detected on a monitored pool — reserves likely stale
    SwapDetected { pool: Address },
    /// Large swap detected — backrun opportunity
    LargeSwap {
        pool: Address,
        block: u64,
        amount0: U256,
        amount1: U256,
        is_v3: bool,
    },
    /// New pool created
    NewPool { pool: Pool },
}

/// Real-time chain event feed via aggressive polling.
/// Monitors newHeads + swap logs on priority pools.
/// Tracks which pools had recent swaps for priority refresh.
pub struct RealtimeFeed {
    rpc: Arc<MultiRpcProvider>,
    monitored_pools: Arc<DashMap<Address, Pool>>,
    tx: mpsc::Sender<ChainEvent>,
    /// Pools with swaps since last refresh — used by main loop for priority refresh
    pub stale_pools: Arc<DashMap<Address, ()>>,
}

// Swap event signatures
const V3_SWAP_SIG: &str = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";
const V2_SWAP_SIG: &str = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822";

// Pool creation signatures
const V3_POOL_CREATED: &str = "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118";
const V2_PAIR_CREATED: &str = "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9";

impl RealtimeFeed {
    pub fn new(
        rpc: Arc<MultiRpcProvider>,
        monitored_pools: Arc<DashMap<Address, Pool>>,
    ) -> (Self, mpsc::Receiver<ChainEvent>) {
        let (tx, rx) = mpsc::channel(2000);
        let stale_pools = Arc::new(DashMap::new());
        (Self { rpc, monitored_pools, tx, stale_pools }, rx)
    }

    /// Run the real-time feed loop.
    /// Polls every 200ms for new blocks + swap events.
    /// Swap detection feeds into stale_pools for priority refresh.
    pub async fn run(&self, factory_addresses: Vec<Address>) {
        let provider = self.rpc.get();
        let mut last_block = match provider.get_block_number().await {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to get initial block: {}", e);
                return;
            }
        };

        info!("RealtimeFeed started at block {} (200ms polling)", last_block);

        let v3_swap: FixedBytes<32> = V3_SWAP_SIG.parse().unwrap();
        let v2_swap: FixedBytes<32> = V2_SWAP_SIG.parse().unwrap();
        let v3_created: FixedBytes<32> = V3_POOL_CREATED.parse().unwrap();
        let v2_created: FixedBytes<32> = V2_PAIR_CREATED.parse().unwrap();

        loop {
            // Poll every 50ms to react within flashblock window (200ms)
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

            let provider = self.rpc.get();
            let current_block = match provider.get_block_number().await {
                Ok(b) => b,
                Err(_) => continue,
            };

            if current_block <= last_block {
                continue;
            }

            // Emit new block event
            let _ = self.tx.send(ChainEvent::NewBlock { number: current_block }).await;

            // Collect monitored pool addresses for swap filter
            let pool_addrs: Vec<Address> = self.monitored_pools.iter()
                .map(|e| *e.key())
                .collect();

            // Fetch swap events — V2 and V3 in parallel
            if !pool_addrs.is_empty() {
                let mut v2_addrs = Vec::new();
                let mut v3_addrs = Vec::new();
                for addr in &pool_addrs {
                    if let Some(pool) = self.monitored_pools.get(addr) {
                        match pool.pool_type {
                            PoolTypeSerializable::V2 => v2_addrs.push(*addr),
                            PoolTypeSerializable::V3 => v3_addrs.push(*addr),
                        }
                    }
                }

                // Launch V2 and V3 swap queries in parallel
                let from = last_block + 1;
                let to = current_block;

                let v3_fut = {
                    let provider = self.rpc.get();
                    let addrs = v3_addrs.clone();
                    let sig = v3_swap;
                    async move {
                        let mut swaps: Vec<(Address, U256, U256)> = Vec::new();
                        for chunk in addrs.chunks(100) {
                            let filter = Filter::new()
                                .address(chunk.to_vec())
                                .event_signature(sig)
                                .from_block(from)
                                .to_block(to);
                            if let Ok(logs) = provider.get_logs(&filter).await {
                                for log in &logs {
                                    // V3 Swap(sender, recipient, amount0, amount1, sqrtPriceX96, liquidity, tick)
                                    // amount0 and amount1 are int256 in data[0..32] and data[32..64]
                                    let (a0, a1) = if log.data().data.len() >= 64 {
                                        let a0 = U256::from_be_slice(&log.data().data[0..32]);
                                        let a1 = U256::from_be_slice(&log.data().data[32..64]);
                                        (a0, a1)
                                    } else {
                                        (U256::ZERO, U256::ZERO)
                                    };
                                    swaps.push((log.address(), a0, a1));
                                }
                            }
                        }
                        swaps
                    }
                };

                let v2_fut = {
                    let provider = self.rpc.get();
                    let addrs = v2_addrs.clone();
                    let sig = v2_swap;
                    async move {
                        let mut swaps: Vec<(Address, U256, U256)> = Vec::new();
                        for chunk in addrs.chunks(100) {
                            let filter = Filter::new()
                                .address(chunk.to_vec())
                                .event_signature(sig)
                                .from_block(from)
                                .to_block(to);
                            if let Ok(logs) = provider.get_logs(&filter).await {
                                for log in &logs {
                                    // V2 Swap(sender, amount0In, amount1In, amount0Out, amount1Out, to)
                                    // data: [amount0In, amount1In, amount0Out, amount1Out]
                                    let (a0, a1) = if log.data().data.len() >= 128 {
                                        let a0_in = U256::from_be_slice(&log.data().data[0..32]);
                                        let a1_in = U256::from_be_slice(&log.data().data[32..64]);
                                        let a0_out = U256::from_be_slice(&log.data().data[64..96]);
                                        let a1_out = U256::from_be_slice(&log.data().data[96..128]);
                                        // Total volume = max of in/out
                                        (a0_in.max(a0_out), a1_in.max(a1_out))
                                    } else {
                                        (U256::ZERO, U256::ZERO)
                                    };
                                    swaps.push((log.address(), a0, a1));
                                }
                            }
                        }
                        swaps
                    }
                };

                let (v3_swaps, v2_swaps) = tokio::join!(v3_fut, v2_fut);

                // Large swap threshold: ~0.5 ETH equivalent (backrun-worthy)
                let large_threshold = U256::from(100_000_000_000_000_000u128); // 0.1 ETH — lower for more backrun chances

                // Mark swapped pools as stale + detect large swaps for backrun
                for (addr, a0, a1) in v3_swaps.iter() {
                    self.stale_pools.insert(*addr, ());
                    let _ = self.tx.send(ChainEvent::SwapDetected { pool: *addr }).await;
                    if *a0 > large_threshold || *a1 > large_threshold {
                        let _ = self.tx.send(ChainEvent::LargeSwap {
                            pool: *addr, block: current_block,
                            amount0: *a0, amount1: *a1, is_v3: true,
                        }).await;
                    }
                }
                for (addr, a0, a1) in v2_swaps.iter() {
                    self.stale_pools.insert(*addr, ());
                    let _ = self.tx.send(ChainEvent::SwapDetected { pool: *addr }).await;
                    if *a0 > large_threshold || *a1 > large_threshold {
                        let _ = self.tx.send(ChainEvent::LargeSwap {
                            pool: *addr, block: current_block,
                            amount0: *a0, amount1: *a1, is_v3: false,
                        }).await;
                    }
                }
            }

            // Check for new pool creation events — parallel V2+V3
            if !factory_addresses.is_empty() {
                let from = last_block + 1;
                let to = current_block;

                let v3_factory_fut = {
                    let provider = self.rpc.get();
                    let factories = factory_addresses.clone();
                    let sig = v3_created;
                    async move {
                        let filter = Filter::new()
                            .address(factories)
                            .event_signature(sig)
                            .from_block(from)
                            .to_block(to);
                        provider.get_logs(&filter).await.unwrap_or_default()
                    }
                };

                let v2_factory_fut = {
                    let provider = self.rpc.get();
                    let factories = factory_addresses.clone();
                    let sig = v2_created;
                    async move {
                        let filter = Filter::new()
                            .address(factories)
                            .event_signature(sig)
                            .from_block(from)
                            .to_block(to);
                        provider.get_logs(&filter).await.unwrap_or_default()
                    }
                };

                let (v3_logs, v2_logs) = tokio::join!(v3_factory_fut, v2_factory_fut);

                for log in &v3_logs {
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
                            dex_name: "NewV3".to_string(),
                            pool_type: PoolTypeSerializable::V3,
                            fee,
                            liquidity_usd: 0.0,
                            competition_score: 0,
                            last_bot_tx_count: 0,
                            last_updated_block: current_block,
                        };

                        info!("RT: New V3 pool {} ({}/{})", pool_addr, token0, token1);
                        let _ = self.tx.send(ChainEvent::NewPool { pool }).await;
                    }
                }

                for log in &v2_logs {
                    if log.topics().len() >= 3 && log.data().data.len() >= 64 {
                        let token0 = Address::from_word(log.topics()[1]);
                        let token1 = Address::from_word(log.topics()[2]);
                        let pair_addr = Address::from_slice(&log.data().data[12..32]);

                        let pool = Pool {
                            address: pair_addr,
                            token0,
                            token1,
                            dex_name: "NewV2".to_string(),
                            pool_type: PoolTypeSerializable::V2,
                            fee: 3000,
                            liquidity_usd: 0.0,
                            competition_score: 0,
                            last_bot_tx_count: 0,
                            last_updated_block: current_block,
                        };

                        info!("RT: New V2 pair {} ({}/{})", pair_addr, token0, token1);
                        let _ = self.tx.send(ChainEvent::NewPool { pool }).await;
                    }
                }
            }

            last_block = current_block;
        }
    }
}
