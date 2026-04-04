#!/usr/bin/env python3
"""
Event Hunter - Backruns liquidity events + large swaps
Strategy 2: Mint/Burn on pool -> price shifts -> flash arb vs sister pool
Strategy 3: Large swap -> price impact -> flash arb vs sister pool
Uses V2 flash swap. Zero capital.
"""
import os, json, subprocess, time, urllib.request, urllib.parse
from collections import defaultdict

CONTRACT = os.environ.get("ARB_CONTRACT", "0xA5D20A16aEB02C30b1611C382FA516aE46710664")
PK = os.environ.get("PRIVATE_KEY", "")
WALLET = "0xd69F9856A569B1655B43B0395b7c2923a217Cfe0"
TG = os.environ.get("TG_TOKEN", "")
CHAT = os.environ.get("TG_CHAT", "")
WETH = "0x4200000000000000000000000000000000000006".lower()

# Event signatures
V3_SWAP = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
V2_SWAP = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
V2_MINT = "0x4c209b5fc8ad50758f13e2e1088ba56a560dff690a1c6fef26394f4c03821c4f"
V2_BURN = "0xdccd412f0b1252819cb1fd330b93224ca42612892bb3f4f789976e6d81936496"
V3_MINT = "0x7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde"
V3_BURN = "0x0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c"
PAIR_CREATED = "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9"
POOL_CREATED = "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118"

ALL_EVENTS = [V3_SWAP, V2_SWAP, V2_MINT, V2_BURN, V3_MINT, V3_BURN, PAIR_CREATED, POOL_CREATED]

# ML Engine
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ml_engine import MLEngine
ml = MLEngine()

RPCS = ["https://mainnet.base.org", "https://base.publicnode.com", "http://localhost:8545"]
ri = 0

# Dynamic pool registry
pool_tokens = {}  # addr -> (t0, t1, is_v2)
pair_pools = defaultdict(list)  # (tA, tB) -> [addr, ...]

def rpc():
    global ri
    r = RPCS[ri % len(RPCS)]
    ri += 1
    return r

def cast(args):
    try:
        r = subprocess.run(["/root/.foundry/bin/cast"] + args, capture_output=True, text=True, timeout=10)
        return r.stdout.strip(), r.stderr.strip()
    except:
        return "", "timeout"

def tg(msg):
    if not TG: return
    try:
        d = urllib.parse.urlencode({"chat_id": CHAT, "text": msg, "parse_mode": "Markdown"}).encode()
        urllib.request.urlopen(urllib.request.Request(f"https://api.telegram.org/bot{TG}/sendMessage", d), timeout=5)
    except: pass

def get_block():
    for r in RPCS:
        o, _ = cast(["block-number", "--rpc-url", r])
        try: return int(o)
        except: continue
    return 0

def get_logs(frm, to):
    """Get swap + mint + burn events in block range"""
    # Only swap events first (most reliable on public RPCs)
    swap_topics = [V3_SWAP, V2_SWAP, V2_MINT, V3_MINT, V2_BURN, V3_BURN]
    for r in RPCS:
        try:
            payload = json.dumps({"jsonrpc": "2.0", "method": "eth_getLogs", "params": [{
                "fromBlock": hex(frm), "toBlock": hex(to),
                "topics": [swap_topics]
            }], "id": 1})
            cmd = ["curl", "-s", "-X", "POST", r, "-H", "Content-Type: application/json", "-d", payload]
            res = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
            if not res.stdout: continue
            data = json.loads(res.stdout)
            if "result" in data and "error" not in data:
                return data["result"]
        except:
            continue
    return []

def discover_pool(addr):
    if addr in pool_tokens:
        return pool_tokens[addr]
    o0, _ = cast(["call", addr, "token0()(address)", "--rpc-url", rpc()])
    o1, _ = cast(["call", addr, "token1()(address)", "--rpc-url", rpc()])
    if not o0 or not o1: return None
    t0 = o0.strip().split()[0].lower()
    t1 = o1.strip().split()[0].lower()
    # Check if V2
    o, e = cast(["call", addr, "getReserves()(uint112,uint112,uint32)", "--rpc-url", rpc()])
    is_v2 = bool(o and "error" not in (e or "").lower())
    pool_tokens[addr] = (t0, t1, is_v2)
    k = (min(t0, t1), max(t0, t1))
    if addr not in pair_pools[k]:
        pair_pools[k].append(addr)
    return (t0, t1, is_v2)

def sim(v2, sell, token, amt, v3):
    o, e = cast(["call", CONTRACT, "exec(address,address,address,uint256,bool)",
        v2, sell, token, str(amt), str(v3).lower(), "--from", WALLET, "--rpc-url", rpc()])
    return not (e and ("revert" in e.lower() or "error" in e.lower()))

def exe(v2, sell, token, amt, v3):
    return cast(["send", CONTRACT, "exec(address,address,address,uint256,bool)",
        v2, sell, token, str(amt), str(v3).lower(),
        "--private-key", PK, "--rpc-url", rpc(), "--gas-limit", "400000"])

