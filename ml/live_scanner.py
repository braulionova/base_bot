import os
#!/usr/bin/env python3
"""
Live Longtail Scanner - Finds REAL arb opportunities with verified liquidity.
Checks actual prices on V2/V3 pools and finds profitable spreads.
"""

import json
import subprocess
import time
import sys

CONTRACT = "0x275690F4F52E3023093Cb396E5633f0e3002571F"
PK = os.environ.get("PRIVATE_KEY", "")
WALLET = "0xd69F9856A569B1655B43B0395b7c2923a217Cfe0"
RPCS = [
    "https://mainnet.base.org",
    "https://base.publicnode.com",
    "https://base.drpc.org",
    "https://base-mainnet.public.blastapi.io",
]
rpc_idx = 0
WETH = "0x4200000000000000000000000000000000000006"

def next_rpc():
    global rpc_idx
    rpc = RPCS[rpc_idx % len(RPCS)]
    rpc_idx += 1
    return rpc

def cast(args, timeout=15):
    cmd = ["/root/.foundry/bin/cast"] + args
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return r.stdout.strip(), r.stderr.strip()
    except:
        return "", "timeout"

def get_v2_reserves(pair, rpc):
    """Get reserves from V2 pair."""
    out, err = cast(["call", pair, "getReserves()(uint112,uint112,uint32)", "--rpc-url", rpc])
    if err or not out:
        return None, None
    lines = out.strip().split("\n")
    if len(lines) >= 2:
        try:
            r0 = int(lines[0])
            r1 = int(lines[1])
            return r0, r1
        except:
            return None, None
    return None, None

def get_v2_token0(pair, rpc):
    out, _ = cast(["call", pair, "token0()(address)", "--rpc-url", rpc])
    return out.strip().lower() if out else None

def get_v3_sqrt_price(pool, rpc):
    """Get sqrtPriceX96 from V3 pool slot0."""
    out, err = cast(["call", pool, "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)", "--rpc-url", rpc])
    if err or not out:
        return None
    lines = out.strip().split("\n")
    if lines:
        try:
            return int(lines[0])
        except:
            return None
    return None

def get_v3_liquidity(pool, rpc):
    out, _ = cast(["call", pool, "liquidity()(uint128)", "--rpc-url", rpc])
    try:
        return int(out.strip()) if out else 0
    except:
        return 0

def v2_price(r0, r1, t0_is_weth):
    """Price of token1 in terms of token0."""
    if r0 == 0 or r1 == 0:
        return 0
    if t0_is_weth:
        return r0 / r1  # WETH per token
    else:
        return r1 / r0

def v3_price(sqrt_price_x96, t0_is_weth):
    """Price from sqrtPriceX96."""
    if not sqrt_price_x96 or sqrt_price_x96 == 0:
        return 0
    price = (sqrt_price_x96 / (2**96)) ** 2
    if t0_is_weth:
        return price  # token1 per token0, i.e., WETH per longtail
    else:
        return 1.0 / price if price > 0 else 0

def simulate_exec(pool_a, pool_b, token_in, token_out, amount_wei, pool_b_v3, rpc):
    """Simulate the flash arb."""
    sig = "exec(address,address,address,address,uint256,bool)"
    args = [pool_a, pool_b, token_in, token_out, str(amount_wei), str(pool_b_v3).lower()]
    out, err = cast(["call", CONTRACT, sig] + args + ["--from", WALLET, "--rpc-url", rpc])
    if err and ("revert" in err.lower() or "error" in err.lower()):
        return False, err[:150]
    return True, "OK"

def execute_tx(pool_a, pool_b, token_in, token_out, amount_wei, pool_b_v3, rpc):
    """Send actual transaction."""
    sig = "exec(address,address,address,address,uint256,bool)"
    args = [pool_a, pool_b, token_in, token_out, str(amount_wei), str(pool_b_v3).lower()]
    out, err = cast(["send", CONTRACT, sig] + args + [
        "--private-key", PK, "--rpc-url", rpc, "--gas-limit", "500000"
    ], timeout=30)
    return out, err


