use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::network::EthereumWallet;
use alloy::rpc::types::TransactionRequest;
use eyre::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;
use tracing::{info, warn, error};

use crate::arbitrage::{ArbOpportunity, ArbPath, wei_to_eth};
use crate::blacklist::TokenBlacklist;
use crate::gas_predictor::GasPredictor;
use crate::pnl::{PnlTracker, TxType};
use crate::pools::{Pool, PoolTypeSerializable};
use crate::rpc::MultiRpcProvider;

sol! {
    #[sol(rpc)]
    interface IFlashArbV2 {
        function execDirect(
            address tokenIn,
            uint256 amountIn,
            address poolA,
            address poolB,
            bool poolAisV3,
            bool poolBisV3,
            address tokenBridge
        ) external;

        function execTriangular(
            uint256 amountIn,
            address pool1,
            address pool2,
            address pool3,
            bool pool1isV3,
            bool pool2isV3,
            bool pool3isV3,
            address tokenA,
            address tokenB
        ) external;

        function withdraw(address token) external;
    }
}

/// Thread-safe nonce manager for concurrent tx submission
pub struct NonceManager {
    current: AtomicU64,
    /// Pending nonces that have been allocated but not yet confirmed
    pending: Mutex<Vec<u64>>,
}

impl NonceManager {
    pub fn new(start: u64) -> Self {
        Self {
            current: AtomicU64::new(start),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Allocate the next nonce for a transaction
    pub async fn next(&self) -> u64 {
        let nonce = self.current.fetch_add(1, Ordering::SeqCst);
        self.pending.lock().await.push(nonce);
        nonce
    }

    /// Confirm a nonce was used (tx confirmed)
    pub async fn confirm(&self, nonce: u64) {
        let mut pending = self.pending.lock().await;
        pending.retain(|&n| n != nonce);
    }

    /// Resync nonce from chain (after errors)
    pub fn resync(&self, chain_nonce: u64) {
        self.current.store(chain_nonce, Ordering::SeqCst);
    }

    pub fn current(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }
}

pub struct Executor {
    rpc: Arc<MultiRpcProvider>,
    signing_provider: Option<Arc<dyn Provider>>,
    wallet: Address,
    arb_contract: Option<Address>,
    dry_run: bool,
    nonce_mgr: NonceManager,
    total_attempts: AtomicU64,
    total_successes: AtomicU64,
    total_profit: AtomicU64,
    /// Maximum concurrent pending transactions
    max_pending: usize,
    /// Gas margin multiplier for profit check
    gas_margin: f64,
    /// Absolute minimum net profit floor (wei)
    min_net_profit_wei: u128,
    /// MEV-protect RPC URL (Flashbots-like endpoint for Base)
    mev_rpc_url: Option<String>,
    /// Dedicated MEV signing provider (sends txs via MEV relay)
    mev_signing_provider: Option<Arc<dyn Provider>>,
}

impl Executor {
    pub fn new(rpc: Arc<MultiRpcProvider>, wallet: Address, dry_run: bool) -> Self {
        Self {
            rpc,
            signing_provider: None,
            wallet,
            arb_contract: None,
            dry_run,
            nonce_mgr: NonceManager::new(0),
            total_attempts: AtomicU64::new(0),
            total_successes: AtomicU64::new(0),
            total_profit: AtomicU64::new(0),
            max_pending: 4, // max 4 concurrent txs
            gas_margin: 1.5,
            min_net_profit_wei: 10_000_000_000_000, // 0.00001 ETH
            mev_rpc_url: std::env::var("MEV_RPC_URL").ok(),
            mev_signing_provider: None,
        }
    }

    pub fn set_profit_params(&mut self, gas_margin: f64, min_net_profit_wei: u128) {
        self.gas_margin = gas_margin;
        self.min_net_profit_wei = min_net_profit_wei;
    }

    pub fn has_mev_protect(&self) -> bool {
        self.mev_rpc_url.is_some()
    }


    pub fn init_signer(&mut self, private_key: &str, rpc_url: &str) -> Result<()> {
        let key_hex = private_key.strip_prefix("0x").unwrap_or(private_key);
        let signer: PrivateKeySigner = key_hex.parse()?;
        let address = signer.address();
        self.wallet = address;

        let wallet = EthereumWallet::from(signer.clone());

        let provider = ProviderBuilder::new()
            .wallet(wallet.clone())
            .connect_http(rpc_url.parse()?);

        self.signing_provider = Some(Arc::new(provider));
        info!("Signing provider initialized for {}", address);

        // Initialize MEV provider if MEV_RPC_URL is set
        if let Some(ref mev_url) = self.mev_rpc_url {
            let mev_provider = ProviderBuilder::new()
                .wallet(wallet)
                .connect_http(mev_url.parse()?);
            self.mev_signing_provider = Some(Arc::new(mev_provider));
            info!("MEV protect provider initialized: {}", mev_url);
        }

        Ok(())
    }

    pub fn set_arb_contract(&mut self, addr: Address) {
        self.arb_contract = Some(addr);
        info!("Arb contract set: {}", addr);
    }

    pub async fn init_nonce(&self) -> Result<()> {
        let provider = self.rpc.get();
        let nonce = provider.get_transaction_count(self.wallet).await?;
        self.nonce_mgr.resync(nonce);
        info!("Initialized nonce: {}", nonce);
        Ok(())
    }

    /// Simulate arb using gas predictor for cost estimation
    pub async fn simulate_arb(
        &self,
        opp: &ArbOpportunity,
        gas_predictor: &GasPredictor,
        pools: &dashmap::DashMap<Address, Pool>,
    ) -> Result<bool> {
        let estimated_gas = match &opp.path {
            ArbPath::Direct { .. } => 350_000u64,
            ArbPath::Triangular { .. } => 500_000u64,
        };

        // Use gas predictor with dynamic margin for accurate cost estimation
        let (profitable, net_profit) = gas_predictor.net_profit_after_gas_dynamic(
            opp.profit_wei, estimated_gas, self.gas_margin, self.min_net_profit_wei,
        );
        if !profitable {
            return Ok(false);
        }

        // eth_call simulation if we have a contract
        if let Some(contract_addr) = self.arb_contract {
            let provider = self.rpc.get();
            let calldata = self.encode_arb_call(opp, pools);
            let tx = TransactionRequest::default()
                .from(self.wallet)
                .to(contract_addr)
                .input(calldata.into());

            match provider.call(tx).await {
                Ok(_) => {
                    info!(
                        "SIM OK: {} -> {} | net {:.6} ETH",
                        opp.dex_a, opp.dex_b, wei_to_eth(net_profit)
                    );
                    return Ok(true);
                }
                Err(_) => {
                    return Ok(false);
                }
            }
        }

        // In dry run mode without contract, pass gas-profitable opps for logging
        if self.dry_run {
            info!(
                "SIM (dry): {} -> {} | net {:.6} ETH (gas check passed)",
                opp.dex_a, opp.dex_b, wei_to_eth(net_profit)
            );
            return Ok(true);
        }

        Ok(false)
    }

    /// Execute arb — supports concurrent submission with nonce management
    pub async fn execute_arb(
        &self,
        opp: &ArbOpportunity,
        gas_predictor: &GasPredictor,
        pnl: &Mutex<PnlTracker>,
        blacklist: &Mutex<TokenBlacklist>,
        pools: &dashmap::DashMap<Address, Pool>,
    ) -> Result<()> {
        self.total_attempts.fetch_add(1, Ordering::Relaxed);

        if self.dry_run {
            self.log_dry_run(opp);
            return Ok(());
        }

        let contract_addr = match self.arb_contract {
            Some(addr) => addr,
            None => {
                warn!("No arb contract set");
                return Ok(());
            }
        };

        let signing = match &self.signing_provider {
            Some(p) => p,
            None => {
                error!("No signing provider");
                return Ok(());
            }
        };

        let calldata = self.encode_arb_call(opp, pools);
        let calldata_clone = calldata.clone();

        let estimated_gas = match &opp.path {
            ArbPath::Direct { .. } => 400_000u64,
            ArbPath::Triangular { .. } => 600_000u64,
        };

        // Use gas predictor for optimal gas price
        let gas_price = gas_predictor.gas_for_urgency(0.7); // slightly aggressive
        let (base_fee, priority_tip) = gas_predictor.optimal_gas();

        let nonce = self.nonce_mgr.next().await;

        let tx = TransactionRequest::default()
            .to(contract_addr)
            .input(calldata.into())
            .gas_limit(estimated_gas)
            .nonce(nonce)
            .max_fee_per_gas(base_fee + priority_tip)
            .max_priority_fee_per_gas(priority_tip);

        // Use MEV-protect RPC if available (prevents frontrunning/sandwiching)
        let send_provider: &dyn Provider = if let Some(ref mev) = self.mev_signing_provider {
            info!("Sending via MEV-protect RPC");
            mev.as_ref()
        } else {
            signing.as_ref()
        };

        info!(
            "EXEC: {} -> {} | {:.6} ETH in | nonce {} | gas {:.2}gwei",
            opp.dex_a, opp.dex_b, wei_to_eth(opp.amount_in), nonce, gas_price as f64 / 1e9
        );

        match send_provider.send_transaction(tx).await {
            Ok(pending) => {
                let tx_hash = *pending.tx_hash();
                info!("TX SENT: {:?} nonce={}", tx_hash, nonce);

                match tokio::time::timeout(
                    tokio::time::Duration::from_secs(8),
                    pending.get_receipt()
                ).await {
                    Ok(Ok(receipt)) => {
                        self.nonce_mgr.confirm(nonce).await;
                        let gas_used = receipt.gas_used;
                        let block = receipt.block_number.unwrap_or(0);

                        if receipt.status() {
                            self.total_successes.fetch_add(1, Ordering::Relaxed);
                            info!("TX CONFIRMED: {:?} | gas: {}", tx_hash, gas_used);

                            let tx_type = match &opp.path {
                                ArbPath::Direct { .. } => TxType::Direct,
                                ArbPath::Triangular { .. } => TxType::Triangular,
                            };
                            pnl.lock().await.record_success(
                                block,
                                &opp.dex_a, &opp.dex_b,
                                opp.pool_a, opp.pool_b,
                                opp.token_in, opp.token_bridge,
                                wei_to_u128(opp.amount_in),
                                wei_to_u128(opp.profit_wei),
                                gas_used, gas_price,
                                tx_type,
                            );

                            blacklist.lock().await.record_success(opp.token_bridge, block);
                        } else {
                            warn!("TX REVERTED: {:?}", tx_hash);

                            pnl.lock().await.record_failure(
                                block,
                                &opp.dex_a, &opp.dex_b,
                                opp.pool_a, opp.pool_b,
                                opp.token_in, opp.token_bridge,
                                wei_to_u128(opp.amount_in),
                                gas_used, gas_price,
                                "reverted",
                            );

                            blacklist.lock().await.record_revert(opp.token_bridge, gas_used, block);
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("Receipt error: {}", e);
                        self.nonce_mgr.confirm(nonce).await;
                    }
                    Err(_) => {
                        // Timeout after 8s — attempt gas bump replacement
                        info!("TX stuck {:?} nonce={}, attempting gas bump...", tx_hash, nonce);

                        let bumped_tip = priority_tip * 130 / 100; // +30%
                        let bumped_base = base_fee + bumped_tip;
                        let bump_tx = TransactionRequest::default()
                            .to(contract_addr)
                            .input(calldata_clone.into())
                            .gas_limit(estimated_gas)
                            .nonce(nonce) // same nonce = replacement
                            .max_fee_per_gas(bumped_base)
                            .max_priority_fee_per_gas(bumped_tip);

                        match signing.send_transaction(bump_tx).await {
                            Ok(bump_pending) => {
                                let bump_hash = *bump_pending.tx_hash();
                                info!("GAS BUMP SENT: {:?} (was {:?})", bump_hash, tx_hash);
                                // Don't wait — nonce will resolve on its own
                            }
                            Err(e) => {
                                warn!("Gas bump failed: {}", e);
                            }
                        }
                        self.nonce_mgr.confirm(nonce).await;
                    }
                }
            }
            Err(e) => {
                error!("TX send failed: {}", e);
                self.nonce_mgr.confirm(nonce).await;

                if format!("{}", e).contains("nonce") {
                    let provider = self.rpc.get();
                    if let Ok(n) = provider.get_transaction_count(self.wallet).await {
                        self.nonce_mgr.resync(n);
                        warn!("Nonce re-synced to {}", n);
                    }
                }
            }
        }

        Ok(())
    }

    /// Execute auto-withdraw if PnL threshold is met
    pub async fn auto_withdraw(
        &self,
        pnl: &Mutex<PnlTracker>,
        weth: Address,
    ) -> Result<bool> {
        let should_withdraw = pnl.lock().await.should_withdraw();
        if !should_withdraw || self.dry_run { return Ok(false); }

        let contract_addr = match self.arb_contract {
            Some(addr) => addr,
            None => return Ok(false),
        };

        let signing = match &self.signing_provider {
            Some(p) => p,
            None => return Ok(false),
        };

        let calldata = IFlashArbV2::withdrawCall {
            token: weth,
        }.abi_encode();

        let tx = TransactionRequest::default()
            .to(contract_addr)
            .input(calldata.into())
            .gas_limit(100_000u64);

        match signing.send_transaction(tx).await {
            Ok(pending) => {
                let tx_hash = *pending.tx_hash();
                info!("WITHDRAW TX: {:?}", tx_hash);

                if let Ok(Ok(receipt)) = tokio::time::timeout(
                    tokio::time::Duration::from_secs(15),
                    pending.get_receipt()
                ).await {
                    if receipt.status() {
                        let balance = pnl.lock().await.contract_balance_wei;
                        pnl.lock().await.record_withdrawal(balance);
                        info!("Withdrew {:.6} ETH", balance as f64 / 1e18);
                        return Ok(true);
                    }
                }
            }
            Err(e) => error!("Withdraw failed: {}", e),
        }

        Ok(false)
    }

    /// Encode the arb call for FlashArbV2 contract (supports V2+V3 on both legs)
    fn encode_arb_call(&self, opp: &ArbOpportunity, pools: &dashmap::DashMap<Address, Pool>) -> Vec<u8> {
        match &opp.path {
            ArbPath::Direct { pool_buy, pool_sell } => {
                let pool_a_is_v3 = pools.get(pool_buy)
                    .map(|p| p.pool_type == PoolTypeSerializable::V3)
                    .unwrap_or(true);
                let pool_b_is_v3 = pools.get(pool_sell)
                    .map(|p| p.pool_type == PoolTypeSerializable::V3)
                    .unwrap_or(true);

                IFlashArbV2::execDirectCall {
                    tokenIn: opp.token_in,
                    amountIn: opp.amount_in,
                    poolA: *pool_buy,
                    poolB: *pool_sell,
                    poolAisV3: pool_a_is_v3,
                    poolBisV3: pool_b_is_v3,
                    tokenBridge: opp.token_bridge,
                }.abi_encode()
            }
            ArbPath::Triangular { pool1, pool2, pool3, token_a, token_b } => {
                let pool1_is_v3 = pools.get(pool1)
                    .map(|p| p.pool_type == PoolTypeSerializable::V3)
                    .unwrap_or(true);
                let pool2_is_v3 = pools.get(pool2)
                    .map(|p| p.pool_type == PoolTypeSerializable::V3)
                    .unwrap_or(true);
                let pool3_is_v3 = pools.get(pool3)
                    .map(|p| p.pool_type == PoolTypeSerializable::V3)
                    .unwrap_or(true);

                IFlashArbV2::execTriangularCall {
                    amountIn: opp.amount_in,
                    pool1: *pool1,
                    pool2: *pool2,
                    pool3: *pool3,
                    pool1isV3: pool1_is_v3,
                    pool2isV3: pool2_is_v3,
                    pool3isV3: pool3_is_v3,
                    tokenA: *token_a,
                    tokenB: *token_b,
                }.abi_encode()
            }
        }
    }

    fn log_dry_run(&self, opp: &ArbOpportunity) {
        match &opp.path {
            ArbPath::Direct { pool_buy, pool_sell } => {
                info!(
                    "[DRY] {} -> {} | {:.6} ETH profit | buy {} sell {}",
                    opp.dex_a, opp.dex_b, opp.profit_eth, pool_buy, pool_sell
                );
            }
            ArbPath::Triangular { pool1, pool2, pool3, token_a, token_b } => {
                info!(
                    "[DRY] Triangle WETH->{}->{} | {:.6} ETH | {} {} {}",
                    token_a, token_b, opp.profit_eth, pool1, pool2, pool3
                );
            }
        }
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.total_attempts.load(Ordering::Relaxed),
            self.total_successes.load(Ordering::Relaxed),
            self.total_profit.load(Ordering::Relaxed),
        )
    }
}

#[inline]
fn wei_to_u128(wei: U256) -> u128 {
    wei.to::<u128>()
}