def try_arb(pool_addr, reason):
    """Try to arb a pool against its sisters after an event"""
    if pool_addr not in pool_tokens:
        discover_pool(pool_addr)
    if pool_addr not in pool_tokens:
        return False

    t0, t1, is_v2 = pool_tokens[pool_addr]
    k = (min(t0, t1), max(t0, t1))
    sisters = [a for a in pair_pools[k] if a != pool_addr]
    if not sisters:
        return False

    for sister in sisters:
        if sister not in pool_tokens:
            discover_pool(sister)
        if sister not in pool_tokens:
            continue
        _, _, sis_v2 = pool_tokens[sister]

        # Need at least one V2 for flash
        if not is_v2 and not sis_v2:
            continue

        v2_pool = pool_addr if is_v2 else sister
        other = sister if is_v2 else pool_addr
        other_v3 = not pool_tokens[other][2]

        # Get V2 reserves
        o, _ = cast(["call", v2_pool, "getReserves()(uint112,uint112,uint32)", "--rpc-url", rpc()])
        if not o: continue
        lines = o.split("\n")
        try: r0, r1 = int(lines[0].split()[0]), int(lines[1].split()[0])
        except: continue
        if r0 < 1000000 or r1 < 1000000: continue

        o2, _ = cast(["call", v2_pool, "token0()(address)", "--rpc-url", rpc()])
        v2_t0 = o2.strip().split()[0].lower() if o2 else ""

        # ML: Check pool score first - skip low-score pools
        pool_score = ml.score_pool(pool_addr) if ml.trained else 0.5
        if pool_score < 0.3:
            continue  # ML says this pool is unlikely to be profitable

        for bt in [t0, t1]:
            res = r0 if v2_t0 == bt else r1
            if res < 1000000: continue

            # ML: Use optimal trade sizing instead of fixed percentages
            amounts = ml.optimal_amounts(res, r0 if v2_t0 != bt else r1) if ml.trained else [res * p // 100 for p in [3, 1]]

            for amt in amounts:
                if amt < 100000: continue

                # ML: Predict success before wasting time on simulation
                success_prob = ml.predict_success(
                    pool_addr, r0 / max(r1, 1), len(sisters),
                    2.0, len([a for a in pair_pools.get(k, [])]))
                if success_prob < 0.2 and ml.trained:
                    continue  # ML says too risky

                if sim(v2_pool, other, bt, amt, other_v3):
                    return (v2_pool, other, bt, amt, other_v3, reason, pool_score, success_prob)
    return False

# === Pre-load pools from cache ===
try:
    with open("/root/arb-flash-bot/pools_cache.json") as f:
        for p in json.load(f):
            addr = p["address"].lower()
            t0, t1 = p["token0"].lower(), p["token1"].lower()
            is_v2 = p["pool_type"] == "V2"
            pool_tokens[addr] = (t0, t1, is_v2)
            k = (min(t0, t1), max(t0, t1))
            if addr not in pair_pools[k]:
                pair_pools[k].append(addr)
except: pass

# Also load dynamically discovered pools
try:
    with open("/root/arb-flash-bot/active_pools.json") as f:
        for addr, d in json.load(f).items():
            addr = addr.lower()
            pool_tokens[addr] = (d["t0"], d["t1"], d["v2"])
            k = (min(d["t0"], d["t1"]), max(d["t0"], d["t1"]))
            if addr not in pair_pools[k]:
                pair_pools[k].append(addr)
except: pass

arb_pairs = sum(1 for v in pair_pools.values() if len(v) >= 2)

# Train ML on all pool data
print("Training ML models...")
all_pools = []
try:
    with open("/root/arb-flash-bot/pools_cache.json") as f:
        all_pools = json.load(f)
except: pass
if all_pools:
    ml.train(all_pools)
    ml.load()  # Load any previous execution history
    print(f"ML trained on {len(all_pools)} pools")
    priority = ml.get_priority_pools(10)
    print(f"Top ML pools: {[f'{a[:10]}..={s:.2f}' for a,s in priority]}")

print("=" * 55)
print("EVENT HUNTER + ML - Backrun Liquidity + Large Swaps")
print(f"Pools: {len(pool_tokens)} | Arb pairs: {arb_pairs} | ML: {'ON' if ml.trained else 'OFF'}")
print(f"Contract: {CONTRACT}")
print("=" * 55)
tg(f"⚡ *ML Event Hunter Started*\nPools: {len(pool_tokens)} | ML: ON\nArb pairs: {arb_pairs}\nModels: PoolScorer + TradeSizer + SuccessPredictor + Timing")

last_block = get_block()
cycle = 0
found = 0
executed = 0
events_seen = {"swap": 0, "mint": 0, "burn": 0, "new_pool": 0}

while True:
    cycle += 1
    current = get_block()
    if current <= last_block:
        time.sleep(2)
        continue

    logs = get_logs(last_block + 1, current)
    last_block = current

    if not logs:
        if cycle % 20 == 0:
            print(f"[C{cycle}] blk {current} | no events | pools={len(pool_tokens)} | pairs={arb_pairs}")
        time.sleep(2)
        continue

    # Classify events
    mints = []
    burns = []
    large_swaps = []
    new_pools = []

    for l in logs:
        topic = l["topics"][0] if l["topics"] else ""
        addr = l["address"].lower()

        if topic in (V2_MINT, V3_MINT):
            mints.append(addr)
            events_seen["mint"] += 1

        elif topic in (V2_BURN, V3_BURN):
            burns.append(addr)
            events_seen["burn"] += 1

        elif topic in (V2_SWAP, V3_SWAP):
            events_seen["swap"] += 1
            # Check if large swap (data contains amounts)
            # For simplicity, track all swaps on pools with sisters
            if addr in pool_tokens:
                t0, t1, _ = pool_tokens[addr]
                k = (min(t0, t1), max(t0, t1))
                if len(pair_pools.get(k, [])) >= 2:
                    large_swaps.append(addr)

        elif topic in (PAIR_CREATED, POOL_CREATED):
            events_seen["new_pool"] += 1
            # Discover the new pool
            if len(l["topics"]) >= 3:
                t0 = "0x" + l["topics"][1][-40:]
                t1 = "0x" + l["topics"][2][-40:]
                new_pools.append((addr, t0.lower(), t1.lower()))

    # === STRATEGY 2: Backrun liquidity events ===
    for pool_addr in set(mints + burns):
        result = try_arb(pool_addr, "LIQUIDITY")
        if result:
            v2p, other, bt, amt, ov3, reason, p_score, s_prob = result
            found += 1
            msg = f"💧 *LIQUIDITY ARB #{found}!*\n`{bt[:14]}..`\n{reason} | ML score={p_score:.2f} prob={s_prob:.2f}\nV2→{'V3' if ov3 else 'V2'}"
            print(msg.replace("*", "").replace("`", ""))
            tg(msg)
            success = False
            if PK:
                o, e = exe(v2p, other, bt, amt, ov3)
                if o and "status               1" in o:
                    executed += 1
                    success = True
                    tx = [l.split()[-1] for l in o.split("\n") if "transactionHash" in l]
                    tg(f"✅ *WIN #{executed}!* `{tx[0] if tx else '?'}`")
                    print(f"WIN: {tx}")
            # ML: Learn from result
            ml.record_execution(pool_addr, 1.0, 1, 2.0, 1, success, 0.05 if success else 0)

    # === STRATEGY 3: Backrun large swaps ===
    for pool_addr in set(large_swaps):
        result = try_arb(pool_addr, "SWAP")
        if result:
            v2p, other, bt, amt, ov3, reason, p_score, s_prob = result
            found += 1
            msg = f"🔄 *SWAP BACKRUN #{found}!*\n`{bt[:14]}..`\nML score={p_score:.2f} prob={s_prob:.2f}\nV2→{'V3' if ov3 else 'V2'}"
            print(msg.replace("*", "").replace("`", ""))
            tg(msg)
            success = False
            if PK:
                o, e = exe(v2p, other, bt, amt, ov3)
                if o and "status               1" in o:
                    executed += 1
                    success = True
                    tx = [l.split()[-1] for l in o.split("\n") if "transactionHash" in l]
                    tg(f"✅ *BACKRUN WIN #{executed}!* `{tx[0] if tx else '?'}`")
            ml.record_execution(pool_addr, 1.0, 1, 2.0, 1, success, 0.05 if success else 0)

    # === NEW POOLS: register for future arbs ===
    for factory, t0, t1 in new_pools:
        k = (min(t0, t1), max(t0, t1))
        existing = pair_pools.get(k, [])
        if existing:
            print(f"  NEW POOL for existing pair! {t0[:12]}/{t1[:12]} ({len(existing)} sisters)")
            tg(f"🆕 *New pool for tracked pair!*\n`{t0[:12]}/{t1[:12]}`\n{len(existing)} existing pools")

    arb_pairs = sum(1 for v in pair_pools.values() if len(v) >= 2)

    if cycle % 10 == 0:
        s = events_seen
        scan_int = ml.get_scan_interval() if ml.trained else 2
        print(f"[C{cycle}] blk {current} | swaps={s['swap']} mints={s['mint']} | found={found} wins={executed} | ML_interval={scan_int}s")

    if cycle % 60 == 0:
        s = events_seen
        # ML: Save learned data periodically
        ml.save()
        best_hours = ml.timing.best_hours()[:3]
        hours_str = ", ".join([f"{h}:00={p:.3f}" for h,p in best_hours]) if best_hours else "learning..."
        tg(f"📊 *ML Event Hunter*\nC{cycle} | blk {current}\nSwaps: {s['swap']} Mints: {s['mint']}\nFound: {found} | Wins: {executed}\nBest hours: {hours_str}")

    # ML: Dynamic scan interval
    scan_interval = ml.get_scan_interval() if ml.trained else 2
    time.sleep(scan_interval)
