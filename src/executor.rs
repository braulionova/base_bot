use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::rpc::types::TransactionRequest;
use eyre::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn, error};

use crate::arbitrage::{ArbOpportunity, ArbPath, wei_to_eth};
use crate::rpc::MultiRpcProvider;

sol! {
    #[sol(rpc)]
    interface ILongtailArb {
        /// Flash swap arb: borrow from V3 poolA, sell on poolB, repay, profit
        function exec(
            address poolA,
            address poolB,
            address tokenIn,
            address tokenOut,
            uint256 amountIn,
            bool poolBisV3
        ) external;
    }
}

pub struct Executor {
    rpc: Arc<MultiRpcProvider>,
    signing_provider: Option<Arc<dyn Provider>>,
    wallet: Address,
    arb_contract: Option<Address>,
    dry_run: bool,
    nonce: AtomicU64,
    total_profit: AtomicU64,
    total_attempts: AtomicU64,
    total_successes: AtomicU64,
}

impl Executor {
    pub fn new(rpc: Arc<MultiRpcProvider>, wallet: Address, dry_run: bool) -> Self {
        Self {
            rpc,
            signing_provider: None,
            wallet,
            arb_contract: None,
            dry_run,
            nonce: AtomicU64::new(0),
            total_profit: AtomicU64::new(0),
            total_attempts: AtomicU64::new(0),
            total_successes: AtomicU64::new(0),
        }
    }

    /// Initialize signing provider from private key
    pub fn init_signer(&mut self, private_key: &str, rpc_url: &str) -> Result<()> {
        let key_hex = private_key.strip_prefix("0x").unwrap_or(private_key);
        let signer: PrivateKeySigner = key_hex.parse()?;
        let address = signer.address();
        self.wallet = address;

        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_http(rpc_url.parse()?);

        self.signing_provider = Some(Arc::new(provider));
        info!("Signing provider initialized for {}", address);
        Ok(())
    }

    pub fn set_arb_contract(&mut self, addr: Address) {
        self.arb_contract = Some(addr);
        info!("Arb contract set: {}", addr);
    }

    pub async fn init_nonce(&self) -> Result<()> {
        let provider = self.rpc.get();
        let nonce = provider.get_transaction_count(self.wallet).await?;
        self.nonce.store(nonce, Ordering::SeqCst);
        info!("Initialized nonce: {}", nonce);
        Ok(())
    }

    fn next_nonce(&self) -> u64 {
        self.nonce.fetch_add(1, Ordering::SeqCst)
    }

    pub async fn simulate_arb(&self, opp: &ArbOpportunity) -> Result<bool> {
        let provider = self.rpc.get();

        let gas_price = provider.get_gas_price().await.unwrap_or(100_000);
        let estimated_gas = match &opp.path {
            ArbPath::Direct { .. } => 350_000u128,
            ArbPath::Triangular { .. } => 500_000u128,
        };
        let gas_cost = U256::from(gas_price) * U256::from(estimated_gas);

        if opp.profit_wei <= gas_cost {
            return Ok(false);
        }

        // eth_call simulation if we have a contract
        if let Some(contract_addr) = self.arb_contract {
            let calldata = self.encode_arb_call(opp);
            let tx = TransactionRequest::default()
                .from(self.wallet)
                .to(contract_addr)
                .input(calldata.into());

            match provider.call(tx).await {
                Ok(_) => {
                    let net = opp.profit_wei - gas_cost;
                    info!(
                        "SIM OK: {} -> {} | net {:.6} ETH",
                        opp.dex_a, opp.dex_b, wei_to_eth(net)
                    );
                    return Ok(true);
                }
                Err(_) => {
                    return Ok(false);
                }
            }
        }

        // No contract = no simulation = don't execute (avoid wasting gas)
        Ok(false)
    }

    pub async fn execute_arb(&self, opp: &ArbOpportunity) -> Result<()> {
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
                error!("No signing provider - cannot send tx");
                return Ok(());
            }
        };

        let calldata = self.encode_arb_call(opp);

        let estimated_gas = match &opp.path {
            ArbPath::Direct { .. } => 400_000u64,
            ArbPath::Triangular { .. } => 600_000u64,
        };

        let tx = TransactionRequest::default()
            .to(contract_addr)
            .input(calldata.into())
            .gas_limit(estimated_gas);

        info!(
            "EXEC: {} -> {} | {:.6} ETH in | gas_limit {}",
            opp.dex_a, opp.dex_b, wei_to_eth(opp.amount_in), estimated_gas
        );

        match signing.send_transaction(tx).await {
            Ok(pending) => {
                let tx_hash = *pending.tx_hash();
                info!("TX SENT: {:?}", tx_hash);

                match tokio::time::timeout(
                    tokio::time::Duration::from_secs(15),
                    pending.get_receipt()
                ).await {
                    Ok(Ok(receipt)) => {
                        if receipt.status() {
                            self.total_successes.fetch_add(1, Ordering::Relaxed);
                            info!("TX CONFIRMED: {:?} | gas: {}", tx_hash, receipt.gas_used);
                        } else {
                            warn!("TX REVERTED: {:?}", tx_hash);
                        }
                    }
                    Ok(Err(e)) => warn!("Receipt error: {}", e),
                    Err(_) => info!("TX pending: {:?}", tx_hash),
                }
            }
            Err(e) => {
                error!("TX send failed: {}", e);
                if format!("{}", e).contains("nonce") {
                    let provider = self.rpc.get();
                    if let Ok(n) = provider.get_transaction_count(self.wallet).await {
                        self.nonce.store(n, Ordering::SeqCst);
                        warn!("Nonce re-synced to {}", n);
                    }
                }
            }
        }

        Ok(())
    }

    fn encode_arb_call(&self, opp: &ArbOpportunity) -> Vec<u8> {
        match &opp.path {
            ArbPath::Direct { pool_buy, pool_sell } => {
                // Flash swap: borrow from pool_buy (must be V3), sell on pool_sell
                let call = ILongtailArb::execCall {
                    poolA: *pool_buy,
                    poolB: *pool_sell,
                    tokenIn: opp.token_in,
                    tokenOut: opp.token_bridge,
                    amountIn: opp.amount_in,
                    poolBisV3: true, // TODO: lookup from pool data
                };
                call.abi_encode()
            }
            ArbPath::Triangular { pool1, pool2: _, pool3: _, token_a, token_b: _ } => {
                // For triangular, use first and last pool as direct arb
                // (simplified - full triangular needs contract update)
                let call = ILongtailArb::execCall {
                    poolA: *pool1,
                    poolB: opp.pool_b,
                    tokenIn: opp.token_in,
                    tokenOut: *token_a,
                    amountIn: opp.amount_in,
                    poolBisV3: true,
                };
                call.abi_encode()
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
