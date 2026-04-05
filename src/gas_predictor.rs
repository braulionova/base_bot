use std::collections::VecDeque;
use std::sync::Arc;
use alloy::primitives::U256;
use alloy::providers::Provider;
use tracing::warn;

use crate::rpc::MultiRpcProvider;

/// Rolling window gas price predictor.
/// Tracks gas prices by hour-of-day and uses historical median to predict optimal gas.
pub struct GasPredictor {
    rpc: Arc<MultiRpcProvider>,
    /// Gas prices grouped by hour (0-23), each a rolling window of recent observations
    hourly_prices: [VecDeque<u128>; 24],
    /// Global recent prices (last N blocks)
    recent_prices: VecDeque<u128>,
    /// Maximum observations per hour bucket
    max_per_hour: usize,
    /// Maximum recent observations
    max_recent: usize,
    /// Current gas price (cached)
    current_gas: u128,
    /// Total samples collected
    total_samples: u64,
}

impl GasPredictor {
    pub fn new(rpc: Arc<MultiRpcProvider>) -> Self {
        const EMPTY: VecDeque<u128> = VecDeque::new();
        Self {
            rpc,
            hourly_prices: [EMPTY; 24],
            recent_prices: VecDeque::with_capacity(500),
            max_per_hour: 200,
            max_recent: 500,
            current_gas: 100_000, // 0.1 gwei default
            total_samples: 0,
        }
    }

    /// Sample current gas price and record it
    pub async fn sample(&mut self) {
        let provider = self.rpc.get();
        match provider.get_gas_price().await {
            Ok(price) => {
                self.current_gas = price;
                self.total_samples += 1;

                // Record in recent window
                if self.recent_prices.len() >= self.max_recent {
                    self.recent_prices.pop_front();
                }
                self.recent_prices.push_back(price);

                // Record in hourly bucket
                let hour = current_hour();
                let bucket = &mut self.hourly_prices[hour];
                if bucket.len() >= self.max_per_hour {
                    bucket.pop_front();
                }
                bucket.push_back(price);
            }
            Err(e) => {
                warn!("Gas sample failed: {}", e);
            }
        }
    }

    /// Get optimal gas price for current conditions.
    /// Returns (base_gas, priority_tip) in wei.
    pub fn optimal_gas(&self) -> (u128, u128) {
        let hour = current_hour();
        let bucket = &self.hourly_prices[hour];

        // Use hourly median if we have enough data, else recent median, else current
        let base = if bucket.len() >= 10 {
            median(bucket)
        } else if self.recent_prices.len() >= 5 {
            median(&self.recent_prices)
        } else {
            self.current_gas
        };

        // Tip: use p75 of recent prices minus median as "competitive" premium
        let tip = if self.recent_prices.len() >= 10 {
            let p75 = percentile(&self.recent_prices, 75);
            p75.saturating_sub(base) / 2 // half the spread as tip
        } else {
            base / 20 // 5% default tip
        };

        (base, tip.max(1_000)) // minimum 1000 wei tip
    }

    /// Get gas price adjusted for competitiveness.
    /// `urgency` 0.0 = cheap (p25), 0.5 = normal (median), 1.0 = aggressive (p90)
    pub fn gas_for_urgency(&self, urgency: f64) -> u128 {
        let pct = (urgency * 90.0).max(10.0).min(99.0) as usize;

        if self.recent_prices.len() >= 5 {
            percentile(&self.recent_prices, pct)
        } else {
            let mult = 0.8 + urgency * 0.4; // 0.8x to 1.2x
            (self.current_gas as f64 * mult) as u128
        }
    }

    /// Check if gas price should be applied to this arb.
    /// Returns adjusted profit after gas cost.
    /// `gas_margin` = multiplier on gas cost as safety margin (e.g., 1.5 = need 50% over gas).
    /// `min_net_wei` = absolute minimum net profit floor.
    pub fn net_profit_after_gas_dynamic(
        &self,
        gross_profit_wei: U256,
        estimated_gas_units: u64,
        gas_margin: f64,
        min_net_wei: u128,
    ) -> (bool, U256) {
        let (base, tip) = self.optimal_gas();
        let gas_price = base + tip;
        let gas_cost = U256::from(gas_price) * U256::from(estimated_gas_units);

        if gross_profit_wei <= gas_cost {
            return (false, U256::ZERO);
        }

        let net = gross_profit_wei - gas_cost;

        // Dynamic margin: scale with gas conditions
        // When gas is cheap, accept tighter margins; when expensive, require more
        let effective_margin = if self.is_gas_cheap() {
            (gas_margin * 0.7).max(1.1) // relax margin in cheap gas
        } else if self.is_gas_expensive() {
            gas_margin * 1.5 // tighten margin in expensive gas
        } else {
            gas_margin
        };

        // Net profit must exceed: gas_cost * (margin - 1) AND absolute floor
        let min_over_gas = U256::from((gas_cost.to::<u128>() as f64 * (effective_margin - 1.0)) as u128);
        let min_floor = U256::from(min_net_wei);

        let threshold = if min_over_gas > min_floor { min_over_gas } else { min_floor };
        (net > threshold, net)
    }

    /// Legacy: fixed 20% margin (backwards compat)
    pub fn net_profit_after_gas(&self, gross_profit_wei: U256, estimated_gas_units: u64) -> (bool, U256) {
        self.net_profit_after_gas_dynamic(gross_profit_wei, estimated_gas_units, 1.2, 0)
    }

    /// Is gas currently "cheap" relative to historical? Good time to execute marginal arbs.
    pub fn is_gas_cheap(&self) -> bool {
        if self.recent_prices.len() < 20 { return true; }
        let med = median(&self.recent_prices);
        self.current_gas < med * 80 / 100 // current is 20%+ below median
    }

    /// Is gas currently "expensive"? Skip marginal arbs.
    pub fn is_gas_expensive(&self) -> bool {
        if self.recent_prices.len() < 20 { return false; }
        let med = median(&self.recent_prices);
        self.current_gas > med * 150 / 100 // current is 50%+ above median
    }

    pub fn current_gas_price(&self) -> u128 {
        self.current_gas
    }

    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    pub fn stats_string(&self) -> String {
        let hour = current_hour();
        let bucket_size = self.hourly_prices[hour].len();
        let (base, tip) = self.optimal_gas();
        format!(
            "gas: current={:.2}gwei optimal={:.2}gwei+{:.2}tip hour={} samples={}/{}",
            self.current_gas as f64 / 1e9,
            base as f64 / 1e9,
            tip as f64 / 1e9,
            hour,
            bucket_size,
            self.total_samples
        )
    }
}

fn current_hour() -> usize {
    let now = chrono::Utc::now();
    now.format("%H").to_string().parse::<usize>().unwrap_or(0)
}

fn median(data: &VecDeque<u128>) -> u128 {
    if data.is_empty() { return 0; }
    let mut sorted: Vec<u128> = data.iter().copied().collect();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn percentile(data: &VecDeque<u128>, pct: usize) -> u128 {
    if data.is_empty() { return 0; }
    let mut sorted: Vec<u128> = data.iter().copied().collect();
    sorted.sort_unstable();
    let idx = (sorted.len() * pct / 100).min(sorted.len() - 1);
    sorted[idx]
}
