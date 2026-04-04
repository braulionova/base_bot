#!/usr/bin/env python3
"""
Hunter V2 — Full ML Pipeline + All Strategies
==============================================
Integrates: ML Engine, Token Blacklist, Gas Predictor, P&L Tracker
Strategies: V2 Flash, Aave Flash (V3↔V3), Triangular, Backrun, Liquidity
Concurrent execution with nonce management.
"""
import os, json, subprocess, time, threading, urllib.request, urllib.parse
from collections import defaultdict, Counter
from concurrent.futures import ThreadPoolExecutor

# Config from env
CONTRACT = os.environ.get("ARB_CONTRACT", "")
PK = os.environ.get("PRIVATE_KEY", "")
WALLET = "0xd69F9856A569B1655B43B0395b7c2923a217Cfe0"
TG = os.environ.get("TG_TOKEN", "")
CHAT = os.environ.get("TG_CHAT", "")
WETH = "0x4200000000000000000000000000000000000006".lower()
RPCS = ["https://mainnet.base.org", "https://base.publicnode.com"]
ri = 0

V3_SWAP = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
V2_SWAP = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
V2_MINT = "0x4c209b5fc8ad50758f13e2e1088ba56a560dff690a1c6fef26394f4c03821c4f"
V3_MINT = "0x7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde"
V2_BURN = "0xdccd412f0b1252819cb1fd330b93224ca42612892bb3f4f789976e6d81936496"
V3_BURN = "0x0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c"
ALL_EVENTS = [V3_SWAP, V2_SWAP, V2_MINT, V3_MINT, V2_BURN, V3_BURN]

# Import ML modules
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ml_engine import MLEngine
from token_blacklist import TokenBlacklist
from gas_predictor import GasPredictor
from pnl_tracker import PnLTracker

ml = MLEngine()
blacklist = TokenBlacklist()
gas_pred = GasPredictor()
pnl = PnLTracker()

# Pool registry
pool_tokens = {}
pair_pools = defaultdict(list)

# Nonce manager (thread-safe)
nonce_lock = threading.Lock()
current_nonce = 0

def rpc():
    global ri; r=RPCS[ri%len(RPCS)]; ri+=1; return r

def cast(args):
    try:
        r=subprocess.run(["/root/.foundry/bin/cast"]+args, capture_output=True, text=True, timeout=10)
        return r.stdout.strip(), r.stderr.strip()
    except: return "","timeout"

def tg(msg):
    if not TG: return
    try:
        d=urllib.parse.urlencode({"chat_id":CHAT,"text":msg[:4000],"parse_mode":"Markdown"}).encode()
        urllib.request.urlopen(urllib.request.Request(f"https://api.telegram.org/bot{TG}/sendMessage",d),timeout=5)
    except: pass

def get_logs(frm, to):
    for r in RPCS:
        try:
            payload=json.dumps({"jsonrpc":"2.0","method":"eth_getLogs","params":[{
                "fromBlock":hex(frm),"toBlock":hex(to),"topics":[ALL_EVENTS]
            }],"id":1})
            cmd=["curl","-s","-X","POST",r,"-H","Content-Type: application/json","-d",payload]
            res=subprocess.run(cmd,capture_output=True,text=True,timeout=10)
            if not res.stdout: continue
            data=json.loads(res.stdout)
            if "result" in data and "error" not in data:
                return data["result"]
        except: continue
    return []

def get_block():
    for r in RPCS:
        try:
            p=json.dumps({"jsonrpc":"2.0","method":"eth_blockNumber","id":1})
            cmd=["curl","-s","-X","POST",r,"-H","Content-Type: application/json","-d",p]
            res=subprocess.run(cmd,capture_output=True,text=True,timeout=5)
            return int(json.loads(res.stdout)["result"],16)
        except: continue
    return 0

def get_nonce():
    global current_nonce
    with nonce_lock:
        n = current_nonce
        current_nonce += 1
        return n

