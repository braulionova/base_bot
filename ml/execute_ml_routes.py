#!/usr/bin/env python3
"""
Execute ML-prioritized longtail routes via flash swap contract.
Tests each route with eth_call simulation, executes profitable ones.
"""

import json
import subprocess
import sys
import time

CONTRACT = "0x275690F4F52E3023093Cb396E5633f0e3002571F"
PK = "0xfb7d62cfba588e53df82089cb9ad1b99397b8718e821b23585f6608c01d2de61"
WALLET = "0xd69F9856A569B1655B43B0395b7c2923a217Cfe0"
RPC = "https://mainnet.base.org"
ROUTES_FILE = "/root/arb-flash-bot/prioritized_routes.json"

# Trade sizes to test (smallest first for longtail)
TRADE_SIZES = [
    ("0.001", "1000000000000000"),     # 0.001 ETH
    ("0.002", "2000000000000000"),     # 0.002 ETH
    ("0.005", "5000000000000000"),     # 0.005 ETH
    ("0.01",  "10000000000000000"),    # 0.01 ETH
]

def cast_cmd(args):
    """Run a cast command and return output."""
    cmd = ["/root/.foundry/bin/cast"] + args
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        return result.stdout.strip(), result.stderr.strip()
    except Exception as e:
        return "", str(e)


def simulate_route(route, amount_wei):
    """Simulate a flash arb route using eth_call."""
    pool_a = route["pool_a"]
    pool_b = route["pool_b"]
    token_in = route["token_in"]
    token_out = route["token_out"]
    is_v3_v2 = route["is_v3_v2_mix"]

    # Determine which pool is V3 (must be poolA for flash swap)
    # If dex_a contains "V3", poolA is V3 (good for flash)
    # If dex_a is V2, swap the order: use pool_b as poolA
    dex_a = route["dex_a"]
    dex_b = route["dex_b"]

    if "V2" in dex_a and "V3" in dex_b:
        # Swap: V3 pool should be poolA (flash source)
        pool_a, pool_b = pool_b, pool_a
        dex_a, dex_b = dex_b, dex_a
        pool_b_is_v3 = False
    elif "V3" in dex_a and "V2" in dex_b:
        pool_b_is_v3 = False
    elif "V3" in dex_a and "V3" in dex_b:
        pool_b_is_v3 = True
    else:
        # Both V2 - can't flash swap. Skip.
        return None, "both V2, can't flash"

    # Build calldata: exec(poolA, poolB, tokenIn, tokenOut, amountIn, poolBisV3)
    sig = "exec(address,address,address,address,uint256,bool)"
    args = [pool_a, pool_b, token_in, token_out, amount_wei, str(pool_b_is_v3).lower()]

    # Simulate with eth_call
    out, err = cast_cmd([
        "call", CONTRACT, sig] + args + [
        "--from", WALLET,
        "--rpc-url", RPC
    ])

    if err and ("revert" in err.lower() or "error" in err.lower() or "execution" in err.lower()):
        # Extract revert reason
        reason = err[:200]
        return False, reason
    elif out or (not err):
        return True, "simulation passed"
    else:
        return None, err[:200]


def execute_route(route, amount_wei, pool_a, pool_b, pool_b_is_v3):
    """Send actual transaction to execute the arb."""
    sig = "exec(address,address,address,address,uint256,bool)"
    args = [pool_a, pool_b, route["token_in"], route["token_out"],
            amount_wei, str(pool_b_is_v3).lower()]

    out, err = cast_cmd([
        "send", CONTRACT, sig] + args + [
        "--private-key", PK,
        "--rpc-url", RPC,
        "--gas-limit", "500000",
    ])

    return out, err


def main():
    print("=" * 60)
    print("FLASH ARB EXECUTOR - ML Prioritized Routes")
    print(f"Contract: {CONTRACT}")
    print(f"Routes file: {ROUTES_FILE}")
    print("=" * 60)

    # Check balance
    bal, _ = cast_cmd(["balance", WALLET, "--rpc-url", RPC, "--ether"])
    print(f"Wallet balance: {bal} ETH")

    with open(ROUTES_FILE) as f:
        routes = json.load(f)
    print(f"Routes to test: {len(routes)}")

    tested = 0
    simulated_ok = 0

    for i, route in enumerate(routes):
        token_short = route["token_out"][:10] + "..."
        dex_pair = f"{route['dex_a']}->{route['dex_b']}"

        for size_name, size_wei in TRADE_SIZES:
            tested += 1
            print(f"\n[{i+1}/{len(routes)}] {dex_pair} | {token_short} | {size_name} ETH")

            success, msg = simulate_route(route, size_wei)

            if success is True:
                simulated_ok += 1
                print(f"  SIM OK! Attempting execution...")

                # Determine pool order (V3 first)
                dex_a = route["dex_a"]
                dex_b = route["dex_b"]
                pool_a = route["pool_a"]
                pool_b = route["pool_b"]
                pool_b_is_v3 = False

                if "V2" in dex_a and "V3" in dex_b:
                    pool_a, pool_b = pool_b, pool_a
                    pool_b_is_v3 = False
                elif "V3" in dex_a and "V3" in dex_b:
                    pool_b_is_v3 = True

                out, err = execute_route(route, size_wei, pool_a, pool_b, pool_b_is_v3)
                if "status" in out and "1" in out:
                    print(f"  TX SUCCESS!")
                    print(out[:500])
                    return  # First successful trade!
                elif err:
                    print(f"  TX failed: {err[:200]}")
                else:
                    print(f"  TX result: {out[:200]}")

                # Don't try larger sizes if smallest failed on-chain
                break

            elif success is False:
                reason = msg
                if "no profit" in reason or "profit" in reason:
                    print(f"  No profit at {size_name} ETH")
                    continue  # Try larger size
                elif "0" in reason or "revert" in reason:
                    print(f"  Revert: {reason[:100]}")
                    break  # This route is dead, skip
                else:
                    print(f"  Failed: {reason[:100]}")
                    break
            else:
                print(f"  Error: {msg[:100]}")
                break

            time.sleep(0.3)  # Rate limit

        time.sleep(0.5)

    print(f"\n=== Summary ===")
    print(f"Routes tested: {tested}")
    print(f"Simulations passed: {simulated_ok}")


if __name__ == "__main__":
    main()
