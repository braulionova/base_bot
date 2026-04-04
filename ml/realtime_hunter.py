import os
#!/usr/bin/env python3
"""
Realtime Longtail Hunter
========================
1. Watch recent blocks for swaps on longtail pools
2. When a swap creates a price deviation vs another pool of same pair -> ARB
3. Also checks intra-DEX arbs (same token, different fee tiers on V3)
4. Executes via flash swap contract

This is the pattern real longtail bots use.
"""

import json
import subprocess
import time
from collections import defaultdict

CONTRACT = "0x275690F4F52E3023093Cb396E5633f0e3002571F"
PK = os.environ.get("PRIVATE_KEY", "")
WALLET = "0xd69F9856A569B1655B43B0395b7c2923a217Cfe0"
WETH = "0x4200000000000000000000000000000000000006"

RPCS = [
    "http://localhost:8545",              # Local node (10ms) - first priority
    "https://mainnet.base.org",
    "https://base.publicnode.com",
    "https://base-mainnet.public.blastapi.io",
]
rpc_idx = 0

V3_SWAP = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
V2_SWAP = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"

def rpc():
    global rpc_idx
    r = RPCS[rpc_idx % len(RPCS)]
    rpc_idx += 1
    return r

def cast(args, timeout=15):
    cmd = ["/root/.foundry/bin/cast"] + args
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return r.stdout.strip(), r.stderr.strip()
    except:
        return "", "timeout"

def get_block():
    out, _ = cast(["block-number", "--rpc-url", rpc()])
    return int(out) if out else 0

def get_logs(from_block, to_block, topics):
    """Get swap logs from a block range."""
    import urllib.request
    payload = json.dumps({
        "jsonrpc": "2.0",
        "method": "eth_getLogs",
        "params": [{
            "fromBlock": hex(from_block),
            "toBlock": hex(to_block),
            "topics": [topics]
        }],
        "id": 1
    }).encode()

    r = rpc()
    req = urllib.request.Request(r, data=payload,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read())
            return data.get("result", [])
    except:
        return []

def load_pool_index():
    """Build token pair -> pool index from cache."""
    with open("/root/arb-flash-bot/pools_cache.json") as f:
        pools = json.load(f)

    pool_by_addr = {}
    pair_pools = defaultdict(list)

    for p in pools:
        addr = p["address"].lower()
        pool_by_addr[addr] = p
        t0 = p["token0"].lower()
        t1 = p["token1"].lower()
        key = (min(t0, t1), max(t0, t1))
        pair_pools[key].append(p)

    return pool_by_addr, pair_pools

def check_v2_reserves(pair):
    out, _ = cast(["call", pair, "getReserves()(uint112,uint112,uint32)", "--rpc-url", rpc()])
    if not out:
        return 0, 0
    lines = out.split("\n")
    try:
        return int(lines[0]), int(lines[1])
    except:
        return 0, 0

def check_v3_price(pool):
    out, _ = cast(["call", pool, "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)", "--rpc-url", rpc()])
    if not out:
        return 0
    try:
        return int(out.split("\n")[0])
    except:
        return 0

def simulate_arb(pool_a, pool_b, token_in, token_out, amount_wei, pool_b_v3):
    sig = "exec(address,address,address,address,uint256,bool)"
    args = [pool_a, pool_b, token_in, token_out, str(amount_wei), str(pool_b_v3).lower()]
    out, err = cast(["call", CONTRACT, sig] + args + ["--from", WALLET, "--rpc-url", rpc()])
    if err and ("revert" in err.lower() or "error" in err.lower()):
        return False, err[:100]
    return True, "OK"

def execute_arb(pool_a, pool_b, token_in, token_out, amount_wei, pool_b_v3):
    sig = "exec(address,address,address,address,uint256,bool)"
    args = [pool_a, pool_b, token_in, token_out, str(amount_wei), str(pool_b_v3).lower()]
    out, err = cast(["send", CONTRACT, sig] + args + [
        "--private-key", PK, "--rpc-url", rpc(), "--gas-limit", "500000"
    ], timeout=30)
    return out, err


