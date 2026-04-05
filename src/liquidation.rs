use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::rpc::types::TransactionRequest;
use eyre::Result;
use std::sync::Arc;
use tracing::{info, warn, debug};

use crate::rpc::MultiRpcProvider;

sol! {
    #[sol(rpc)]
    interface IAavePool {
        function getUserAccountData(address user) external view returns (
            uint256 totalCollateralBase,
            uint256 totalDebtBase,
            uint256 availableBorrowsBase,
            uint256 currentLiquidationThreshold,
            uint256 ltv,
            uint256 healthFactor
        );

        function liquidationCall(
            address collateralAsset,
            address debtAsset,
            address user,
            uint256 debtToCover,
            bool receiveAToken
        ) external;

        function flashLoanSimple(
            address receiverAddress,
            address asset,
            uint256 amount,
            bytes calldata params,
            uint16 referralCode
        ) external;
    }

    #[sol(rpc)]
    interface IAaveDataProvider {
        function getUserReserveData(address asset, address user) external view returns (
            uint256 currentATokenBalance,
            uint256 currentStableDebt,
            uint256 currentVariableDebt,
            uint256 principalStableDebt,
            uint256 scaledVariableDebt,
            uint256 stableBorrowRate,
            uint256 liquidityRate,
            uint40 stableRateLastUpdated,
            bool usageAsCollateralEnabled
        );

        function getReserveTokensAddresses(address asset) external view returns (
            address aTokenAddress,
            address stableDebtTokenAddress,
            address variableDebtTokenAddress
        );
    }
}

/// Aave V3 addresses on Base
const AAVE_POOL: &str = "0xA238Dd80C259a72e81d7e4664a9801593F98d1c5";
const AAVE_DATA_PROVIDER: &str = "0x2d8A3C5677189723C4cB8873CfC9C8976FDF38Ac";

/// Tokens to monitor for liquidation on Base
const MONITORED_ASSETS: &[(&str, &str)] = &[
    ("0x4200000000000000000000000000000000000006", "WETH"),
    ("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", "USDC"),
    ("0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb", "DAI"),
    ("0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA", "USDbC"),
    ("0x2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22", "cbETH"),
    ("0xc1CBa3fCea344f92D9239c08C0568f6F2F0ee452", "wstETH"),
];

/// Health factor threshold — below 1e18 = liquidatable
const HEALTH_FACTOR_THRESHOLD: u128 = 1_000_000_000_000_000_000; // 1.0

/// Maximum users to track
const MAX_TRACKED_USERS: usize = 500;

#[derive(Debug, Clone)]
pub struct LiquidationOpportunity {
    pub user: Address,
    pub collateral_asset: Address,
    pub debt_asset: Address,
    pub debt_to_cover: U256,
    pub health_factor: U256,
    pub estimated_profit_wei: u128,
    pub collateral_name: String,
    pub debt_name: String,
}

pub struct LiquidationMonitor {
    rpc: Arc<MultiRpcProvider>,
    aave_pool: Address,
    data_provider: Address,
    /// Users with positions to monitor (discovered from events)
    tracked_users: Vec<Address>,
    /// Stats
    pub total_checks: u64,
    pub total_liquidatable: u64,
    pub total_liquidated: u64,
    pub total_profit_wei: u128,
}

impl LiquidationMonitor {
    pub fn new(rpc: Arc<MultiRpcProvider>) -> Self {
        Self {
            rpc,
            aave_pool: AAVE_POOL.parse().unwrap(),
            data_provider: AAVE_DATA_PROVIDER.parse().unwrap(),
            tracked_users: Vec::new(),
            total_checks: 0,
            total_liquidatable: 0,
            total_liquidated: 0,
            total_profit_wei: 0,
        }
    }

    /// Discover users with Aave positions by scanning recent Supply/Borrow events
    pub async fn discover_users(&mut self, lookback_blocks: u64) -> Result<usize> {
        let provider = self.rpc.get();
        let latest = provider.get_block_number().await?;
        let from = latest.saturating_sub(lookback_blocks);

        // Supply event: 0x2b627736bca15cd5381dcf80b0bf11fd197d01a037c52b927a881a10fb73ba61
        // Borrow event: 0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0
        let supply_sig: alloy::primitives::FixedBytes<32> =
            "0x2b627736bca15cd5381dcf80b0bf11fd197d01a037c52b927a881a10fb73ba61".parse()?;
        let borrow_sig: alloy::primitives::FixedBytes<32> =
            "0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0".parse()?;

        let mut users = std::collections::HashSet::new();

        // Fetch supply events
        for chunk_start in (from..latest).step_by(10_000) {
            let chunk_end = (chunk_start + 10_000).min(latest);

            let filter = alloy::rpc::types::Filter::new()
                .address(self.aave_pool)
                .event_signature(vec![supply_sig, borrow_sig])
                .from_block(chunk_start)
                .to_block(chunk_end);

            match provider.get_logs(&filter).await {
                Ok(logs) => {
                    for log in &logs {
                        // Both Supply and Borrow have user as topic[2]
                        if log.topics().len() >= 3 {
                            let user = Address::from_word(log.topics()[2]);
                            users.insert(user);
                        }
                    }
                }
                Err(e) => {
                    debug!("Liquidation: log fetch error: {}", e);
                }
            }

            if users.len() >= MAX_TRACKED_USERS { break; }
        }

        self.tracked_users = users.into_iter().collect();
        info!("Liquidation: tracking {} Aave users", self.tracked_users.len());
        Ok(self.tracked_users.len())
    }

