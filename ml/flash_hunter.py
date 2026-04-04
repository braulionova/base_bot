import os
#!/usr/bin/env python3
"""
Flash Loan Hunter - Continuous scanner for V2 flash swap arbs
Zero capital. Scans every block. Executes immediately.
Sends Telegram notifications.
"""
import json, subprocess, time, sys, urllib.request
from collections import defaultdict

CONTRACT = "0xA5D20A16aEB02C30b1611C382FA516aE46710664"
PK = os.environ.get("PRIVATE_KEY", "")
WALLET = "0xd69F9856A569B1655B43B0395b7c2923a217Cfe0"
WETH = "0x4200000000000000000000000000000000000006".lower()
TG_TOKEN = os.environ.get("TG_TOKEN", "")
TG_CHAT = os.environ.get("TG_CHAT", "")

RPCS = ["https://mainnet.base.org", "https://base.publicnode.com", "https://base.drpc.org"]
rpc_i = 0

def rpc():
    global rpc_i
    r = RPCS[rpc_i % len(RPCS)]
    rpc_i += 1
    return r

def cast(args):
    r = subprocess.run(["/root/.foundry/bin/cast"]+args, capture_output=True, text=True, timeout=15)
    return r.stdout.strip(), r.stderr.strip()

def tg(msg):
    try:
        data = urllib.parse.urlencode({"chat_id": TG_CHAT, "text": msg, "parse_mode": "Markdown"}).encode()
        req = urllib.request.Request(f"https://api.telegram.org/bot{TG_TOKEN}/sendMessage", data=data)
        urllib.request.urlopen(req, timeout=5)
    except: pass

def load_targets():
    with open("/root/arb-flash-bot/pools_cache.json") as f:
        pools = json.load(f)
    pairs = defaultdict(list)
    for p in pools:
        t0, t1 = p["token0"].lower(), p["token1"].lower()
        k = (min(t0,t1), max(t0,t1))
        pairs[k].append(p)

    targets = []
    for (t0,t1), plist in pairs.items():
        has_weth = WETH in (t0,t1)
        v2s = [p for p in plist if p["pool_type"] == "V2"]
        v3s = [p for p in plist if p["pool_type"] == "V3"]
        # V2->V3 arbs
        if v2s and v3s and has_weth:
            longtail = t1 if t0 == WETH else t0
            targets.append(("v2v3", longtail, v2s, v3s))
        # V2->V2 arbs (different DEXes)
        if len(v2s) >= 2 and has_weth:
            dexes = set(p["dex_name"] for p in v2s)
            if len(dexes) >= 2:
                longtail = t1 if t0 == WETH else t0
                targets.append(("v2v2", longtail, v2s, []))
    return targets

def check_v2_reserves(pair_addr):
    out, _ = cast(["call", pair_addr, "getReserves()(uint112,uint112,uint32)", "--rpc-url", rpc()])
    if not out: return 0, 0
    lines = out.split("\n")
    try: return int(lines[0].split()[0]), int(lines[1].split()[0])
    except: return 0, 0

def simulate(v2pair, sellpool, borrow_token, amount, sell_v3):
    out, err = cast(["call", CONTRACT, "exec(address,address,address,uint256,bool)",
        v2pair, sellpool, borrow_token, str(amount), str(sell_v3).lower(),
        "--from", WALLET, "--rpc-url", rpc()])
    if err and ("revert" in err.lower() or "error" in err.lower()):
        return False
    return True

def execute(v2pair, sellpool, borrow_token, amount, sell_v3):
    out, err = cast(["send", CONTRACT, "exec(address,address,address,uint256,bool)",
        v2pair, sellpool, borrow_token, str(amount), str(sell_v3).lower(),
        "--private-key", PK, "--rpc-url", rpc(), "--gas-limit", "500000"])
    return out, err

import urllib.parse

print("=" * 50)
print("FLASH LOAN HUNTER - Zero Capital")
print(f"Contract: {CONTRACT}")
print("=" * 50)

targets = load_targets()
print(f"Targets: {len(targets)} V2/V3 WETH pairs")
tg("🔄 *Flash Hunter Started*\nTargets: " + str(len(targets)) + " pairs\nZero capital mode")

cycle = 0
while True:
    cycle += 1

    for arb_type, longtail, v2s, v3s in targets:
        for v2 in v2s:
            r0, r1 = check_v2_reserves(v2["address"])
            if r0 == 0: continue

            out, _ = cast(["call", v2["address"], "token0()(address)", "--rpc-url", rpc()])
            t0 = out.strip().lower() if out else ""
            weth_res = r0 if t0 == WETH else r1
            token_res = r1 if t0 == WETH else r0

            if weth_res < 50000000000000: continue  # < 0.00005 ETH skip

            # Try different borrow amounts
            for pct in [5, 2, 1]:
                borrow = token_res * pct // 100
                if borrow == 0: continue

                sell_pools = v3s if arb_type == "v2v3" else [p for p in v2s if p["address"] != v2["address"]]
                sell_v3 = arb_type == "v2v3"

                for sp in sell_pools:
                    if simulate(v2["address"], sp["address"], longtail, borrow, sell_v3):
                        weth_eth = weth_res / 1e18
                        msg = (f"🎯 *FLASH ARB FOUND!*\n"
                               f"Token: `{longtail[:16]}..`\n"
                               f"V2: {v2['dex_name']} ({weth_eth:.4f} ETH liq)\n"
                               f"Sell: {sp['dex_name']} ({'V3' if sell_v3 else 'V2'})\n"
                               f"Borrow: {borrow} ({pct}% of reserve)\n"
                               f"Executing...")
                        print(msg.replace("*",""))
                        tg(msg)

                        out, err = execute(v2["address"], sp["address"], longtail, borrow, sell_v3)
                        if out and "status               1" in out:
                            tx = ""
                            for line in out.split("\n"):
                                if "transactionHash" in line:
                                    tx = line.split()[-1]
                            tg(f"✅ *TX SUCCESS!*\nHash: `{tx}`")
                            print(f"SUCCESS! TX: {tx}")
                        elif out and "status               0" in out:
                            tg("❌ TX reverted on-chain")
                            print("TX reverted")
                        else:
                            print(f"Result: {(out or err)[:150]}")

            time.sleep(0.2)

    if cycle % 30 == 0:
        print(f"[Cycle {cycle}] Scanned {len(targets)} pairs")

    time.sleep(3)  # Base block time ~2s
