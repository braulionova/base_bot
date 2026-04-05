use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, error};

use crate::pools::{Pool, PoolTypeSerializable};
use crate::rpc::MultiRpcProvider;

/// Events emitted by the WebSocket feed
#[derive(Debug, Clone)]
pub enum ChainEvent {
    /// New block detected
    NewBlock { number: u64 },
    /// Swap detected on a monitored pool — reserves likely stale
    SwapDetected { pool: Address },
    /// New pool created
    NewPool { pool: Pool },
}

/// Real-time chain event feed via polling (upgradeable to WS)
/// Monitors newHeads + swap logs on priority pools
pub struct RealtimeFeed {
    rpc: Arc<MultiRpcProvider>,
    monitored_pools: Arc<DashMap<Address, Pool>>,
    tx: mpsc::Sender<ChainEvent>,
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
        let (tx, rx) = mpsc::channel(1000);
        (Self { rpc, monitored_pools, tx }, rx)
    }

    /// Run the real-time feed loop. Polls every ~1s for new blocks + swap events.
    /// With a local node at colocation, this gives ~1s latency.
    pub async fn run(&self, factory_addresses: Vec<Address>) {
        let provider = self.rpc.get();
        let mut last_block = match provider.get_block_number().await {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to get initial block: {}", e);
                return;
            }
        };

        info!("RealtimeFeed started at block {}", last_block);

        let v3_swap: FixedBytes<32> = V3_SWAP_SIG.parse().unwrap();
        let v2_swap: FixedBytes<32> = V2_SWAP_SIG.parse().unwrap();
        let v3_created: FixedBytes<32> = V3_POOL_CREATED.parse().unwrap();
        let v2_created: FixedBytes<32> = V2_PAIR_CREATED.parse().unwrap();

        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

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

            // Batch: fetch swap events on all monitored pools
            if !pool_addrs.is_empty() {
                // Split V2 and V3 pools
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

                // V3 swaps
                if !v3_addrs.is_empty() {
                    for chunk in v3_addrs.chunks(50) {
                        let filter = Filter::new()
                            .address(chunk.to_vec())
                            .event_signature(v3_swap)
                            .from_block(last_block + 1)
                            .to_block(current_block);

                        if let Ok(logs) = provider.get_logs(&filter).await {
                            for log in &logs {
                                let _ = self.tx.send(ChainEvent::SwapDetected {
                                    pool: log.address(),
                                }).await;
                            }
                        }
                    }
                }

                // V2 swaps
                if !v2_addrs.is_empty() {
                    for chunk in v2_addrs.chunks(50) {
                        let filter = Filter::new()
                            .address(chunk.to_vec())
                            .event_signature(v2_swap)
                            .from_block(last_block + 1)
                            .to_block(current_block);

                        if let Ok(logs) = provider.get_logs(&filter).await {
                            for log in &logs {
                                let _ = self.tx.send(ChainEvent::SwapDetected {
                                    pool: log.address(),
                                }).await;
                            }
                        }
                    }
                }
            }

            // Check for new pool creation events
            if !factory_addresses.is_empty() {
                // V3 factory events
                let filter_v3 = Filter::new()
                    .address(factory_addresses.clone())
                    .event_signature(v3_created)
                    .from_block(last_block + 1)
                    .to_block(current_block);

                if let Ok(logs) = provider.get_logs(&filter_v3).await {
                    for log in &logs {
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
                }

                // V2 factory events
                let filter_v2 = Filter::new()
                    .address(factory_addresses.clone())
                    .event_signature(v2_created)
                    .from_block(last_block + 1)
                    .to_block(current_block);

                if let Ok(logs) = provider.get_logs(&filter_v2).await {
                    for log in &logs {
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
            }

            last_block = current_block;
        }
    }
}
