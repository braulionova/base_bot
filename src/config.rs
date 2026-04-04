use alloy::primitives::Address;
use std::str::FromStr;

pub struct Config {
    pub wallet_address: Address,
    pub rpc_urls: Vec<String>,
    pub chain_id: u64,
    pub min_profit_wei: u128,
    pub max_trade_size_eth: f64,
    pub poll_interval_ms: u64,
    pub max_pools: usize,
    pub competition_threshold: u64,
    pub pool_cache_path: String,
}

impl Config {
    pub fn base_mainnet() -> Self {
        Self {
            wallet_address: Address::from_str("0xd69f9856a569b1655b43b0395b7c2923a217cfe0").unwrap(),
            rpc_urls: vec![
                "http://localhost:8545".into(),       // Local node (10ms) - first priority
                "https://mainnet.base.org".into(),
                "https://base.meowrpc.com".into(),
                "https://base.publicnode.com".into(),
                "https://base.drpc.org".into(),
                "https://base-mainnet.public.blastapi.io".into(),
                "https://base-rpc.publicnode.com".into(),
            ],
            chain_id: 8453,
            min_profit_wei: 20_000_000_000_000, // 0.00002 ETH = ~$0.05 target
            max_trade_size_eth: 0.1,
            poll_interval_ms: 250, // Faster polling for arb speed
            max_pools: 50_000,
            competition_threshold: 3,
            pool_cache_path: "pools_cache.json".into(),
        }
    }
}

pub mod dex {
    use alloy::primitives::{address, Address};

    #[derive(Clone)]
    pub struct DexFactory {
        pub name: &'static str,
        pub factory: Address,
        pub pool_type: PoolType,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum PoolType {
        UniswapV2,
        UniswapV3,
    }

    pub fn factories() -> Vec<DexFactory> {
        vec![
            // ===== V3 / Concentrated Liquidity =====
            DexFactory {
                name: "UniswapV3",
                factory: address!("33128a8fC17869897dcE68Ed026d694621f6FDfD"),
                pool_type: PoolType::UniswapV3,
            },
            DexFactory {
                name: "Aerodrome_CL",
                factory: address!("5e7BB104d84c7CB9B682AaC2F3d509f5F406809A"),
                pool_type: PoolType::UniswapV3,
            },
            DexFactory {
                name: "Aerodrome_SlipStream",
                factory: address!("eC8E5342B19977B4eF8892e02D8DAEcfa1315831"),
                pool_type: PoolType::UniswapV3,
            },
            DexFactory {
                name: "SushiSwapV3",
                factory: address!("c35DADB65012eC5796536bD9864eD8773aBc74C4"),
                pool_type: PoolType::UniswapV3,
            },
            DexFactory {
                name: "BaseSwapV3",
                factory: address!("38015D05f4fEC8AFe15D7cc0386a126574e8077B"),
                pool_type: PoolType::UniswapV3,
            },
            DexFactory {
                name: "PancakeSwapV3",
                factory: address!("0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865"),
                pool_type: PoolType::UniswapV3,
            },
            DexFactory {
                name: "AlienBaseV3",
                factory: address!("0Fd83557b2be93617c9C1C1B6fd549401C74558C"),
                pool_type: PoolType::UniswapV3,
            },
            DexFactory {
                name: "DackieSwapV3",
                factory: address!("3D237AC6D2f425D2E890Cc99198818cc1FA48870"),
                pool_type: PoolType::UniswapV3,
            },
            DexFactory {
                name: "MaverickV2",
                factory: address!("0A7e848Aca42d879EF06507Fca0E7b33A0a63c1e"),
                pool_type: PoolType::UniswapV3,
            },
            DexFactory {
                name: "HorizonDEX",
                factory: address!("9Fe607e5dCd0Ea318dBB4D8a7B04fa553d6cB2c5"),
                pool_type: PoolType::UniswapV3,
            },
            // ===== V2 / AMM =====
            DexFactory {
                name: "UniswapV2",
                factory: address!("8909Dc15e40173Ff4699343b6eB8132c65e18eC6"),
                pool_type: PoolType::UniswapV2,
            },
            DexFactory {
                name: "Aerodrome_V2",
                factory: address!("420DD381b31aEf6683db6B902084cB0FFECe40Da"),
                pool_type: PoolType::UniswapV2,
            },
            DexFactory {
                name: "BaseSwapV2",
                factory: address!("FDa619b6d20975be80A10332cD39b9a4b0FAa8BB"),
                pool_type: PoolType::UniswapV2,
            },
            DexFactory {
                name: "SwapBasedV2",
                factory: address!("04C9f118d21e8B767D2e50C946f0cC9F6C367300"),
                pool_type: PoolType::UniswapV2,
            },
            DexFactory {
                name: "AlienBaseV2",
                factory: address!("3E84D913803b02A4A7f027165E8cA42C14C0FdE7"),
                pool_type: PoolType::UniswapV2,
            },
            DexFactory {
                name: "DackieSwapV2",
                factory: address!("591f122D1df761E616c13d265006fcbf4c6d6551"),
                pool_type: PoolType::UniswapV2,
            },
            DexFactory {
                name: "SynthswapV2",
                factory: address!("4bd16d59A5E1E0DB903F724aa9d721a31d7D720D"),
                pool_type: PoolType::UniswapV2,
            },
            DexFactory {
                name: "Equalizer",
                factory: address!("eD8db60aCc29e14bc867a497D94ca6e3CeB5eC04"),
                pool_type: PoolType::UniswapV2,
            },
        ]
    }
}
