use alloy::providers::{Provider, ProviderBuilder};
use eyre::Result;
use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

// Use a boxed provider that erases the transport type
pub type BoxProvider = alloy::providers::fillers::FillProvider<
    alloy::providers::fillers::JoinFill<
        alloy::providers::Identity,
        alloy::providers::fillers::JoinFill<
            alloy::providers::fillers::GasFiller,
            alloy::providers::fillers::JoinFill<
                alloy::providers::fillers::BlobGasFiller,
                alloy::providers::fillers::JoinFill<
                    alloy::providers::fillers::NonceFiller,
                    alloy::providers::fillers::ChainIdFiller,
                >,
            >,
        >,
    >,
    alloy::providers::RootProvider,
>;

pub struct MultiRpcProvider {
    providers: Vec<BoxProvider>,
    urls: Vec<String>,
    current: AtomicUsize,
    health: Arc<RwLock<Vec<bool>>>,
    /// Latency in microseconds per provider (EWMA)
    latencies: Vec<AtomicU64>,
    /// Sorted indices by latency (updated on health check)
    sorted_indices: Arc<RwLock<Vec<usize>>>,
}

impl MultiRpcProvider {
    pub async fn new(urls: Vec<String>) -> Result<Self> {
        let mut providers = Vec::new();
        let mut health = Vec::new();
        let mut latencies = Vec::new();
        let n = urls.len();

        for url in &urls {
            let provider = ProviderBuilder::new()
                .connect_http(url.parse()?);
            providers.push(provider);
            health.push(true);
            latencies.push(AtomicU64::new(100_000)); // 100ms default
        }

        let sorted_indices: Vec<usize> = (0..n).collect();

        info!("Initialized {} RPC providers", providers.len());

        Ok(Self {
            providers,
            urls,
            current: AtomicUsize::new(0),
            health: Arc::new(RwLock::new(health)),
            latencies,
            sorted_indices: Arc::new(RwLock::new(sorted_indices)),
        })
    }

    /// Get next healthy provider via round-robin
    pub fn get(&self) -> &BoxProvider {
        let len = self.providers.len();
        let mut idx = self.current.fetch_add(1, Ordering::Relaxed) % len;

        if let Ok(h) = self.health.try_read() {
            for _ in 0..len {
                if h[idx] {
                    return &self.providers[idx];
                }
                idx = (idx + 1) % len;
            }
        }

        &self.providers[idx]
    }

    /// Get the fastest healthy provider (by measured latency)
    pub fn get_fastest(&self) -> &BoxProvider {
        if let Ok(sorted) = self.sorted_indices.try_read() {
            if let Ok(h) = self.health.try_read() {
                for &idx in sorted.iter() {
                    if h[idx] {
                        return &self.providers[idx];
                    }
                }
            }
        }
        self.get()
    }

    /// Get provider at specific index
    pub fn get_at(&self, idx: usize) -> &BoxProvider {
        &self.providers[idx % self.providers.len()]
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    /// Record a latency observation for a provider (EWMA with alpha=0.3)
    pub fn record_latency(&self, idx: usize, latency_us: u64) {
        if idx >= self.latencies.len() { return; }
        let current = self.latencies[idx].load(Ordering::Relaxed);
        // EWMA: new = alpha * sample + (1-alpha) * old
        let new_val = (latency_us * 3 + current * 7) / 10;
        self.latencies[idx].store(new_val, Ordering::Relaxed);
    }

    /// Get latency for a provider in microseconds
    pub fn get_latency(&self, idx: usize) -> u64 {
        if idx >= self.latencies.len() { return u64::MAX; }
        self.latencies[idx].load(Ordering::Relaxed)
    }

    pub async fn health_check(&self) {
        let mut health = self.health.write().await;
        let mut latency_pairs: Vec<(usize, u64)> = Vec::with_capacity(self.providers.len());

        for (i, provider) in self.providers.iter().enumerate() {
            let start = std::time::Instant::now();
            match provider.get_block_number().await {
                Ok(block) => {
                    let latency_us = start.elapsed().as_micros() as u64;
                    self.record_latency(i, latency_us);

                    if !health[i] {
                        info!("RPC {} recovered: {} (block {}, {:.1}ms)", i, self.urls[i], block, latency_us as f64 / 1000.0);
                    }
                    health[i] = true;
                    latency_pairs.push((i, self.latencies[i].load(Ordering::Relaxed)));
                }
                Err(_) => {
                    health[i] = false;
                    warn!("RPC {} failed health check: {}", i, self.urls[i]);
                    latency_pairs.push((i, u64::MAX));
                }
            }
        }

        // Update sorted indices by latency
        latency_pairs.sort_by_key(|&(_, lat)| lat);
        let mut sorted = self.sorted_indices.write().await;
        *sorted = latency_pairs.iter().map(|&(idx, _)| idx).collect();

        // Log ranking
        let ranking: Vec<String> = latency_pairs.iter()
            .filter(|&&(_, lat)| lat < u64::MAX)
            .map(|&(idx, lat)| format!("{}:{:.1}ms", &self.urls[idx][8..self.urls[idx].len().min(30)], lat as f64 / 1000.0))
            .collect();
        info!("RPC ranking: {}", ranking.join(" > "));
    }
}
