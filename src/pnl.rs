use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use tracing::info;

/// P&L Tracker: records every arb attempt with gas, profit, token, pool data.
/// Provides hourly summaries for Telegram dashboard and auto-withdraw logic.
#[derive(Default, Serialize, Deserialize)]
pub struct PnlTracker {
    /// All recorded transactions
    pub records: VecDeque<TxRecord>,
    /// Running totals
    pub total_gross_profit_wei: u128,
    pub total_gas_spent_wei: u128,
    pub total_net_profit_wei: u128,
    pub total_attempts: u64,
    pub total_successes: u64,
    pub total_reverts: u64,
    /// Current contract balance (tracked, not queried)
    pub contract_balance_wei: u128,
    /// Profit withdrawn so far
    pub total_withdrawn_wei: u128,
    /// Auto-withdraw threshold in wei
    pub withdraw_threshold_wei: u128,
    /// Maximum records to keep in memory
    #[serde(skip)]
    max_records: usize,
    /// Save path
    #[serde(skip)]
    save_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRecord {
    pub timestamp: i64,
    pub block_number: u64,
    pub tx_type: TxType,
    pub dex_a: String,
    pub dex_b: String,
    pub pool_a: Address,
    pub pool_b: Address,
    pub token_in: Address,
    pub token_bridge: Address,
    pub amount_in_wei: u128,
    pub gross_profit_wei: u128,
    pub gas_used: u64,
    pub gas_price_wei: u128,
    pub gas_cost_wei: u128,
    pub net_profit_wei: i128, // can be negative
    pub success: bool,
    pub revert_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TxType {
    Direct,
    Triangular,
    Simulation, // only simulated, not executed
}

impl PnlTracker {
    pub fn new(save_path: &str, withdraw_threshold_wei: u128) -> Self {
        Self {
            records: VecDeque::new(),
            total_gross_profit_wei: 0,
            total_gas_spent_wei: 0,
            total_net_profit_wei: 0,
            total_attempts: 0,
            total_successes: 0,
            total_reverts: 0,
            contract_balance_wei: 0,
            total_withdrawn_wei: 0,
            withdraw_threshold_wei,
            max_records: 10_000,
            save_path: save_path.to_string(),
        }
    }

    /// Load from disk
    pub fn load(path: &str, withdraw_threshold: u128) -> Self {
        let p = std::path::Path::new(path);
        if p.exists() {
            if let Ok(data) = std::fs::read_to_string(p) {
                if let Ok(mut tracker) = serde_json::from_str::<PnlTracker>(&data) {
                    tracker.save_path = path.to_string();
                    tracker.max_records = 10_000;
                    info!("Loaded PnL: {} records, net profit: {:.6} ETH",
                        tracker.records.len(),
                        tracker.total_net_profit_wei as f64 / 1e18
                    );
                    return tracker;
                }
            }
        }
        Self::new(path, withdraw_threshold)
    }

    /// Save to disk
    pub fn save(&self) {
        if self.save_path.is_empty() { return; }
        if let Ok(data) = serde_json::to_string(self) {
            let _ = std::fs::write(&self.save_path, data);
        }
    }

    /// Record a successful arb execution
    pub fn record_success(
        &mut self,
        block: u64,
        dex_a: &str,
        dex_b: &str,
        pool_a: Address,
        pool_b: Address,
        token_in: Address,
        token_bridge: Address,
        amount_in_wei: u128,
        gross_profit_wei: u128,
        gas_used: u64,
        gas_price_wei: u128,
        tx_type: TxType,
    ) {
        let gas_cost = gas_used as u128 * gas_price_wei;
        let net_profit = gross_profit_wei as i128 - gas_cost as i128;

        let record = TxRecord {
            timestamp: chrono::Utc::now().timestamp(),
            block_number: block,
            tx_type,
            dex_a: dex_a.to_string(),
            dex_b: dex_b.to_string(),
            pool_a,
            pool_b,
            token_in,
            token_bridge,
            amount_in_wei,
            gross_profit_wei,
            gas_used,
            gas_price_wei,
            gas_cost_wei: gas_cost,
            net_profit_wei: net_profit,
            success: true,
            revert_reason: String::new(),
        };

        self.total_attempts += 1;
        self.total_successes += 1;
        self.total_gross_profit_wei += gross_profit_wei;
        self.total_gas_spent_wei += gas_cost;
        if net_profit > 0 {
            self.total_net_profit_wei += net_profit as u128;
            self.contract_balance_wei += net_profit as u128;
        }

        self.push_record(record);
    }

    /// Record a failed arb (revert)
    pub fn record_failure(
        &mut self,
        block: u64,
        dex_a: &str,
        dex_b: &str,
        pool_a: Address,
        pool_b: Address,
        token_in: Address,
        token_bridge: Address,
        amount_in_wei: u128,
        gas_used: u64,
        gas_price_wei: u128,
        revert_reason: &str,
    ) {
        let gas_cost = gas_used as u128 * gas_price_wei;

        let record = TxRecord {
            timestamp: chrono::Utc::now().timestamp(),
            block_number: block,
            tx_type: TxType::Direct,
            dex_a: dex_a.to_string(),
            dex_b: dex_b.to_string(),
            pool_a,
            pool_b,
            token_in,
            token_bridge,
            amount_in_wei,
            gross_profit_wei: 0,
            gas_used,
            gas_price_wei,
            gas_cost_wei: gas_cost,
            net_profit_wei: -(gas_cost as i128),
            success: false,
            revert_reason: revert_reason.to_string(),
        };

        self.total_attempts += 1;
        self.total_reverts += 1;
        self.total_gas_spent_wei += gas_cost;

        self.push_record(record);
    }

    fn push_record(&mut self, record: TxRecord) {
        if self.records.len() >= self.max_records {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// Check if auto-withdraw should trigger
    pub fn should_withdraw(&self) -> bool {
        self.contract_balance_wei >= self.withdraw_threshold_wei && self.withdraw_threshold_wei > 0
    }

    /// Record that a withdrawal was made
    pub fn record_withdrawal(&mut self, amount_wei: u128) {
        self.total_withdrawn_wei += amount_wei;
        self.contract_balance_wei = self.contract_balance_wei.saturating_sub(amount_wei);
    }

    /// Get hourly summary for Telegram dashboard
    pub fn hourly_summary(&self) -> String {
        let now = chrono::Utc::now().timestamp();
        let hour_ago = now - 3600;

        let recent: Vec<&TxRecord> = self.records.iter()
            .filter(|r| r.timestamp >= hour_ago)
            .collect();

        let hour_attempts = recent.len();
        let hour_successes = recent.iter().filter(|r| r.success).count();
        let hour_gross: u128 = recent.iter().map(|r| r.gross_profit_wei).sum();
        let hour_gas: u128 = recent.iter().map(|r| r.gas_cost_wei).sum();
        let hour_net: i128 = recent.iter().map(|r| r.net_profit_wei).sum();

        format!(
            "📊 *Hourly P&L Report*\n\
             \n\
             *Last Hour:*\n\
             Attempts: {} | Success: {} | Win rate: {:.0}%\n\
             Gross: {:.6} ETH\n\
             Gas: {:.6} ETH\n\
             Net: {:.6} ETH (~${:.2})\n\
             \n\
             *All Time:*\n\
             Attempts: {} | Wins: {} | Reverts: {}\n\
             Gross: {:.6} ETH\n\
             Gas spent: {:.6} ETH\n\
             Net profit: {:.6} ETH (~${:.2})\n\
             Withdrawn: {:.6} ETH\n\
             Balance: {:.6} ETH",
            hour_attempts,
            hour_successes,
            if hour_attempts > 0 { hour_successes as f64 / hour_attempts as f64 * 100.0 } else { 0.0 },
            hour_gross as f64 / 1e18,
            hour_gas as f64 / 1e18,
            hour_net as f64 / 1e18,
            hour_net as f64 / 1e18 * 2500.0,
            self.total_attempts,
            self.total_successes,
            self.total_reverts,
            self.total_gross_profit_wei as f64 / 1e18,
            self.total_gas_spent_wei as f64 / 1e18,
            self.total_net_profit_wei as f64 / 1e18,
            self.total_net_profit_wei as f64 / 1e18 * 2500.0,
            self.total_withdrawn_wei as f64 / 1e18,
            self.contract_balance_wei as f64 / 1e18,
        )
    }

    /// Compact stats string for logging
    pub fn stats_string(&self) -> String {
        format!(
            "pnl: {}att {}win {}rev net={:.6}ETH gas={:.6}ETH bal={:.6}ETH",
            self.total_attempts,
            self.total_successes,
            self.total_reverts,
            self.total_net_profit_wei as f64 / 1e18,
            self.total_gas_spent_wei as f64 / 1e18,
            self.contract_balance_wei as f64 / 1e18,
        )
    }
}