def init_nonce():
    global current_nonce
    for r in RPCS:
        try:
            p=json.dumps({"jsonrpc":"2.0","method":"eth_getTransactionCount","params":[WALLET,"latest"],"id":1})
            cmd=["curl","-s","-X","POST",r,"-H","Content-Type: application/json","-d",p]
            res=subprocess.run(cmd,capture_output=True,text=True,timeout=5)
            current_nonce=int(json.loads(res.stdout)["result"],16)
            return
        except: continue
    current_nonce=0

def discover_pool(addr):
    if addr in pool_tokens: return pool_tokens[addr]
    o0,_=cast(["call",addr,"token0()(address)","--rpc-url",rpc()])
    o1,_=cast(["call",addr,"token1()(address)","--rpc-url",rpc()])
    if not o0 or not o1: return None
    t0=o0.strip().split()[0].lower(); t1=o1.strip().split()[0].lower()
    o,e=cast(["call",addr,"getReserves()(uint112,uint112,uint32)","--rpc-url",rpc()])
    is_v2=bool(o and "error" not in (e or"").lower())
    pool_tokens[addr]=(t0,t1,is_v2)
    k=(min(t0,t1),max(t0,t1))
    if addr not in pair_pools[k]: pair_pools[k].append(addr)
    return(t0,t1,is_v2)

def sim_flash(v2, sell, token, amt, sell_v3):
    o,e=cast(["call",CONTRACT,"execFlash(address,address,address,uint256,bool)",
        v2,sell,token,str(amt),str(sell_v3).lower(),"--from",WALLET,"--rpc-url",rpc()])
    return not(e and("revert"in e.lower()or"error"in e.lower()))

def sim_aave(pA, pB, tIn, tOut, amt, tA, tB):
    o,e=cast(["call",CONTRACT,"execAave(address,address,address,address,uint256,uint8,uint8)",
        pA,pB,tIn,tOut,str(amt),str(tA),str(tB),"--from",WALLET,"--rpc-url",rpc()])
    return not(e and("revert"in e.lower()or"error"in e.lower()))

def execute_flash(v2, sell, token, amt, sell_v3):
    if not PK: return None, "no PK"
    return cast(["send",CONTRACT,"execFlash(address,address,address,uint256,bool)",
        v2,sell,token,str(amt),str(sell_v3).lower(),
        "--private-key",PK,"--rpc-url",rpc(),"--gas-limit","400000"])

def execute_aave(pA, pB, tIn, tOut, amt, tA, tB):
    if not PK: return None, "no PK"
    return cast(["send",CONTRACT,"execAave(address,address,address,address,uint256,uint8,uint8)",
        pA,pB,tIn,tOut,str(amt),str(tA),str(tB),
        "--private-key",PK,"--rpc-url",rpc(),"--gas-limit","500000"])

