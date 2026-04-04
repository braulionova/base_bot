#!/usr/bin/env python3
"""
Multi-Strategy Hunter
1. BACKRUN: detect large swaps, arb the price impact
2. COPY BOTS: replicate successful arb txs on same pools
3. STABLECOIN: USDC/USDbC micro-arbs
All via V2 flash swap. Zero capital.
"""
import json, subprocess, time, urllib.request, urllib.parse
from collections import defaultdict, Counter

CONTRACT = "0xA5D20A16aEB02C30b1611C382FA516aE46710664"
PK = "0xfb7d62cfba588e53df82089cb9ad1b99397b8718e821b23585f6608c01d2de61"
WALLET = "0xd69F9856A569B1655B43B0395b7c2923a217Cfe0"
WETH = "0x4200000000000000000000000000000000000006".lower()
TG = "7700486521:AAFuu2ygokFNesm1uB6_JM96KxQwcc4q-dk"
CHAT = "483428397"
RPCS = ["https://mainnet.base.org","https://base.publicnode.com","https://base-mainnet.public.blastapi.io"]
ri = 0
V3_SWAP = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
V2_SWAP = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"

def rpc():
    global ri; r=RPCS[ri%len(RPCS)]; ri+=1; return r

def cast(args):
    try:
        r=subprocess.run(["/root/.foundry/bin/cast"]+args,capture_output=True,text=True,timeout=12)
        return r.stdout.strip(),r.stderr.strip()
    except: return "","timeout"

def tg(msg):
    try:
        d=urllib.parse.urlencode({"chat_id":CHAT,"text":msg,"parse_mode":"Markdown"}).encode()
        urllib.request.urlopen(urllib.request.Request(f"https://api.telegram.org/bot{TG}/sendMessage",d),timeout=5)
    except: pass

def get_logs(frm,to):
    payload=json.dumps({"jsonrpc":"2.0","method":"eth_getLogs","params":[{"fromBlock":hex(frm),"toBlock":hex(to),"topics":[[V3_SWAP,V2_SWAP]]}],"id":1}).encode()
    try:
        req=urllib.request.Request(rpc(),data=payload,headers={"Content-Type":"application/json"})
        with urllib.request.urlopen(req,timeout=10) as resp:
            return json.loads(resp.read()).get("result",[])
    except: return []

def get_block():
    o,_=cast(["block-number","--rpc-url",rpc()])
    try: return int(o)
    except: return 0

def sim(v2,sell,token,amt,v3):
    o,e=cast(["call",CONTRACT,"exec(address,address,address,uint256,bool)",v2,sell,token,str(amt),str(v3).lower(),"--from",WALLET,"--rpc-url",rpc()])
    return not(e and("revert"in e.lower()or"error"in e.lower()))

def exe(v2,sell,token,amt,v3):
    return cast(["send",CONTRACT,"exec(address,address,address,uint256,bool)",v2,sell,token,str(amt),str(v3).lower(),"--private-key",PK,"--rpc-url",rpc(),"--gas-limit","400000"])

def get_v2_info(pair):
    o,_=cast(["call",pair,"getReserves()(uint112,uint112,uint32)","--rpc-url",rpc()])
    if not o: return 0,0,""
    lines=o.split("\n")
    try: r0,r1=int(lines[0].split()[0]),int(lines[1].split()[0])
    except: return 0,0,""
    o2,_=cast(["call",pair,"token0()(address)","--rpc-url",rpc()])
    t0=o2.strip().split()[0].lower() if o2 else ""
    return r0,r1,t0

# Load pools
with open("/root/arb-flash-bot/pools_cache.json") as f:
    pools=json.load(f)
pool_by_addr={p["address"].lower():p for p in pools}
pairs=defaultdict(list)
for p in pools:
    t0,t1=p["token0"].lower(),p["token1"].lower()
    pairs[(min(t0,t1),max(t0,t1))].append(p)

print("="*50)
print("MULTI-STRATEGY HUNTER - Zero Capital")
print(f"Pools indexed: {len(pool_by_addr)}")
print("="*50)
tg("🔥 *Multi Hunter Started*\nBackrun + Copy + Stablecoin\nZero capital")

last_block=get_block()
cycle=0; found=0; executed=0

