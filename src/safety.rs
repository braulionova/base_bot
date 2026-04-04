use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::sol;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::debug;

use crate::rpc::MultiRpcProvider;

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function totalSupply() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
        function decimals() external view returns (uint8);
    }
}

#[derive(Debug, Clone)]
pub struct TokenSafetyResult {
    pub token: Address,
    pub is_safe: bool,
    pub is_honeypot: bool,
    pub reason: String,
}

pub struct SafetyChecker {
    rpc: Arc<MultiRpcProvider>,
    known_safe: HashSet<Address>,
    known_unsafe: HashSet<Address>,
}

impl SafetyChecker {
    pub fn new(rpc: Arc<MultiRpcProvider>) -> Self {
        let mut known_safe = HashSet::new();
        let safe_tokens = [
            "0x4200000000000000000000000000000000000006", // WETH
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", // USDC
            "0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb", // DAI
            "0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA", // USDbC
            "0x2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22", // cbETH
            "0xc1CBa3fCea344f92D9239c08C0568f6F2F0ee452", // wstETH
            "0x940181a94A35A4569E4529A3CDfB74e38FD98631", // AERO
            "0x236aa50979D5f3De3Bd1Eeb40E81137F22ab794b", // tBTC
            "0xB6fe221Fe9EeF5aBa221c348bA20A1Bf5e73624c", // rETH
            "0x04C0599Ae5A44757c0af6F9eC3b93da8976c150A", // weETH
            "0xDBFeFD2e8460a6Ee4955A68582F85708BAEA60A3", // BRETT
            "0x532f27101965dd16442E59d40670FaF5eBB142E4", // TOSHI
        ];
        for t in safe_tokens {
            known_safe.insert(t.parse().unwrap());
        }

        Self {
            rpc,
            known_safe,
            known_unsafe: HashSet::new(),
        }
    }

    pub async fn check_token(&mut self, token: Address) -> TokenSafetyResult {
        if self.known_safe.contains(&token) {
            return TokenSafetyResult {
                token,
                is_safe: true,
                is_honeypot: false,
                reason: "known safe token".into(),
            };
        }

        if self.known_unsafe.contains(&token) {
            return TokenSafetyResult {
                token,
                is_safe: false,
                is_honeypot: true,
                reason: "known unsafe token".into(),
            };
        }

        let provider = self.rpc.get();
        let contract = IERC20::new(token, provider);

        // Check totalSupply
        let total_supply = match contract.totalSupply().call().await {
            Ok(s) => s,
            Err(_) => {
                self.known_unsafe.insert(token);
                return TokenSafetyResult {
                    token,
                    is_safe: false,
                    is_honeypot: true,
                    reason: "totalSupply() call failed".into(),
                };
            }
        };

        if total_supply.is_zero() {
            self.known_unsafe.insert(token);
            return TokenSafetyResult {
                token,
                is_safe: false,
                is_honeypot: true,
                reason: "zero total supply".into(),
            };
        }

        // Check decimals
        let decimals = match contract.decimals().call().await {
            Ok(d) => d,
            Err(_) => 18,
        };

        if decimals > 24 {
            self.known_unsafe.insert(token);
            return TokenSafetyResult {
                token,
                is_safe: false,
                is_honeypot: true,
                reason: format!("suspicious decimals: {}", decimals),
            };
        }

        // Check code size
        match provider.get_code_at(token).await {
            Ok(code) => {
                if code.len() < 50 {
                    self.known_unsafe.insert(token);
                    return TokenSafetyResult {
                        token,
                        is_safe: false,
                        is_honeypot: true,
                        reason: "code too small or no code".into(),
                    };
                }
            }
            Err(_) => {}
        }

        self.known_safe.insert(token);
        TokenSafetyResult {
            token,
            is_safe: true,
            is_honeypot: false,
            reason: "passed basic safety checks".into(),
        }
    }

    pub async fn check_pool_tokens(&mut self, token0: Address, token1: Address) -> bool {
        let r0 = self.check_token(token0).await;
        if !r0.is_safe {
            debug!("Token {} unsafe: {}", token0, r0.reason);
            return false;
        }
        let r1 = self.check_token(token1).await;
        if !r1.is_safe {
            debug!("Token {} unsafe: {}", token1, r1.reason);
            return false;
        }
        true
    }
}
