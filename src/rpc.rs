use alloy::providers::{Provider, ProviderBuilder};
use eyre::Result;
use std::sync::atomic::{AtomicUsize, Ordering};
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
}

impl MultiRpcProvider {
    pub async fn new(urls: Vec<String>) -> Result<Self> {
        let mut providers = Vec::new();
        let mut health = Vec::new();

        for url in &urls {
            let provider = ProviderBuilder::new()
                .connect_http(url.parse()?);
            providers.push(provider);
            health.push(true);
        }

        info!("Initialized {} RPC providers", providers.len());

        Ok(Self {
            providers,
            urls,
            current: AtomicUsize::new(0),
            health: Arc::new(RwLock::new(health)),
        })
    }

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

    pub async fn health_check(&self) {
        let mut health = self.health.write().await;
        for (i, provider) in self.providers.iter().enumerate() {
            match provider.get_block_number().await {
                Ok(block) => {
                    if !health[i] {
                        info!("RPC {} recovered: {} (block {})", i, self.urls[i], block);
                    }
                    health[i] = true;
                }
                Err(_) => {
                    health[i] = false;
                    warn!("RPC {} failed health check: {}", i, self.urls[i]);
                }
            }
        }
    }
}