while True:
    cycle+=1
    current=get_block()
    if current<=last_block:
        time.sleep(2); continue

    logs=get_logs(last_block+1,current)
    last_block=current

    if not logs:
        if cycle%15==0: print(f"[C{cycle}] blk {current} no swaps")
        time.sleep(2); continue

    swapped=defaultdict(int)
    for l in logs: swapped[l["address"].lower()]+=1
    tx_count=Counter(l["transactionHash"]for l in logs)

    # === STRATEGY 1: BACKRUN large swaps ===
    for pool_addr,cnt in swapped.items():
        if pool_addr not in pool_by_addr: continue
        p=pool_by_addr[pool_addr]
        t0,t1=p["token0"].lower(),p["token1"].lower()
        k=(min(t0,t1),max(t0,t1))
        sisters=[x for x in pairs.get(k,[]) if x["address"].lower()!=pool_addr]
        if not sisters: continue

        # Need one V2 for flash borrow
        v2_options=[s for s in sisters if s["pool_type"]=="V2"]
        if p["pool_type"]=="V2": v2_options.append(p)
        v3_options=[s for s in sisters if s["pool_type"]=="V3"]
        if p["pool_type"]=="V3": v3_options.append(p)

        for v2 in v2_options:
            r0,r1,v2_t0=get_v2_info(v2["address"])
            if r0<100000 or r1<100000: continue

            sell_targets=v3_options if v3_options else [s for s in v2_options if s["address"]!=v2["address"]]
            for bt in [t0,t1]:
                res=r0 if v2_t0==bt else r1
                if res<100000: continue
                for pct in [3,1]:
                    amt=res*pct//100
                    if amt<10000: continue
                    for sp in sell_targets:
                        is_v3=sp["pool_type"]=="V3"
                        if sim(v2["address"],sp["address"],bt,amt,is_v3):
                            found+=1
                            msg=f"🎯 *BACKRUN #{found}*\n{v2['dex_name']}→{sp['dex_name']}\n`{bt[:14]}..` amt={amt}"
                            print(msg.replace("*","").replace("`",""))
                            tg(msg)
                            o,e=exe(v2["address"],sp["address"],bt,amt,is_v3)
                            if o and "status               1"in o:
                                executed+=1
                                tx=[l.split()[-1]for l in o.split("\n")if"transactionHash"in l]
                                tg(f"✅ *WIN #{executed}!* `{tx[0]if tx else'?'}`")
                                print(f"WIN: {tx}")
                            break
                    else: continue
                    break

    # === STRATEGY 2: COPY successful arb bots ===
    arb_txs=[tx for tx,cnt in tx_count.items()if cnt>=2]
    for tx in arb_txs[:2]:
        tx_pools=list(set(l["address"].lower()for l in logs if l["transactionHash"]==tx))
        if len(tx_pools)<2: continue
        for i,p1 in enumerate(tx_pools):
            for p2 in tx_pools[i+1:]:
                if p1 not in pool_by_addr or p2 not in pool_by_addr: continue
                pi1,pi2=pool_by_addr[p1],pool_by_addr[p2]
                v2p=pi1 if pi1["pool_type"]=="V2"else(pi2 if pi2["pool_type"]=="V2"else None)
                if not v2p: continue
                other=pi2 if v2p==pi1 else pi1
                r0,r1,v2_t0=get_v2_info(v2p["address"])
                if r0<100000: continue
                t0,t1=v2p["token0"].lower(),v2p["token1"].lower()
                for bt in[t0,t1]:
                    res=r0 if v2_t0==bt else r1
                    amt=res//50
                    if amt<10000: continue
                    is_v3=other["pool_type"]=="V3"
                    if sim(v2p["address"],other["address"],bt,amt,is_v3):
                        found+=1
                        tg(f"🔄 *COPYBOT #{found}*\nFrom `{tx[:16]}..`")
                        o,e=exe(v2p["address"],other["address"],bt,amt,is_v3)
                        if o and"status               1"in o:
                            executed+=1
                            tg(f"✅ *COPYBOT WIN #{executed}!*")

    if cycle%15==0:
        print(f"[C{cycle}] blk {current} swaps={len(logs)} found={found} wins={executed}")
    if cycle%90==0:
        tg(f"📊 C{cycle} | swaps={len(logs)} | found={found} | wins={executed}")

    time.sleep(2)
