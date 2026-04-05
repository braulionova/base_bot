mod arbitrage;
mod blacklist;
mod competition;
mod config;
mod executor;
mod gas_predictor;
mod multicall;
mod pnl;
mod pools;
mod rpc;
mod safety;
mod signer;
mod telegram;
mod websocket;

use std::sync::Arc;
use tokio::sync::Mutex;
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

    info!("=== LONGTAIL ARB BOT V2 — Base Chain (Full Pipeline) ===");
    info!("=== 18 DEXes | Multicall | WS Feed | Gas Predictor | ML Blacklist | P&L ===");

    let cfg = Config::base_mainnet();

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

    let weth: alloy::primitives::Address = "0x4200000000000000000000000000000000000006".parse().unwrap();

    // ============================================================
    // 1. Initialize multi-RPC provider
    // ============================================================
    let rpc = Arc::new(rpc::MultiRpcProvider::new(cfg.rpc_urls.clone()).await?);
    rpc.health_check().await;

    // ============================================================
    // 2. Initialize subsystems
    // ============================================================
    let mut gas_pred = gas_predictor::GasPredictor::new(rpc.clone());
    gas_pred.sample().await; // initial sample

    let blacklist = Arc::new(Mutex::new(
        blacklist::TokenBlacklist::load("blacklist.json")
    ));

    let pnl_tracker = Arc::new(Mutex::new(
        pnl::PnlTracker::load("pnl.json", 50_000_000_000_000_000) // auto-withdraw at 0.05 ETH
    ));

    // ============================================================
    // 3. Pool discovery with disk persistence
    // ============================================================
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

    // ============================================================
    // 4. Pre-filter: cross-DEX arbable pools
    // ============================================================
    info!("Pre-filtering for cross-DEX arbable pools...");
    let mut pair_map: std::collections::HashMap<
        (alloy::primitives::Address, alloy::primitives::Address),
        Vec<alloy::primitives::Address>,
    > = std::collections::HashMap::new();

    for entry in discovery.pools.iter() {
        let p = entry.value();
        let key = if p.token0 < p.token1 { (p.token0, p.token1) } else { (p.token1, p.token0) };
        pair_map.entry(key).or_default().push(p.address);
    }

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
            arbable_addrs.extend(addrs.iter());
        }
    }
    info!("Arbable cross-DEX pools: {} (from {} total)", arbable_addrs.len(), total_pools);

    // ============================================================
    // 5. Batched safety checks via multicall
    // ============================================================
    let mut safety = safety::SafetyChecker::new(rpc.clone());
    info!("Batch checking token safety on {} arbable pools...", arbable_addrs.len());

    let mut unique_tokens: Vec<alloy::primitives::Address> = Vec::new();
    {
        let mut seen = std::collections::HashSet::new();
        for addr in &arbable_addrs {
            if let Some(pool) = discovery.pools.get(addr) {
                let t0 = pool.token0;
                let t1 = pool.token1;
                if seen.insert(t0) { unique_tokens.push(t0); }
                if seen.insert(t1) { unique_tokens.push(t1); }
            }
        }
    }
    info!("Checking {} unique tokens via multicall...", unique_tokens.len());
    let _safe_tokens = safety.batch_check_tokens(&unique_tokens).await;

    // Filter pools: both tokens safe + not blacklisted
    let bl = blacklist.lock().await;
    let safe_pools: Vec<alloy::primitives::Address> = arbable_addrs.iter()
        .filter(|addr| {
            if let Some(pool) = discovery.pools.get(*addr) {
                safety.check_pool_tokens_cached(pool.token0, pool.token1)
                    && bl.is_pair_safe(&pool.token0, &pool.token1)
            } else {
                false
            }
        })
        .copied()
        .collect();
    drop(bl);
    info!("{} pools passed safety + blacklist checks", safe_pools.len());

    // ============================================================
    // 6. Build pre-computed pair index
    // ============================================================
    let mut pair_index = arbitrage::PairIndex::build(&discovery.pools, &safe_pools, weth);
    info!(
        "Pair index: {} arbable pairs, {} WETH pools for triangular",
        pair_index.arbable_pairs.len(), pair_index.weth_pools.len()
    );
    let mut safe_pools = safe_pools;

    // ============================================================
    // 7. Initialize arb engine and executor
    // ============================================================
    let arb_engine = arbitrage::ArbitrageEngine::new(rpc.clone());
    let mut executor = executor::Executor::new(rpc.clone(), cfg.wallet_address, dry_run);
    executor.set_profit_params(cfg.gas_margin, cfg.min_net_profit_wei);

    if let Some(contract) = arb_contract {
        executor.set_arb_contract(contract);
    }

    if let Some(ref pk) = private_key {
        let rpc_url = &cfg.rpc_urls[0];
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

    // ============================================================
    // 8. Background tasks
    // ============================================================

    // 8a. Real-time feed (swap detection + new pools)
    let monitored_pools: Arc<dashmap::DashMap<alloy::primitives::Address, pools::Pool>> = Arc::new(dashmap::DashMap::new());
    for addr in &safe_pools {
        if let Some(pool) = discovery.pools.get(addr) {
            monitored_pools.insert(*addr, pool.clone());
        }
    }

    let (rt_feed, mut rt_rx) = websocket::RealtimeFeed::new(rpc.clone(), monitored_pools.clone());
    let stale_pools = rt_feed.stale_pools.clone();
    let factory_addrs: Vec<alloy::primitives::Address> = factories.iter().map(|f| f.factory).collect();

    tokio::spawn(async move {
        rt_feed.run(factory_addrs).await;
    });

    // 8b. Process real-time events (update stale pools)
    let discovery_pools_rt = discovery.pools.clone();
    tokio::spawn(async move {
        while let Some(event) = rt_rx.recv().await {
            match event {
                websocket::ChainEvent::NewPool { pool } => {
                    info!("RT: New pool {} on {}", pool.address, pool.dex_name);
                    discovery_pools_rt.insert(pool.address, pool);
                }
                websocket::ChainEvent::SwapDetected { pool: _ } => {
                    // Swap detected — reserves are stale. The main loop refresh
                    // will pick this up on next multicall batch.
                }
                websocket::ChainEvent::NewBlock { number: _ } => {
                    // Block progression tracked
                }
            }
        }
    });

    // 8c. RPC health check
    let rpc_health = rpc.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            rpc_health.health_check().await;
        }
    });

    // 8d. Auto-save cache + blacklist + PnL
    let cache_pools = discovery.pools.clone();
    let cache_path = cfg.pool_cache_path.clone();
    let bl_save = blacklist.clone();
    let pnl_save = pnl_tracker.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;

            // Save pool cache
            let pools_vec: Vec<pools::Pool> = cache_pools.iter().map(|p| p.value().clone()).collect();
            if let Ok(data) = serde_json::to_string(&pools_vec) {
                let _ = std::fs::write(&cache_path, data);
                info!("Auto-saved {} pools to cache", pools_vec.len());
            }

            // Save blacklist
            bl_save.lock().await.save();

            // Save PnL
            pnl_save.lock().await.save();
        }
    });

    // 8e. Hourly P&L Telegram report
    let pnl_hourly = pnl_tracker.clone();
    let bl_hourly = blacklist.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            let report = pnl_hourly.lock().await.hourly_summary();
            let bl_stats = bl_hourly.lock().await.stats_string();
            telegram::send(&format!("{}\n\n{}", report, bl_stats)).await;
        }
    });

    // ============================================================
    // 9. Main loop — Full optimized pipeline
    // ============================================================
    info!("=== MAIN LOOP STARTED (Full Pipeline V2) ===");
    info!("Pipeline: Gas sample -> Multicall refresh -> Cached scan -> Blacklist filter -> Simulate -> Execute");
    info!("Monitoring {} pools | {} arbable pairs | {} triangular bases",
        safe_pools.len(), pair_index.arbable_pairs.len(), pair_index.weth_pools.len());

    telegram::send(&format!(
        "🟢 *Bot V2 Started*\n\
         Pools: {} | Monitoring: {}\n\
         Arbable pairs: {} | Tri bases: {}\n\
         Features: Multicall, WS feed, Gas pred, ML blacklist, P&L\n\
         Mode: {}",
        total_pools, safe_pools.len(),
        pair_index.arbable_pairs.len(), pair_index.weth_pools.len(),
        if dry_run { "DRY RUN" } else { "LIVE" }
    )).await;

    let mut cycle = 0u64;
    let mut total_opps = 0u64;

    loop {
        cycle += 1;
        let cycle_start = std::time::Instant::now();

        // Step 0: Rebuild pair index every 500 cycles to pick up new RT pools
        if cycle % 500 == 0 {
            let all_addrs: Vec<alloy::primitives::Address> = discovery.pools.iter()
                .map(|e| *e.key())
                .collect();
            // Re-run safety filter on new pools
            let bl = blacklist.lock().await;
            safe_pools = all_addrs.iter()
                .filter(|addr| {
                    if let Some(pool) = discovery.pools.get(*addr) {
                        safety.check_pool_tokens_cached(pool.token0, pool.token1)
                            && bl.is_pair_safe(&pool.token0, &pool.token1)
                    } else {
                        false
                    }
                })
                .copied()
                .collect();
            drop(bl);
            pair_index = arbitrage::PairIndex::build(&discovery.pools, &safe_pools, weth);
            info!(
                "Rebuilt pair index: {} pools, {} arbable pairs, {} WETH pools",
                safe_pools.len(), pair_index.arbable_pairs.len(), pair_index.weth_pools.len()
            );
        }

        // Step 1: Sample gas price (every 10 cycles to reduce RPC)
        if cycle % 10 == 1 {
            gas_pred.sample().await;
        }

        // Step 2: Smart refresh — stale pools only (full refresh every 20 cycles)
        if cycle % 20 == 0 {
            // Full refresh periodically to catch drift
            arb_engine.refresh_reserves(&discovery.pools, &safe_pools).await;
        } else {
            // Only refresh pools with recent swaps + their pair partners
            arb_engine.refresh_stale_only(&discovery.pools, &safe_pools, &stale_pools).await;
        }
        let refresh_elapsed = cycle_start.elapsed();

        // Step 3: Scan opportunities using cached reserves (pure CPU)
        let scan_start = std::time::Instant::now();
        let mut opps = arb_engine.find_opportunities_cached(&discovery.pools, &pair_index);
        let scan_elapsed = scan_start.elapsed();

        // Step 4: Filter out blacklisted tokens
        {
            let bl = blacklist.lock().await;
            opps.retain(|opp| bl.is_pair_safe(&opp.token_in, &opp.token_bridge));
        }

        // Step 5: Filter by gas profitability
        // Dynamic floor: use gas predictor to estimate actual gas cost per opp type
        opps.retain(|opp| {
            let est_gas = match &opp.path {
                arbitrage::ArbPath::Direct { .. } => 350_000u64,
                arbitrage::ArbPath::Triangular { .. } => 500_000u64,
            };
            let (profitable, _) = gas_pred.net_profit_after_gas_dynamic(
                opp.profit_wei, est_gas, cfg.gas_margin, cfg.min_net_profit_wei,
            );
            profitable
        });

        total_opps += opps.len() as u64;

        // Step 6: Parallel simulate top 3, then execute best passing
        let top_opps: Vec<_> = opps.iter().take(3).cloned().collect();
        if !top_opps.is_empty() {
            // Fire off Telegram notifications non-blocking
            let gas_label = if gas_pred.is_gas_cheap() { "CHEAP ✓" } else if gas_pred.is_gas_expensive() { "HIGH ⚠" } else { "normal" };
            for opp in &top_opps {
                let tg_msg = format!(
                    "🔍 *Arb Detected*\n{} → {} | {:.6} ETH | Gas: {}",
                    opp.dex_a, opp.dex_b, opp.profit_eth, gas_label
                );
                tokio::spawn(async move { telegram::send(&tg_msg).await; });
            }

            // Parallel simulation of all candidates
            let mut sim_futures = Vec::new();
            for opp in &top_opps {
                sim_futures.push(executor.simulate_arb(opp, &gas_pred, &discovery.pools));
            }
            let sim_results = futures::future::join_all(sim_futures).await;

            // Execute first passing sim (highest profit first since opps are sorted)
            let mut executed = false;
            for (opp, sim_result) in top_opps.iter().zip(sim_results.iter()) {
                match sim_result {
                    Ok(true) => {
                        if !executed {
                            let tg_msg = format!(
                                "✅ *SIM PASSED* - Executing!\n{} → {} | {:.6} ETH",
                                opp.dex_a, opp.dex_b, opp.profit_eth
                            );
                            tokio::spawn(async move { telegram::send(&tg_msg).await; });

                            match executor.execute_arb(opp, &gas_pred, &pnl_tracker, &blacklist, &discovery.pools).await {
                                Ok(()) => {
                                    let tg_msg = format!(
                                        "⚡ *TX SENT*\n{} → {} | {:.6} ETH",
                                        opp.dex_a, opp.dex_b, opp.profit_eth
                                    );
                                    tokio::spawn(async move { telegram::send(&tg_msg).await; });
                                    executed = true;
                                }
                                Err(e) => {
                                    error!("Execution error: {}", e);
                                    let tg_msg = format!("❌ Exec error: {}", e);
                                    tokio::spawn(async move { telegram::send(&tg_msg).await; });
                                }
                            }
                        }
                    }
                    Ok(false) => {
                        let mut bl = blacklist.lock().await;
                        bl.record_revert(opp.token_bridge, 350_000, 0);
                    }
                    Err(e) => warn!("Sim error: {}", e),
                }
            }
        }

        // Step 7: Auto-withdraw check (every 50 cycles)
        if cycle % 50 == 0 {
            if let Ok(true) = executor.auto_withdraw(&pnl_tracker, weth).await {
                telegram::send("💰 *Auto-withdraw executed!*").await;
            }
        }

        // Periodic status report
        if cycle % 100 == 0 {
            let (attempts, successes, _) = executor.stats();
            let pnl_stats = pnl_tracker.lock().await.stats_string();
            let bl_stats = blacklist.lock().await.stats_string();
            let gas_stats = gas_pred.stats_string();

            info!(
                "Stats: {} cycles | {} opps | {} att | {} win | {} | {} | {}",
                cycle, total_opps, attempts, successes, pnl_stats, bl_stats, gas_stats
            );

            telegram::send(&format!(
                "📊 *Status Update*\n\
                 Cycle: {} | Pools: {}\n\
                 Opps: {} | Attempts: {} | Wins: {}\n\
                 {}\n\
                 {}\n\
                 {}",
                cycle, safe_pools.len(),
                total_opps, attempts, successes,
                pnl_stats, bl_stats, gas_stats,
            )).await;
        }

        let elapsed = cycle_start.elapsed();
        if cycle % 5 == 0 {
            info!(
                "Cycle {} | {:.1}ms ({:.1}ms refresh + {:.1}ms scan) | {} opps total",
                cycle,
                elapsed.as_secs_f64() * 1000.0,
                refresh_elapsed.as_secs_f64() * 1000.0,
                scan_elapsed.as_secs_f64() * 1000.0,
                total_opps
            );
        }

        // Adaptive sleep: shorter when opportunities found, longer when idle
        let sleep_ms = if !opps.is_empty() {
            // Hot: found opps this cycle, check again ASAP
            50
        } else if stale_pools.len() > 0 {
            // Warm: swaps happening but no arbs yet, check quickly
            100
        } else if elapsed.as_millis() < cfg.poll_interval_ms as u128 {
            // Cold: no activity, use normal interval
            cfg.poll_interval_ms
        } else {
            // Cycle was slow (heavy refresh), minimal sleep
            10
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(sleep_ms)).await;
    }
}
