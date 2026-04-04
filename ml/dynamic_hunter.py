#!/usr/bin/env python3
"""
Dynamic Hunter - Discovers pools on-the-fly from live swaps, arbs instantly.
Solves the core problem: we were blind to 85% of active pools.
"""
import os, json, subprocess, time, urllib.request, urllib.parse
from collections import defaultdict

CONTRACT = "0xA5D20A16aEB02C30b1611C382FA516aE46710664"
PK = os.environ.get("PRIVATE_KEY", "")
WALLET = "0xd69F9856A569B1655B43B0395b7c2923a217Cfe0"
TG = os.environ.get("TG_TOKEN", "")
CHAT = os.environ.get("TG_CHAT", "")
RPCS = ["https://mainnet.base.org","https://base.publicnode.com","https://base-mainnet.public.blastapi.io"]
ri = 0
V3_SWAP = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
V2_SWAP = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"

pool_tokens = {}
pair_pools = defaultdict(list)

def rpc():
    global ri; r=RPCS[ri%len(RPCS)]; ri+=1; return r

def cast(args):
    try:
        r=subprocess.run(["/root/.foundry/bin/cast"]+args,capture_output=True,text=True,timeout=10)
        return r.stdout.strip(),r.stderr.strip()
    except: return "","timeout"

def tg(msg):
    if not TG: return
    try:
        d=urllib.parse.urlencode({"chat_id":CHAT,"text":msg,"parse_mode":"Markdown"}).encode()
        urllib.request.urlopen(urllib.request.Request(f"https://api.telegram.org/bot{TG}/sendMessage",d),timeout=5)
    except: pass

def get_logs(frm, to):
    payload=json.dumps({"jsonrpc":"2.0","method":"eth_getLogs","params":[{
        "fromBlock":hex(frm),"toBlock":hex(to),
        "topics":[[V3_SWAP,V2_SWAP]]}],"id":1}).encode()
    try:
        req=urllib.request.Request(rpc(),data=payload,headers={"Content-Type":"application/json"})
        with urllib.request.urlopen(req,timeout=10) as resp:
            data=json.loads(resp.read())
            return data.get("result",[]) if "error" not in data else []
    except: return []

def get_block():
    o,_=cast(["block-number","--rpc-url",rpc()])
    try: return int(o)
    except: return 0

def discover_pool(addr):
    if addr in pool_tokens: return pool_tokens[addr]
    o,e=cast(["call",addr,"getReserves()(uint112,uint112,uint32)","--rpc-url",rpc()])
    is_v2 = bool(o and "error" not in (e or "").lower())
    o0,_=cast(["call",addr,"token0()(address)","--rpc-url",rpc()])
    o1,_=cast(["call",addr,"token1()(address)","--rpc-url",rpc()])
    if not o0 or not o1: return None
    t0=o0.strip().split()[0].lower()
    t1=o1.strip().split()[0].lower()
    pool_tokens[addr]=(t0,t1,is_v2)
    k=(min(t0,t1),max(t0,t1))
    if addr not in pair_pools[k]:
        pair_pools[k].append(addr)
    return (t0,t1,is_v2)

def sim(v2,sell,token,amt,v3):
    o,e=cast(["call",CONTRACT,"exec(address,address,address,uint256,bool)",
        v2,sell,token,str(amt),str(v3).lower(),"--from",WALLET,"--rpc-url",rpc()])
    return not(e and("revert"in e.lower()or"error"in e.lower()))

def exe(v2,sell,token,amt,v3):
    return cast(["send",CONTRACT,"exec(address,address,address,uint256,bool)",
        v2,sell,token,str(amt),str(v3).lower(),
        "--private-key",PK,"--rpc-url",rpc(),"--gas-limit","400000"])

print("="*50)
print("DYNAMIC HUNTER")
print("="*50)
tg("🔬 *Dynamic Hunter Started*\nDiscovers pools from live swaps\nArbs on-the-fly")

last_block=get_block()
cycle=0;found=0;executed=0

while True:
    cycle+=1
    current=get_block()
    if current<=last_block: time.sleep(2); continue

    logs=get_logs(last_block+1,current)
    last_block=current
    if not logs:
        if cycle%20==0:
            np=sum(1 for v in pair_pools.values() if len(v)>=2)
            print(f"[C{cycle}] blk {current} | pools={len(pool_tokens)} | arb_pairs={np} | found={found}")
        time.sleep(2); continue

    swapped=set(l["address"].lower() for l in logs)
    for addr in swapped:
        discover_pool(addr)

    for addr in swapped:
        if addr not in pool_tokens: continue
        t0,t1,is_v2=pool_tokens[addr]
        k=(min(t0,t1),max(t0,t1))
        sisters=[a for a in pair_pools[k] if a!=addr]
        if not sisters: continue

        for sister in sisters:
            if sister not in pool_tokens: continue
            _,_,sis_v2=pool_tokens[sister]
            if not is_v2 and not sis_v2: continue

            v2_pool=addr if is_v2 else sister
            other_pool=sister if is_v2 else addr
            other_v3=not pool_tokens[other_pool][2]

            o,_=cast(["call",v2_pool,"getReserves()(uint112,uint112,uint32)","--rpc-url",rpc()])
            if not o: continue
            lines=o.split("\n")
            try: r0,r1=int(lines[0].split()[0]),int(lines[1].split()[0])
            except: continue
            if r0<1000000 or r1<1000000: continue

            o2,_=cast(["call",v2_pool,"token0()(address)","--rpc-url",rpc()])
            v2_t0=o2.strip().split()[0].lower() if o2 else ""

            for bt in [t0,t1]:
                res=r0 if v2_t0==bt else r1
                if res<1000000: continue
                for pct in [3,1]:
                    amt=res*pct//100
                    if amt<100000: continue
                    if sim(v2_pool,other_pool,bt,amt,other_v3):
                        found+=1
                        msg=f"🎯 *ARB #{found}!*\n`{bt[:14]}..`\nV2→{'V3'if other_v3 else'V2'}\nAmt:{amt}"
                        print(msg.replace("*","").replace("`",""))
                        tg(msg)
                        o,e=exe(v2_pool,other_pool,bt,amt,other_v3)
                        if o and "status               1" in o:
                            executed+=1
                            tx=[l.split()[-1]for l in o.split("\n")if"transactionHash"in l]
                            tg(f"✅ *WIN #{executed}!* `{tx[0]if tx else'?'}`")
                            print(f"WIN: {tx}")
                        break
                else: continue
                break

    np=sum(1 for v in pair_pools.values() if len(v)>=2)
    if cycle%10==0:
        print(f"[C{cycle}] blk {current} | swaps={len(logs)} | pools={len(pool_tokens)} | arb_pairs={np} | found={found} | wins={executed}")
    if cycle%60==0:
        tg(f"📊 C{cycle} | pools={len(pool_tokens)} | arb_pairs={np} | found={found} | wins={executed}")
    time.sleep(2)
