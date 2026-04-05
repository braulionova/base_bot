use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use std::sync::Arc;
use tracing::warn;

use crate::rpc::MultiRpcProvider;

// Multicall3 on Base (same address on all EVM chains)
pub const MULTICALL3: Address = alloy::primitives::address!("cA11bde05977b3631167028862bE2a173976CA11");

sol! {
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct McResult {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (McResult[] memory returnData);
    }

    interface IReservesV2 {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }

    interface ISlot0V3 {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
    }
}

#[derive(Debug, Clone)]
pub struct V2Reserves {
    pub reserve0: U256,
    pub reserve1: U256,
}

#[derive(Debug, Clone)]
pub struct V3State {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
}

/// Batch fetch V2 reserves for multiple pools in a single RPC call.
pub async fn batch_v2_reserves(
    rpc: &Arc<MultiRpcProvider>,
    pools: &[Address],
) -> Vec<Option<V2Reserves>> {
    if pools.is_empty() {
        return vec![];
    }

    let provider = rpc.get();
    let calldata = IReservesV2::getReservesCall {}.abi_encode();

    let mut all_results: Vec<Option<V2Reserves>> = Vec::with_capacity(pools.len());

    for chunk in pools.chunks(200) {
        let calls: Vec<IMulticall3::Call3> = chunk.iter().map(|addr| {
            IMulticall3::Call3 {
                target: *addr,
                allowFailure: true,
                callData: Bytes::from(calldata.clone()),
            }
        }).collect();

        let mc = IMulticall3::new(MULTICALL3, provider);
        match mc.aggregate3(calls).call().await {
            Ok(ret) => {
                // aggregate3 returns Vec<McResult> directly (single return val)
                for res in ret.iter() {
                    if res.success && res.returnData.len() >= 64 {
                        if let Ok(decoded) = IReservesV2::getReservesCall::abi_decode_returns(&res.returnData) {
                            all_results.push(Some(V2Reserves {
                                reserve0: U256::from(decoded.reserve0),
                                reserve1: U256::from(decoded.reserve1),
                            }));
                            continue;
                        }
                    }
                    all_results.push(None);
                }
            }
            Err(e) => {
                warn!("Multicall V2 reserves failed: {}", e);
                all_results.extend(chunk.iter().map(|_| None));
            }
        }
    }

    all_results
}

/// Batch fetch V3 state (slot0 + liquidity) for multiple pools.
pub async fn batch_v3_state(
    rpc: &Arc<MultiRpcProvider>,
    pools: &[Address],
) -> Vec<Option<V3State>> {
    if pools.is_empty() {
        return vec![];
    }

    let provider = rpc.get();
    let slot0_cd = ISlot0V3::slot0Call {}.abi_encode();
    let liq_cd = ISlot0V3::liquidityCall {}.abi_encode();

    let mut all_results: Vec<Option<V3State>> = Vec::with_capacity(pools.len());

    for chunk in pools.chunks(100) {
        let mut calls: Vec<IMulticall3::Call3> = Vec::with_capacity(chunk.len() * 2);
        for addr in chunk {
            calls.push(IMulticall3::Call3 {
                target: *addr,
                allowFailure: true,
                callData: Bytes::from(slot0_cd.clone()),
            });
            calls.push(IMulticall3::Call3 {
                target: *addr,
                allowFailure: true,
                callData: Bytes::from(liq_cd.clone()),
            });
        }

        let mc = IMulticall3::new(MULTICALL3, provider);
        match mc.aggregate3(calls).call().await {
            Ok(ret) => {
                let results = &ret;
                for i in (0..results.len()).step_by(2) {
                    if i + 1 < results.len() && results[i].success && results[i + 1].success {
                        let slot0_ok = ISlot0V3::slot0Call::abi_decode_returns(&results[i].returnData);
                        let liq_ok = ISlot0V3::liquidityCall::abi_decode_returns(&results[i + 1].returnData);
                        if let (Ok(s), Ok(l)) = (slot0_ok, liq_ok) {
                            all_results.push(Some(V3State {
                                sqrt_price_x96: U256::from(s.sqrtPriceX96),
                                liquidity: l,
                            }));
                            continue;
                        }
                    }
                    all_results.push(None);
                }
            }
            Err(e) => {
                warn!("Multicall V3 state failed: {}", e);
                all_results.extend(chunk.iter().map(|_| None));
            }
        }
    }

    all_results
}

/// Batch safety check: fetch totalSupply + decimals for tokens
pub async fn batch_token_info(
    rpc: &Arc<MultiRpcProvider>,
    tokens: &[Address],
) -> Vec<Option<(U256, u8)>> {
    if tokens.is_empty() {
        return vec![];
    }

    sol! {
        interface IERC20Check {
            function totalSupply() external view returns (uint256);
            function decimals() external view returns (uint8);
        }
    }

    let provider = rpc.get();
    let supply_cd = IERC20Check::totalSupplyCall {}.abi_encode();
    let dec_cd = IERC20Check::decimalsCall {}.abi_encode();

    let mut all_results: Vec<Option<(U256, u8)>> = Vec::with_capacity(tokens.len());

    for chunk in tokens.chunks(150) {
        let mut calls: Vec<IMulticall3::Call3> = Vec::with_capacity(chunk.len() * 2);
        for addr in chunk {
            calls.push(IMulticall3::Call3 {
                target: *addr,
                allowFailure: true,
                callData: Bytes::from(supply_cd.clone()),
            });
            calls.push(IMulticall3::Call3 {
                target: *addr,
                allowFailure: true,
                callData: Bytes::from(dec_cd.clone()),
            });
        }

        let mc = IMulticall3::new(MULTICALL3, provider);
        match mc.aggregate3(calls).call().await {
            Ok(ret) => {
                let results = &ret;
                for i in (0..results.len()).step_by(2) {
                    if i + 1 < results.len() && results[i].success {
                        let supply_ok = IERC20Check::totalSupplyCall::abi_decode_returns(&results[i].returnData);
                        let dec: u8 = if results[i + 1].success {
                            IERC20Check::decimalsCall::abi_decode_returns(&results[i + 1].returnData)
                                .unwrap_or(18)
                        } else {
                            18
                        };
                        if let Ok(supply) = supply_ok {
                            all_results.push(Some((supply, dec)));
                            continue;
                        }
                    }
                    all_results.push(None);
                }
            }
            Err(e) => {
                warn!("Multicall token info failed: {}", e);
                all_results.extend(chunk.iter().map(|_| None));
            }
        }
    }

    all_results
}
