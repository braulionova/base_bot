mod arbitrage;
mod competition;
mod config;
mod executor;
mod pools;
mod rpc;
mod safety;
mod signer;
mod telegram;

use std::sync::Arc;
use tracing::{info, error, warn};

use config::Config;
use config::dex;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "longtail_bot=info".into()),
        )
        .with_target(false)
        .init();

    info!("=== LONGTAIL ARB BOT - Base Chain ===");
    info!("=== 18 DEXes | 20,000+ longtail pools | zero competition ===");

    let cfg = Config::base_mainnet();

    // Check for live execution mode
    let arb_contract = std::env::var("ARB_CONTRACT").ok()
        .and_then(|s| s.parse::<alloy::primitives::Address>().ok());
    let private_key = std::env::var("PRIVATE_KEY").ok();
    let dry_run = arb_contract.is_none() || private_key.is_none();

    if dry_run {
        info!("MODE: DRY RUN (simulation only)");
        info!("  To go live, set: ARB_CONTRACT=0x... PRIVATE_KEY=0x...");
    } else {
        info!("MODE: LIVE EXECUTION");
        info!("  Arb contract: {}", arb_contract.unwrap());
        info!("  Wallet: {}", cfg.wallet_address);
    }

    info!("RPCs: {} public endpoints", cfg.rpc_urls.len());
    info!("Min profit: {:.4} ETH", cfg.min_profit_wei as f64 / 1e18);

    // 1. Initialize multi-RPC provider
    let rpc = Arc::new(rpc::MultiRpcProvider::new(cfg.rpc_urls.clone()).await?);
    rpc.health_check().await;

    // 2. Pool discovery with disk persistence
    let factories = dex::factories();
    let discovery = pools::PoolDiscovery::new(rpc.clone());

    let cached = discovery.load_cache(&cfg.pool_cache_path).unwrap_or(0);
    info!("Loaded {} cached pools", cached);

    info!("Scanning {} DEX factories for new pools...", factories.len());
    discovery.discover_all(&factories).await?;

    let total_pools = discovery.pools.len();
    info!("Total pools: {}", total_pools);

    if let Err(e) = discovery.save_cache(&cfg.pool_cache_path) {
        warn!("Failed to save pool cache: {}", e);
    }

    if total_pools == 0 {
        warn!("No pools found. RPCs might be rate-limiting. Watching for new pools...");
        discovery.watch_new_pools(&factories).await?;
        return Ok(());
    }

    // 3. Pre-filter: only keep pools that have cross-DEX pairs (arbable)
    // This reduces 43k pools to ~100-200 that actually matter
    info!("Pre-filtering for cross-DEX arbable pools...");
    let mut pair_map: std::collections::HashMap<(alloy::primitives::Address, alloy::primitives::Address), Vec<alloy::primitives::Address>> = std::collections::HashMap::new();
    for entry in discovery.pools.iter() {
        let p = entry.value();
        let key = if p.token0 < p.token1 { (p.token0, p.token1) } else { (p.token1, p.token0) };
        pair_map.entry(key).or_default().push(p.address);
    }
    // Keep only pools whose pair exists on 2+ different DEXes
    let mut arbable_addrs: Vec<alloy::primitives::Address> = Vec::new();
    for (_pair, addrs) in &pair_map {
        if addrs.len() < 2 { continue; }
        let mut dexes = std::collections::HashSet::new();
        for addr in addrs {
            if let Some(p) = discovery.pools.get(addr) {
                dexes.insert(p.dex_name.clone());
            }
        }
        if dexes.len() >= 2 {
            arbable_addrs.extend(addrs);
        }
    }
    info!("Arbable cross-DEX pools: {} (from {} total)", arbable_addrs.len(), total_pools);

    // 4. Skip competition analysis on 43k pools (too slow with public RPCs)
    // Instead, treat all arbable pools as targets (longtail = low competition by nature)
    let detector = competition::CompetitionDetector::new(rpc.clone());

    // 5. Safety checks (only on arbable pools, not all 43k)
    let mut safety = safety::SafetyChecker::new(rpc.clone());
    info!("Checking token safety on {} arbable pools...", arbable_addrs.len());
    let mut safe_pools: Vec<alloy::primitives::Address> = Vec::new();
    for addr in &arbable_addrs {
        if let Some(pool) = discovery.pools.get(addr) {
            if safety.check_pool_tokens(pool.token0, pool.token1).await {
                safe_pools.push(*addr);
            }
        }
    }
    info!("{} pools passed safety checks", safe_pools.len());

    // 5. Initialize engine and executor
    let arb_engine = arbitrage::ArbitrageEngine::new(rpc.clone());
    let mut executor = executor::Executor::new(rpc.clone(), cfg.wallet_address, dry_run);

    if let Some(contract) = arb_contract {
        executor.set_arb_contract(contract);
    }

    // Initialize signing provider if private key is available
    if let Some(ref pk) = private_key {
        let rpc_url = &cfg.rpc_urls[0]; // Use first RPC for signing
        match executor.init_signer(pk, rpc_url) {
            Ok(_) => info!("Wallet signer initialized"),
            Err(e) => error!("Failed to init signer: {}", e),
        }
    }

    if !dry_run {
        if let Err(e) = executor.init_nonce().await {
            error!("Failed to init nonce: {}", e);
        }
    }

    // 6. Background: watch new pools
    let rpc_watcher = rpc.clone();
    let discovery_pools = discovery.pools.clone();
    let factories_clone = factories.clone();
    tokio::spawn(async move {
        let watcher = pools::PoolDiscovery {
            pools: discovery_pools,
            rpc: rpc_watcher,
        };
        if let Err(e) = watcher.watch_new_pools(&factories_clone).await {
            error!("Pool watcher error: {}", e);
        }
    });

    // 7. Background: RPC health check
    let rpc_health = rpc.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            rpc_health.health_check().await;
        }
    });

    // 8. Background: auto-save cache
    let cache_pools = discovery.pools.clone();
    let cache_path = cfg.pool_cache_path.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
            let pools_vec: Vec<pools::Pool> = cache_pools.iter().map(|p| p.value().clone()).collect();
            if let Ok(data) = serde_json::to_string(&pools_vec) {
                let _ = std::fs::write(&cache_path, data);
                info!("Auto-saved {} pools to cache", pools_vec.len());
            }
        }
    });

    // 9. Main loop
    info!("=== MAIN LOOP STARTED ===");
    info!("Monitoring {} priority longtail pools", safe_pools.len());
    info!("Strategy: Cross-DEX + Triangular arb on zero-competition pools");

    telegram::send(&format!(
        "🟢 *Bot Started*\n\
         Pools: {}\n\
         Monitoring: {}\n\
         Target: $0.05+/arb\n\
         Mode: {}",
        total_pools, safe_pools.len(),
        if dry_run { "DRY RUN" } else { "LIVE" }
    )).await;

    let mut cycle = 0u64;
    let mut total_opps = 0u64;

    loop {
        cycle += 1;
        let cycle_start = std::time::Instant::now();

        match arb_engine.find_opportunities(&discovery.pools, &safe_pools).await {
            Ok(opps) => {
                total_opps += opps.len() as u64;
                for opp in &opps {
                    // Notify on detection
                    telegram::send(&format!(
                        "🔍 *Arb Detected*\n\
                         {} → {}\n\
                         Profit: {:.6} ETH (~${:.2})\n\
                         Amount: {:.4} ETH\n\
                         Pool A: `{}`\n\
                         Pool B: `{}`",
                        opp.dex_a, opp.dex_b,
                        opp.profit_eth, opp.profit_eth * 2500.0,
                        arbitrage::wei_to_eth(opp.amount_in),
                        opp.pool_a, opp.pool_b
                    )).await;

                    match executor.simulate_arb(opp).await {
                        Ok(true) => {
                            telegram::send(&format!(
                                "✅ *SIM PASSED* - Executing!\n\
                                 {} → {} | {:.6} ETH profit",
                                opp.dex_a, opp.dex_b, opp.profit_eth
                            )).await;

                            match executor.execute_arb(opp).await {
                                Ok(()) => {
                                    telegram::send(&format!(
                                        "⚡ *TX SENT*\n\
                                         {} → {}\n\
                                         Expected: {:.6} ETH (~${:.2})",
                                        opp.dex_a, opp.dex_b,
                                        opp.profit_eth, opp.profit_eth * 2500.0
                                    )).await;
                                }
                                Err(e) => {
                                    error!("Execution error: {}", e);
                                    telegram::send(&format!("❌ Exec error: {}", e)).await;
                                }
                            }
                        }
                        Ok(false) => {}
                        Err(e) => warn!("Sim error: {}", e),
                    }
                }
            }
            Err(e) => {
                if cycle % 10 == 0 {
                    warn!("Cycle {} error: {}", cycle, e);
                }
            }
        }

        // Periodic status report (skip heavy re-analysis, just report)
        if cycle % 100 == 0 {
            let (attempts, successes, _profit) = executor.stats();
            info!(
                "Stats: {} cycles | {} opps | {} attempts | {} successes",
                cycle, total_opps, attempts, successes
            );
            telegram::send(&format!(
                "📊 *Status Update*\n\
                 Cycle: {}\n\
                 Pools: {} | Monitoring: {}\n\
                 Opps found: {} | Attempts: {} | Wins: {}",
                cycle, discovery.pools.len(), safe_pools.len(),
                total_opps, attempts, successes
            )).await;
        }

        let elapsed = cycle_start.elapsed();
        if cycle % 5 == 0 || !elapsed.is_zero() {
            info!(
                "Cycle {} | {:.1}s | monitoring: {} | opps: {}",
                cycle, elapsed.as_secs_f64(), safe_pools.len(), total_opps
            );
        }

        // Only sleep if cycle was fast (otherwise we're already behind)
        if elapsed.as_millis() < cfg.poll_interval_ms as u128 {
            tokio::time::sleep(tokio::time::Duration::from_millis(cfg.poll_interval_ms)).await;
        }
    }
}