def main():
    print("=" * 60)
    print("LIVE LONGTAIL SCANNER + EXECUTOR")
    print("=" * 60)

    # Load pools
    with open("/root/arb-flash-bot/pools_cache.json") as f:
        pools = json.load(f)
    print(f"Loaded {len(pools)} pools")

    # Index by token pair
    from collections import defaultdict
    pair_pools = defaultdict(list)
    for p in pools:
        t0 = p["token0"].lower()
        t1 = p["token1"].lower()
        key = (min(t0, t1), max(t0, t1))
        pair_pools[key].append(p)

    # Find WETH pairs with 2+ pools on different DEXes
    weth = WETH.lower()
    candidates = []
    for (t0, t1), plist in pair_pools.items():
        if t0 != weth and t1 != weth:
            continue
        dexes = set(p["dex_name"] for p in plist)
        if len(dexes) < 2:
            continue
        longtail = t1 if t0 == weth else t0
        candidates.append((longtail, plist))

    print(f"WETH pairs with cross-DEX pools: {len(candidates)}")

    # Check each candidate for liquidity and price spread
    opportunities = []
    checked = 0

    for longtail, plist in candidates:
        checked += 1
        if checked % 5 == 0:
            print(f"Checking {checked}/{len(candidates)}...")

        rpc = next_rpc()
        pool_prices = []

        for p in plist:
            addr = p["address"]
            t0_is_weth = p["token0"].lower() == weth
            ptype = p["pool_type"]

            if ptype == "V2":
                r0, r1 = get_v2_reserves(addr, rpc)
                if r0 is None or r0 == 0 or r1 == 0:
                    continue
                # Check minimum liquidity (at least 0.0001 ETH worth)
                weth_reserve = r0 if t0_is_weth else r1
                if weth_reserve < 100000000000000:  # 0.0001 ETH
                    continue
                price = v2_price(r0, r1, t0_is_weth)
                pool_prices.append({
                    "pool": addr, "dex": p["dex_name"], "type": "V2",
                    "price": price, "liquidity_eth": weth_reserve / 1e18,
                    "t0_is_weth": t0_is_weth
                })
            else:
                sqp = get_v3_sqrt_price(addr, rpc)
                liq = get_v3_liquidity(addr, rpc)
                if not sqp or liq == 0:
                    continue
                price = v3_price(sqp, t0_is_weth)
                pool_prices.append({
                    "pool": addr, "dex": p["dex_name"], "type": "V3",
                    "price": price, "liquidity": liq,
                    "t0_is_weth": t0_is_weth
                })

            time.sleep(0.1)  # Rate limit

        if len(pool_prices) < 2:
            continue

        # Find price spreads
        for i, pa in enumerate(pool_prices):
            for pb in pool_prices[i+1:]:
                if pa["dex"] == pb["dex"]:
                    continue
                if pa["price"] == 0 or pb["price"] == 0:
                    continue

                spread = abs(pa["price"] - pb["price"]) / min(pa["price"], pb["price"])

                if spread > 0.005:  # > 0.5% spread
                    buy_pool = pa if pa["price"] < pb["price"] else pb
                    sell_pool = pb if pa["price"] < pb["price"] else pa

                    opportunities.append({
                        "longtail": longtail,
                        "spread": spread,
                        "buy": buy_pool,
                        "sell": sell_pool,
                    })
                    print(f"  SPREAD {spread*100:.2f}% | buy@{buy_pool['dex']}({buy_pool['type']}) "
                          f"sell@{sell_pool['dex']}({sell_pool['type']}) | token={longtail[:10]}...")

    print(f"\n=== Found {len(opportunities)} opportunities ===")

    if not opportunities:
        print("No profitable spreads found. This is normal - arbs are fleeting.")
        print("Run this script in a loop to catch them when they appear.")
        return

    # Sort by spread (highest first)
    opportunities.sort(key=lambda x: -x["spread"])

    # Try to execute top opportunities
    for opp in opportunities[:10]:
        buy = opp["buy"]
        sell = opp["sell"]
        spread = opp["spread"]

        # V3 pool must be poolA (flash source)
        if buy["type"] == "V3":
            pool_a, pool_b = buy["pool"], sell["pool"]
            pool_b_v3 = sell["type"] == "V3"
        elif sell["type"] == "V3":
            pool_a, pool_b = sell["pool"], buy["pool"]
            pool_b_v3 = buy["type"] == "V3"
        else:
            print(f"  Skip: both V2")
            continue

        rpc = next_rpc()

        # Try small amounts
        for amt_name, amt_wei in [("0.001", 1000000000000000), ("0.0005", 500000000000000)]:
            print(f"\n  Simulating {amt_name} ETH | spread {spread*100:.2f}% | {buy['dex']}->{sell['dex']}...")

            ok, msg = simulate_exec(pool_a, pool_b, WETH, opp["longtail"],
                                    amt_wei, pool_b_v3, rpc)

            if ok:
                print(f"  SIM PASSED! Executing...")
                out, err = execute_tx(pool_a, pool_b, WETH, opp["longtail"],
                                      amt_wei, pool_b_v3, rpc)
                if out and "status" in out:
                    print(f"  TX RESULT: {out[:300]}")
                    if "true" in out or "0x1" in out:
                        print(f"\n  === SUCCESSFUL LONGTAIL ARB! ===")
                        print(f"  Spread: {spread*100:.2f}%")
                        print(f"  Amount: {amt_name} ETH")
                        return
                else:
                    print(f"  TX err: {(err or out)[:200]}")
                break
            else:
                print(f"  Reverted: {msg[:100]}")

    print("\nNo executable arb right now. Re-run to catch the next one.")


if __name__ == "__main__":
    main()