def main():
    print("=" * 60)
    print("REALTIME LONGTAIL HUNTER")
    print("Watching blocks for swap-induced price deviations...")
    print("=" * 60)

    pool_by_addr, pair_pools = load_pool_index()
    print(f"Indexed {len(pool_by_addr)} pools, {len(pair_pools)} pairs")

    # Find pairs with multiple pools (arb candidates)
    multi_pool_pairs = {k: v for k, v in pair_pools.items() if len(v) >= 2}
    print(f"Pairs with 2+ pools (arb candidates): {len(multi_pool_pairs)}")

    # Also find same-token different-fee V3 pools (intra-DEX arb)
    intra_dex = defaultdict(list)
    for (t0, t1), plist in pair_pools.items():
        v3_pools = [p for p in plist if p["pool_type"] == "V3"]
        if len(v3_pools) >= 2:
            intra_dex[(t0, t1)] = v3_pools
    print(f"V3 same-pair different-fee pools: {len(intra_dex)}")

    last_block = get_block()
    print(f"Starting from block {last_block}")

    cycle = 0
    opportunities_found = 0

    while True:
        cycle += 1
        current = get_block()

        if current <= last_block:
            time.sleep(2)
            continue

        # Get ALL swap events in new blocks
        swap_topics = [V3_SWAP, V2_SWAP]
        logs = get_logs(last_block + 1, current, swap_topics)

        if logs:
            # Group swaps by pool address
            swapped_pools = set()
            for log in logs:
                pool_addr = log.get("address", "").lower()
                swapped_pools.add(pool_addr)

            # Check if any swapped pool has a sister pool (same pair, different DEX/fee)
            for pool_addr in swapped_pools:
                if pool_addr not in pool_by_addr:
                    continue

                pool_info = pool_by_addr[pool_addr]
                t0 = pool_info["token0"].lower()
                t1 = pool_info["token1"].lower()
                key = (min(t0, t1), max(t0, t1))

                sister_pools = pair_pools.get(key, [])
                if len(sister_pools) < 2:
                    continue

                # Found a swapped pool with a sister! Check for arb
                has_weth = WETH.lower() in (t0, t1)
                if not has_weth:
                    continue

                longtail = t1 if t0 == WETH.lower() else t0

                for sister in sister_pools:
                    if sister["address"].lower() == pool_addr:
                        continue

                    # We have pool_addr (just swapped) and sister (potentially stale price)
                    # The swap may have moved the price, creating an arb vs sister

                    # Determine which is V3 (for flash swap source)
                    p_main = pool_info
                    p_sister = sister

                    # V3 pool must be poolA for flash swap
                    if p_main["pool_type"] == "V3":
                        pool_a = p_main["address"]
                        pool_b = p_sister["address"]
                        pool_b_v3 = p_sister["pool_type"] == "V3"
                    elif p_sister["pool_type"] == "V3":
                        pool_a = p_sister["address"]
                        pool_b = p_main["address"]
                        pool_b_v3 = p_main["pool_type"] == "V3"
                    else:
                        continue  # Both V2, skip

                    # Try different amounts
                    for amt_name, amt in [("0.001", 1000000000000000), ("0.005", 5000000000000000)]:
                        ok, msg = simulate_arb(pool_a, pool_b, WETH, longtail, amt, pool_b_v3)
                        if ok:
                            opportunities_found += 1
                            print(f"\n{'='*50}")
                            print(f"ARB OPPORTUNITY #{opportunities_found}!")
                            print(f"Token: {longtail}")
                            print(f"PoolA: {pool_a} ({p_main['dex_name']})")
                            print(f"PoolB: {pool_b} ({p_sister['dex_name']})")
                            print(f"Amount: {amt_name} ETH")
                            print(f"Executing...")

                            out, err = execute_arb(pool_a, pool_b, WETH, longtail, amt, pool_b_v3)
                            if out:
                                print(f"TX: {out[:300]}")
                            if err:
                                print(f"Err: {err[:200]}")
                            print(f"{'='*50}")
                            break
                        # If "no profit" try larger amount
                        if "no profit" not in msg:
                            break

            # Also check intra-DEX V3 arbs (same pair, different fees)
            for pool_addr in swapped_pools:
                if pool_addr not in pool_by_addr:
                    continue
                p = pool_by_addr[pool_addr]
                t0 = p["token0"].lower()
                t1 = p["token1"].lower()
                key = (min(t0, t1), max(t0, t1))

                if key not in intra_dex:
                    continue
                has_weth = WETH.lower() in (t0, t1)
                if not has_weth:
                    continue

                longtail = t1 if t0 == WETH.lower() else t0
                v3_sisters = [x for x in intra_dex[key] if x["address"].lower() != pool_addr]

                for sister in v3_sisters:
                    for amt_name, amt in [("0.001", 1000000000000000)]:
                        # Try both directions
                        ok1, _ = simulate_arb(p["address"], sister["address"], WETH, longtail, amt, True)
                        if ok1:
                            print(f"\nINTRA-DEX ARB! {p['dex_name']} fee={p['fee']} -> {sister['dex_name']} fee={sister['fee']}")
                            out, err = execute_arb(p["address"], sister["address"], WETH, longtail, amt, True)
                            print(f"Result: {(out or err)[:200]}")
                            break

                        ok2, _ = simulate_arb(sister["address"], p["address"], WETH, longtail, amt, True)
                        if ok2:
                            print(f"\nINTRA-DEX ARB! {sister['dex_name']} fee={sister['fee']} -> {p['dex_name']} fee={p['fee']}")
                            out, err = execute_arb(sister["address"], p["address"], WETH, longtail, amt, True)
                            print(f"Result: {(out or err)[:200]}")
                            break

        last_block = current

        if cycle % 30 == 0:
            print(f"[Cycle {cycle}] Block {current} | Opps: {opportunities_found} | "
                  f"Watching {len(multi_pool_pairs)} pairs + {len(intra_dex)} intra-V3")

        time.sleep(2)  # Base produces blocks every 2 seconds


if __name__ == "__main__":
    main()