    /// Scan tracked users for liquidation opportunities
    pub async fn scan_opportunities(&mut self) -> Vec<LiquidationOpportunity> {
        let mut opportunities = Vec::new();
        if self.tracked_users.is_empty() { return opportunities; }

        let provider = self.rpc.get();
        self.total_checks += 1;

        // Check health factors in batches
        for chunk in self.tracked_users.chunks(20) {
            for &user in chunk {
                let pool = IAavePool::new(self.aave_pool, provider);
                match pool.getUserAccountData(user).call().await {
                    Ok(data) => {
                        let health = data.healthFactor;

                        // Liquidatable if healthFactor < 1e18
                        if health < U256::from(HEALTH_FACTOR_THRESHOLD) && health > U256::ZERO {
                            let total_debt = data.totalDebtBase;
                            let total_collateral = data.totalCollateralBase;

                            if total_debt.is_zero() { continue; }

                            // Can liquidate up to 50% of debt
                            let max_liquidatable = total_debt / U256::from(2);

                            // Liquidation bonus is typically 5-10% of collateral
                            // Estimate profit as ~5% of liquidatable amount
                            let est_profit = max_liquidatable / U256::from(20);

                            // Find the actual debt and collateral assets
                            let (debt_asset, debt_name, collateral_asset, collateral_name) =
                                self.find_user_assets(provider, user).await;

                            if debt_asset != Address::ZERO {
                                self.total_liquidatable += 1;
                                info!(
                                    "LIQUIDATABLE: {} hf={:.4} debt={} collateral={} est_profit={:.6}ETH",
                                    user,
                                    health.to::<u128>() as f64 / 1e18,
                                    debt_name,
                                    collateral_name,
                                    est_profit.to::<u128>() as f64 / 1e18,
                                );

                                opportunities.push(LiquidationOpportunity {
                                    user,
                                    collateral_asset,
                                    debt_asset,
                                    debt_to_cover: max_liquidatable,
                                    health_factor: health,
                                    estimated_profit_wei: est_profit.to::<u128>(),
                                    collateral_name,
                                    debt_name,
                                });
                            }
                        }
                    }
                    Err(_) => {} // skip on error
                }
            }
        }

        opportunities.sort_by(|a, b| b.estimated_profit_wei.cmp(&a.estimated_profit_wei));
        opportunities
    }

    /// Find user's largest debt and collateral positions
    async fn find_user_assets(
        &self,
        provider: &impl Provider,
        user: Address,
    ) -> (Address, String, Address, String) {
        let dp = IAaveDataProvider::new(self.data_provider, provider);
        let mut max_debt: U256 = U256::ZERO;
        let mut debt_asset = Address::ZERO;
        let mut debt_name = String::new();
        let mut max_collateral: U256 = U256::ZERO;
        let mut collateral_asset = Address::ZERO;
        let mut collateral_name = String::new();

        for &(addr_str, name) in MONITORED_ASSETS {
            let asset: Address = addr_str.parse().unwrap();
            if let Ok(data) = dp.getUserReserveData(asset, user).call().await {
                let total_debt = data.currentVariableDebt + data.currentStableDebt;
                if total_debt > max_debt {
                    max_debt = total_debt;
                    debt_asset = asset;
                    debt_name = name.to_string();
                }
                if data.currentATokenBalance > max_collateral && data.usageAsCollateralEnabled {
                    max_collateral = data.currentATokenBalance;
                    collateral_asset = asset;
                    collateral_name = name.to_string();
                }
            }
        }

        (debt_asset, debt_name, collateral_asset, collateral_name)
    }

    /// Simulate a liquidation via eth_call
    pub async fn simulate_liquidation(&self, opp: &LiquidationOpportunity) -> Result<bool> {
        let provider = self.rpc.get();

        let calldata = IAavePool::liquidationCallCall {
            collateralAsset: opp.collateral_asset,
            debtAsset: opp.debt_asset,
            user: opp.user,
            debtToCover: opp.debt_to_cover,
            receiveAToken: false,
        }.abi_encode();

        let tx = TransactionRequest::default()
            .to(self.aave_pool)
            .input(calldata.into());

        match provider.call(tx).await {
            Ok(_) => {
                info!("LIQUIDATION SIM OK: user={} profit~{:.6}ETH",
                    opp.user, opp.estimated_profit_wei as f64 / 1e18);
                Ok(true)
            }
            Err(e) => {
                debug!("Liquidation sim failed for {}: {}", opp.user, e);
                Ok(false)
            }
        }
    }

    pub fn tracked_count(&self) -> usize {
        self.tracked_users.len()
    }

    pub fn stats_string(&self) -> String {
        format!(
            "liq: {} users, {} checks, {} liquidatable, {} executed, {:.6}ETH profit",
            self.tracked_users.len(),
            self.total_checks,
            self.total_liquidatable,
            self.total_liquidated,
            self.total_profit_wei as f64 / 1e18,
        )
    }
}
