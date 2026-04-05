use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

/// ML-inspired token blacklist that learns from reverts and honeypots.
/// Tracks features per token and auto-blacklists based on failure patterns.
#[derive(Default, Serialize, Deserialize)]
pub struct TokenBlacklist {
    /// Per-token scoring data
    tokens: HashMap<Address, TokenScore>,
    /// Blacklist threshold — tokens scoring above this are blocked
    threshold: f64,
    /// Path for persistence
    #[serde(skip)]
    save_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenScore {
    /// Total number of arb attempts involving this token
    pub attempts: u32,
    /// Number of reverts (simulation or execution failures)
    pub reverts: u32,
    /// Number of successful executions
    pub successes: u32,
    /// Whether the token has been manually whitelisted
    pub whitelisted: bool,
    /// Whether the token is hard-blacklisted (known scam)
    pub hard_blacklisted: bool,
    /// Revert rate (learned feature)
    pub revert_rate: f64,
    /// Average gas wasted on failed attempts
    pub avg_gas_wasted: f64,
    /// Last block seen
    pub last_seen_block: u64,
    /// Consecutive reverts (streak)
    pub consecutive_reverts: u32,
}

impl Default for TokenScore {
    fn default() -> Self {
        Self {
            attempts: 0,
            reverts: 0,
            successes: 0,
            whitelisted: false,
            hard_blacklisted: false,
            revert_rate: 0.0,
            avg_gas_wasted: 0.0,
            last_seen_block: 0,
            consecutive_reverts: 0,
        }
    }
}

impl TokenBlacklist {
    pub fn new(save_path: &str) -> Self {
        Self {
            tokens: HashMap::new(),
            threshold: 0.7, // 70% revert rate = blacklisted
            save_path: save_path.to_string(),
        }
    }

    /// Load from disk
    pub fn load(path: &str) -> Self {
        let p = Path::new(path);
        if p.exists() {
            match std::fs::read_to_string(p) {
                Ok(data) => {
                    match serde_json::from_str::<TokenBlacklist>(&data) {
                        Ok(mut bl) => {
                            bl.save_path = path.to_string();
                            info!("Loaded blacklist: {} tokens tracked", bl.tokens.len());
                            return bl;
                        }
                        Err(e) => warn!("Failed to parse blacklist: {}", e),
                    }
                }
                Err(e) => warn!("Failed to read blacklist: {}", e),
            }
        }
        Self::new(path)
    }

    /// Save to disk
    pub fn save(&self) {
        if self.save_path.is_empty() { return; }
        match serde_json::to_string_pretty(self) {
            Ok(data) => {
                if let Err(e) = std::fs::write(&self.save_path, data) {
                    warn!("Failed to save blacklist: {}", e);
                }
            }
            Err(e) => warn!("Failed to serialize blacklist: {}", e),
        }
    }

    /// Check if a token is blacklisted
    pub fn is_blacklisted(&self, token: &Address) -> bool {
        if let Some(score) = self.tokens.get(token) {
            if score.whitelisted { return false; }
            if score.hard_blacklisted { return true; }

            // ML scoring: blacklist if revert rate exceeds threshold with enough samples
            if score.attempts >= 3 && score.revert_rate > self.threshold {
                return true;
            }

            // Fast blacklist: 5+ consecutive reverts
            if score.consecutive_reverts >= 5 {
                return true;
            }
        }
        false
    }

    /// Check if both tokens in a pair are safe
    pub fn is_pair_safe(&self, token0: &Address, token1: &Address) -> bool {
        !self.is_blacklisted(token0) && !self.is_blacklisted(token1)
    }

    /// Record a simulation/execution revert for a token
    pub fn record_revert(&mut self, token: Address, gas_wasted: u64, block: u64) {
        let score = self.tokens.entry(token).or_default();
        score.attempts += 1;
        score.reverts += 1;
        score.consecutive_reverts += 1;
        score.last_seen_block = block;

        // Update running average gas wasted
        let total_gas = score.avg_gas_wasted * (score.reverts - 1) as f64 + gas_wasted as f64;
        score.avg_gas_wasted = total_gas / score.reverts as f64;

        // Update revert rate
        score.revert_rate = score.reverts as f64 / score.attempts as f64;
    }

    /// Record a successful execution for a token
    pub fn record_success(&mut self, token: Address, block: u64) {
        let score = self.tokens.entry(token).or_default();
        score.attempts += 1;
        score.successes += 1;
        score.consecutive_reverts = 0; // reset streak
        score.last_seen_block = block;

        // Update revert rate
        score.revert_rate = score.reverts as f64 / score.attempts as f64;
    }

    /// Manually blacklist a token (e.g., known honeypot)
    pub fn hard_blacklist(&mut self, token: Address) {
        let score = self.tokens.entry(token).or_default();
        score.hard_blacklisted = true;
    }

    /// Manually whitelist a token (override ML scoring)
    pub fn whitelist(&mut self, token: Address) {
        let score = self.tokens.entry(token).or_default();
        score.whitelisted = true;
    }

    /// Get tokens sorted by danger score (worst first)
    pub fn worst_tokens(&self, limit: usize) -> Vec<(Address, &TokenScore)> {
        let mut sorted: Vec<_> = self.tokens.iter()
            .filter(|(_, s)| s.attempts >= 2)
            .map(|(a, s)| (*a, s))
            .collect();
        sorted.sort_by(|a, b| b.1.revert_rate.partial_cmp(&a.1.revert_rate).unwrap_or(std::cmp::Ordering::Equal));
        sorted.truncate(limit);
        sorted
    }

    /// Get summary stats
    pub fn stats(&self) -> (usize, usize, usize) {
        let total = self.tokens.len();
        let blacklisted = self.tokens.iter()
            .filter(|(addr, _)| self.is_blacklisted(addr))
            .count();
        let whitelisted = self.tokens.iter()
            .filter(|(_, s)| s.whitelisted)
            .count();
        (total, blacklisted, whitelisted)
    }

    pub fn stats_string(&self) -> String {
        let (total, blacklisted, whitelisted) = self.stats();
        format!(
            "blacklist: {}/{} blacklisted, {} whitelisted",
            blacklisted, total, whitelisted
        )
    }
}
