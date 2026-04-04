use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use dashmap::DashMap;
use eyre::Result;
use std::sync::Arc;
use tracing::{info, warn};

use crate::pools::Pool;
use crate::rpc::MultiRpcProvider;

pub struct CompetitionDetector {
    rpc: Arc<MultiRpcProvider>,
}

impl CompetitionDetector {
    pub fn new(rpc: Arc<MultiRpcProvider>) -> Self {
        Self { rpc }
    }

    /// Analyze pools for bot competition by examining recent swap patterns.
    /// Returns addresses sorted by priority (no competition first).
    ///
    /// Strategy: Instead of checking each pool individually (20k+ RPC calls),
    /// we batch pools by address range and use broader filters.
    pub async fn analyze_pools(
        &self,
        pools: &DashMap<Address, Pool>,
        competition_threshold: u64,
    ) -> Result<Vec<Address>> {
        let provider = self.rpc.get();
        let latest_block = match provider.get_block_number().await {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to get block number: {}", e);
                return Ok(vec![]);
            }
        };

        // Swap event signatures
        let v3_swap_sig: FixedBytes<32> =
            "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
                .parse()
                .unwrap();
        let v2_swap_sig: FixedBytes<32> =
            "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
                .parse()
                .unwrap();

        let lookback_blocks = 200u64;
        let from_block = latest_block.saturating_sub(lookback_blocks);

        let mut no_competition: Vec<Address> = Vec::new();
        let mut low_competition: Vec<Address> = Vec::new();
        let mut high_competition: Vec<Address> = Vec::new();

        // Process pools in batches - query multiple pools at once
        let pool_addrs: Vec<Address> = pools.iter().map(|p| *p.key()).collect();
        let batch_size = 20; // Batch pools per RPC call

        for chunk in pool_addrs.chunks(batch_size) {
            // Separate V2 and V3 pools in this batch
            let mut v3_addrs = Vec::new();
            let mut v2_addrs = Vec::new();

            for &addr in chunk {
                if let Some(pool) = pools.get(&addr) {
                    match pool.pool_type {
                        crate::pools::PoolTypeSerializable::V3 => v3_addrs.push(addr),
                        crate::pools::PoolTypeSerializable::V2 => v2_addrs.push(addr),
                    }
                }
            }

            // Batch query V3 pools
            if !v3_addrs.is_empty() {
                let filter = Filter::new()
                    .address(v3_addrs.clone())
                    .event_signature(v3_swap_sig)
                    .from_block(from_block)
                    .to_block(latest_block);

                if let Ok(logs) = provider.get_logs(&filter).await {
                    // Count swaps per pool
                    let mut swap_counts: std::collections::HashMap<Address, Vec<u64>> =
                        std::collections::HashMap::new();

                    for log in &logs {
                        let addr = log.address();
                        let block = log.block_number.unwrap_or(0);
                        swap_counts.entry(addr).or_default().push(block);
                    }

                    for addr in &v3_addrs {
                        let bot_count = if let Some(blocks) = swap_counts.get(addr) {
                            count_bot_activity(blocks)
                        } else {
                            0
                        };
                        update_pool_competition(pools, *addr, bot_count, latest_block);
                        categorize(*addr, bot_count, competition_threshold,
                            &mut no_competition, &mut low_competition, &mut high_competition);
                    }
                } else {
                    // On error, assume no competition for these pools
                    for addr in &v3_addrs {
                        no_competition.push(*addr);
                    }
                }
            }

            // Batch query V2 pools
            if !v2_addrs.is_empty() {
                let filter = Filter::new()
                    .address(v2_addrs.clone())
                    .event_signature(v2_swap_sig)
                    .from_block(from_block)
                    .to_block(latest_block);

                if let Ok(logs) = provider.get_logs(&filter).await {
                    let mut swap_counts: std::collections::HashMap<Address, Vec<u64>> =
                        std::collections::HashMap::new();

                    for log in &logs {
                        let addr = log.address();
                        let block = log.block_number.unwrap_or(0);
                        swap_counts.entry(addr).or_default().push(block);
                    }

                    for addr in &v2_addrs {
                        let bot_count = if let Some(blocks) = swap_counts.get(addr) {
                            count_bot_activity(blocks)
                        } else {
                            0
                        };
                        update_pool_competition(pools, *addr, bot_count, latest_block);
                        categorize(*addr, bot_count, competition_threshold,
                            &mut no_competition, &mut low_competition, &mut high_competition);
                    }
                } else {
                    for addr in &v2_addrs {
                        no_competition.push(*addr);
                    }
                }
            }

            // Rate limit between batches for public RPCs
            tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
        }

        info!(
            "Competition: {} no-comp | {} low-comp | {} high-comp",
            no_competition.len(),
            low_competition.len(),
            high_competition.len()
        );

        let mut prioritized = no_competition;
        prioritized.extend(low_competition);
        Ok(prioritized)
    }
}

/// Count bot-like activity: back-to-back swaps in same block
fn count_bot_activity(block_numbers: &[u64]) -> u64 {
    if block_numbers.is_empty() {
        return 0;
    }
    let mut sorted = block_numbers.to_vec();
    sorted.sort();

    let mut bot_swaps = 0u64;
    let mut prev_block = 0u64;
    let mut same_block_count = 0u64;

    for &block in &sorted {
        if block == prev_block {
            same_block_count += 1;
            if same_block_count >= 2 {
                bot_swaps += 1;
            }
        } else {
            same_block_count = 0;
        }
        prev_block = block;
    }

    bot_swaps
}

fn update_pool_competition(pools: &DashMap<Address, Pool>, addr: Address, bot_count: u64, block: u64) {
    if let Some(mut pool) = pools.get_mut(&addr) {
        pool.competition_score = bot_count;
        pool.last_bot_tx_count = bot_count;
        pool.last_updated_block = block;
    }
}

fn categorize(
    addr: Address,
    bot_count: u64,
    threshold: u64,
    no_comp: &mut Vec<Address>,
    low_comp: &mut Vec<Address>,
    high_comp: &mut Vec<Address>,
) {
    if bot_count == 0 {
        no_comp.push(addr);
    } else if bot_count <= threshold {
        low_comp.push(addr);
    } else {
        high_comp.push(addr);
    }
}