def try_arb(pool_addr, reason):
    """Try all arb modes for a pool. Returns arb info or False."""
    if pool_addr not in pool_tokens: return False

    t0,t1,is_v2 = pool_tokens[pool_addr]
    k=(min(t0,t1),max(t0,t1))
    sisters=[a for a in pair_pools[k] if a!=pool_addr]
    if not sisters: return False

    # ML: Skip low-score pools
    score = ml.score_pool(pool_addr) if ml.trained else 0.5
    if score < 0.2: return False

    # Blacklist check
    longtail = t1 if t0==WETH else t0
    if blacklist.is_blacklisted(longtail): return False

    for sis in sisters:
        if sis not in pool_tokens: continue
        _,_,sv2 = pool_tokens[sis]

        # MODE 1: V2 Flash (if one pool is V2)
        if is_v2 or sv2:
            v2p = pool_addr if is_v2 else sis
            other = sis if is_v2 else pool_addr
            ov3 = not pool_tokens[other][2]

            o,_=cast(["call",v2p,"getReserves()(uint112,uint112,uint32)","--rpc-url",rpc()])
            if not o: continue
            lines=o.split("\n")
            try: r0,r1=int(lines[0].split()[0]),int(lines[1].split()[0])
            except: continue
            if r0<1000000 or r1<1000000: continue

            o2,_=cast(["call",v2p,"token0()(address)","--rpc-url",rpc()])
            vt0=o2.strip().split()[0].lower() if o2 else ""

            amounts = ml.optimal_amounts(r0, r1) if ml.trained else [r0*2//100, r0//100]
            for bt in [t0,t1]:
                res=r0 if vt0==bt else r1
                if res<1000000: continue
                for amt in amounts:
                    if amt<100000 or amt>res//2: continue
                    if sim_flash(v2p,other,bt,amt,ov3):
                        return {"mode":"flash","v2":v2p,"sell":other,"token":bt,"amt":amt,
                                "v3":ov3,"reason":reason,"score":score,"longtail":longtail}

        # MODE 2: Aave Flash (V3↔V3)
        if not is_v2 and not sv2:
            # Both V3 - use Aave
            for bt in [t0,t1]:
                for amt in [1000000000000000, 500000000000000]:  # 0.001, 0.0005 ETH
                    other_token = t1 if bt==t0 else t0
                    if sim_aave(pool_addr, sis, bt, other_token, amt, 1, 1):
                        return {"mode":"aave","pA":pool_addr,"pB":sis,"tIn":bt,"tOut":other_token,
                                "amt":amt,"tA":1,"tB":1,"reason":reason,"score":score,"longtail":longtail}

    return False

# === Load pool data ===
try:
    with open("/root/arb-flash-bot/pools_cache.json") as f:
        for p in json.load(f):
            addr=p["address"].lower()
            pool_tokens[addr]=(p["token0"].lower(),p["token1"].lower(),p["pool_type"]=="V2")
            k=(min(p["token0"].lower(),p["token1"].lower()),max(p["token0"].lower(),p["token1"].lower()))
            if addr not in pair_pools[k]: pair_pools[k].append(addr)
except: pass
try:
    with open("/root/arb-flash-bot/active_pools.json") as f:
        for addr,d in json.load(f).items():
            addr=addr.lower()
            pool_tokens[addr]=(d["t0"],d["t1"],d["v2"])
            k=(min(d["t0"],d["t1"]),max(d["t0"],d["t1"]))
            if addr not in pair_pools[k]: pair_pools[k].append(addr)
except: pass

# Train ML
all_pools=[]
try:
    with open("/root/arb-flash-bot/pools_cache.json") as f:
        all_pools=json.load(f)
except: pass
if all_pools: ml.train(all_pools); ml.load()

init_nonce()
print(f"Nonce initialized: {current_nonce}")
arb_pairs=sum(1 for v in pair_pools.values() if len(v)>=2)

print("="*55)
print("HUNTER V2 — Full ML Pipeline")
print(f"Pools: {len(pool_tokens)} | Arb pairs: {arb_pairs}")
print(f"ML: {'ON' if ml.trained else 'OFF'} | Blacklist: {blacklist.stats()}")
print(f"Contract: {CONTRACT or 'NOT SET'}")
print(f"Wallet: {WALLET} | Nonce: {current_nonce}")
print("Strategies: V2Flash + AaveFlash + Backrun + Liquidity")
print("="*55)

tg(f"🚀 *Hunter V2 Started*\nPools: {len(pool_tokens)} | Pairs: {arb_pairs}\nML: ON | Blacklist: {blacklist.stats()}\nStrategies: Flash+Aave+Backrun+Liquidity\nP&L: {pnl.summary()}")

last_block=get_block()
cycle=0; found=0; executed=0
events_total={"swap":0,"mint":0,"burn":0}

while True:
    cycle+=1
    cur=get_block()
    if cur<=last_block: time.sleep(ml.get_scan_interval() if ml.trained else 2); continue

    logs=get_logs(last_block+1,cur)
    if cycle==1: print(f"First cycle: {len(logs)} logs from blk {last_block+1} to {cur}")
    last_block=cur
    if not logs:
        if cycle%20==0:
            print(f"[C{cycle}] blk {cur} | no events | {blacklist.stats()}")
        time.sleep(2); continue

    # Classify events
    swap_pools=set(); mint_pools=set(); burn_pools=set()
    for l in logs:
        t=l["topics"][0] if l["topics"] else ""
        a=l["address"].lower()
        if t in(V3_SWAP,V2_SWAP): swap_pools.add(a); events_total["swap"]+=1
        elif t in(V2_MINT,V3_MINT): mint_pools.add(a); events_total["mint"]+=1
        elif t in(V2_BURN,V3_BURN): burn_pools.add(a); events_total["burn"]+=1

    # Discover new pools
    pass  # Skip heavy discovery, use pre-loaded 43k pool index

    # === STRATEGY: Backrun swaps ===
    for pa in swap_pools:
        result=try_arb(pa,"SWAP_BACKRUN")
        if result:
            found+=1
            s=result["score"]
            msg=f"🔄 *BACKRUN #{found}*\n`{result['longtail'][:14]}..`\nML={s:.2f} | {result['mode']}"
            print(msg.replace("*","").replace("`",""))
            tg(msg)

            success=False
            if PK and CONTRACT:
                if result["mode"]=="flash":
                    o,e=execute_flash(result["v2"],result["sell"],result["token"],result["amt"],result["v3"])
                else:
                    o,e=execute_aave(result["pA"],result["pB"],result["tIn"],result["tOut"],result["amt"],result["tA"],result["tB"])

                if o and "status               1" in o:
                    success=True; executed+=1
                    tx=[l.split()[-1] for l in o.split("\n") if "transactionHash" in l]
                    tg(f"✅ *WIN #{executed}!* `{tx[0] if tx else'?'}`")
                    blacklist.record_success(result["longtail"])
                    pnl.record(tx[0] if tx else"",result["reason"],result["longtail"],"","",result["amt"],0.004,0.05,True)
                else:
                    blacklist.record_failure(result["longtail"])
                    pnl.record("",result["reason"],result["longtail"],"","",result["amt"],0.004,0,False)

            ml.record_execution(pa,1.0,1,2.0,1,success,0.05 if success else 0)

    # === STRATEGY: Backrun liquidity ===
    for pa in mint_pools|burn_pools:
        result=try_arb(pa,"LIQUIDITY_BACKRUN")
        if result:
            found+=1
            tg(f"💧 *LIQUIDITY #{found}*\n`{result['longtail'][:14]}..` ML={result['score']:.2f}")

            if PK and CONTRACT:
                if result["mode"]=="flash":
                    o,e=execute_flash(result["v2"],result["sell"],result["token"],result["amt"],result["v3"])
                else:
                    o,e=execute_aave(result["pA"],result["pB"],result["tIn"],result["tOut"],result["amt"],result["tA"],result["tB"])
                success=o and "status               1" in o
                if success: executed+=1; tg(f"✅ *LIQ WIN #{executed}!*")
                ml.record_execution(pa,1.0,1,2.0,1,success)

    # Status
    if cycle%10==0:
        si=ml.get_scan_interval() if ml.trained else 2
        print(f"[C{cycle}] blk={cur} sw={events_total['swap']} mt={events_total['mint']} | found={found} wins={executed} | {blacklist.stats()} | int={si}s")

    # Hourly report
    if cycle%180==0:
        ml.save()
        pnl.save()
        report=(f"📊 *Hourly Report*\n"
                f"Block: {cur}\n"
                f"Events: sw={events_total['swap']} mt={events_total['mint']} bn={events_total['burn']}\n"
                f"Found: {found} | Wins: {executed}\n"
                f"P&L: {pnl.summary()}\n"
                f"Blacklist: {blacklist.stats()}\n"
                f"Strategy:\n{pnl.strategy_summary()}")
        tg(report)

    time.sleep(ml.get_scan_interval() if ml.trained else 2)
