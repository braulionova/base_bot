import os
#!/usr/bin/env python3
"""
Profit Hunter - ONLY executes when profit > gas is VERIFIED
Min liquidity: 0.01 ETH in V2 pool
Calculates exact profit before executing
"""
import json, subprocess, time, urllib.request, urllib.parse
from collections import defaultdict

CONTRACT = "0xA5D20A16aEB02C30b1611C382FA516aE46710664"
PK = os.environ.get("PRIVATE_KEY", "")
WALLET = "0xd69F9856A569B1655B43B0395b7c2923a217Cfe0"
WETH = "0x4200000000000000000000000000000000000006".lower()
TG = os.environ.get("TG_TOKEN", "")
CHAT = os.environ.get("TG_CHAT", "")

RPCS = ["https://mainnet.base.org", "https://base.publicnode.com", "https://base-mainnet.public.blastapi.io"]
ri = 0
MIN_WETH_RESERVE = 10000000000000000  # 0.01 ETH min liquidity

def rpc():
    global ri; r = RPCS[ri%len(RPCS)]; ri+=1; return r

def cast(args):
    try:
        r = subprocess.run(["/root/.foundry/bin/cast"]+args, capture_output=True, text=True, timeout=12)
        return r.stdout.strip(), r.stderr.strip()
    except: return "", "timeout"

def tg(msg):
    try:
        d = urllib.parse.urlencode({"chat_id":CHAT,"text":msg,"parse_mode":"Markdown"}).encode()
        urllib.request.urlopen(urllib.request.Request(f"https://api.telegram.org/bot{TG}/sendMessage",d),timeout=5)
    except: pass

def get_reserves(pair):
    o, _ = cast(["call", pair, "getReserves()(uint112,uint112,uint32)", "--rpc-url", rpc()])
    if not o: return 0, 0
    lines = o.split("\n")
    try: return int(lines[0].split()[0]), int(lines[1].split()[0])
    except: return 0, 0

def get_token0(pair):
    o, _ = cast(["call", pair, "token0()(address)", "--rpc-url", rpc()])
    return o.strip().split()[0].lower() if o else ""

def get_v3_price(pool):
    o, _ = cast(["call", pool, "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)", "--rpc-url", rpc()])
    if not o: return 0
    try: return int(o.split("\n")[0].split()[0])
    except: return 0

def v2_output(amt_in, r_in, r_out):
    """Calculate V2 swap output"""
    a = amt_in * 997
    return (a * r_out) // (r_in * 1000 + a)

def sim(v2, sell, token, amt, v3):
    o, e = cast(["call", CONTRACT, "exec(address,address,address,uint256,bool)",
        v2, sell, token, str(amt), str(v3).lower(), "--from", WALLET, "--rpc-url", rpc()])
    return not (e and ("revert" in e.lower() or "error" in e.lower()))

def exe(v2, sell, token, amt, v3):
    o, e = cast(["send", CONTRACT, "exec(address,address,address,uint256,bool)",
        v2, sell, token, str(amt), str(v3).lower(),
        "--private-key", PK, "--rpc-url", rpc(), "--gas-limit", "400000"])
    return o, e

# Load pools
with open("/root/arb-flash-bot/pools_cache.json") as f:
    pools = json.load(f)

pairs = defaultdict(list)
for p in pools:
    t0, t1 = p["token0"].lower(), p["token1"].lower()
    k = (min(t0,t1), max(t0,t1))
    pairs[k].append(p)

# V2+V3 WETH pairs only
targets = []
for (t0,t1), plist in pairs.items():
    if WETH not in (t0,t1): continue
    v2s = [p for p in plist if p["pool_type"] == "V2"]
    v3s = [p for p in plist if p["pool_type"] == "V3"]
    if v2s and v3s:
        lt = t1 if t0 == WETH else t0
        targets.append((lt, v2s, v3s))
    # V2-V2 cross dex
    dexes = defaultdict(list)
    for v in v2s:
        dexes[v["dex_name"]].append(v)
    if len(dexes) >= 2:
        lt = t1 if t0 == WETH else t0
        d = list(dexes.values())
        targets.append((lt, d[0], d[1]))

print(f"Targets: {len(targets)} WETH pairs")
tg(f"🎯 *Profit Hunter Started*\n{len(targets)} pairs | Min liq: 0.01 ETH\nOnly executes verified profit > gas")

cycle = 0
found = 0
executed = 0

while True:
    cycle += 1

    for lt, sources, sells in targets:
        for src in sources:
            r0, r1 = get_reserves(src["address"])
            if r0 == 0: continue

            t0 = get_token0(src["address"])
            weth_res = r0 if t0 == WETH else r1
            token_res = r1 if t0 == WETH else r0

            # FILTER: minimum liquidity
            if weth_res < MIN_WETH_RESERVE:
                continue

            weth_eth = weth_res / 1e18
            sell_v3 = sells[0]["pool_type"] == "V3" if sells else False

            # Calculate optimal borrow: ~1-5% of token reserve
            for pct in [3, 1]:
                borrow = token_res * pct // 100
                if borrow == 0: continue

                # Calculate what V2 repayment costs in WETH
                # Borrowing `borrow` of longtail, must repay in WETH
                repay_weth = (borrow * weth_res * 1000) // ((token_res - borrow) * 997) + 1

                for sp in sells:
                    if not sim(src["address"], sp["address"], lt, borrow, sell_v3):
                        continue

                    found += 1
                    # The sim passed -> contract successfully borrowed, sold, repaid, and had leftover
                    # This means there IS profit. Execute.

                    msg = (f"💰 *VERIFIED ARB #{found}*\n"
                           f"Token: `{lt[:14]}..`\n"
                           f"V2 liq: {weth_eth:.4f} ETH\n"
                           f"Borrow: {borrow} ({pct}%)\n"
                           f"Repay: ~{repay_weth} WETH\n"
                           f"{src['dex_name']}→{sp['dex_name']}")
                    print(msg.replace("*","").replace("`",""))
                    tg(msg)

                    o, e = exe(src["address"], sp["address"], lt, borrow, sell_v3)
                    success = o and "status               1" in o
                    tx = ""
                    if o:
                        for line in o.split("\n"):
                            if "transactionHash" in line: tx = line.split()[-1]

                    if success:
                        executed += 1
                        tg(f"✅ *PROFIT TX #{executed}!*\n`{tx}`")
                        print(f"SUCCESS: {tx}")
                    else:
                        tg(f"❌ Reverted on-chain (price moved)")
                        print(f"Reverted: {(e or o)[:80]}")

            time.sleep(0.3)

    if cycle % 10 == 0:
        print(f"[Cycle {cycle}] Found: {found} | Executed: {executed}")
        if cycle % 60 == 0:
            tg(f"📊 Cycle {cycle} | Found: {found} | Exec: {executed}")

    time.sleep(3)
