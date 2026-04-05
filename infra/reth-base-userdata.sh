#!/bin/bash
set -euo pipefail
exec > >(tee /var/log/reth-setup.log) 2>&1

echo "=== RETH BASE NODE SETUP ==="
echo "Started: $(date)"

# ============================================================
# 1. System setup
# ============================================================
apt-get update && apt-get upgrade -y
apt-get install -y build-essential pkg-config libssl-dev \
  clang llvm curl git jq htop nvme-cli unzip

# ============================================================
# 2. Format and mount NVMe instance storage
# ============================================================
NVME_DEV=$(nvme list -o json | jq -r '.Devices[] | select(.ModelNumber | contains("Instance Storage") or contains("Amazon EC2 NVMe")) | .DevicePath' | head -1)
if [ -z "$NVME_DEV" ]; then
  # Fallback: find the non-root NVMe device
  NVME_DEV=$(lsblk -dpno NAME,TYPE | grep disk | grep -v "$(findmnt -n -o SOURCE / | sed 's/p[0-9]*$//')" | awk '{print $1}' | head -1)
fi

if [ -n "$NVME_DEV" ]; then
  echo "Formatting $NVME_DEV as ext4..."
  mkfs.ext4 -F "$NVME_DEV"
  mkdir -p /data
  mount "$NVME_DEV" /data
  echo "$NVME_DEV /data ext4 defaults,noatime,discard 0 0" >> /etc/fstab
  echo "NVMe mounted at /data ($(df -h /data | tail -1 | awk '{print $2}'))"
else
  echo "WARNING: No NVMe instance storage found, using root disk"
  mkdir -p /data
fi

mkdir -p /data/reth /data/op-node /data/jwt

# ============================================================
# 3. Install Rust
# ============================================================
su - ubuntu -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'

# ============================================================
# 4. Install reth (with OP Stack support)
# ============================================================
echo "Installing reth from binary..."
RETH_VERSION="v1.3.12"
curl -L "https://github.com/paradigmxyz/reth/releases/download/${RETH_VERSION}/reth-${RETH_VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
  -o /tmp/reth.tar.gz
tar -xzf /tmp/reth.tar.gz -C /usr/local/bin/
chmod +x /usr/local/bin/reth
reth --version
echo "reth installed"

# ============================================================
# 5. Install op-node
# ============================================================
echo "Installing op-node..."
OP_VERSION="v1.12.1"
curl -L "https://github.com/ethereum-optimism/optimism/releases/download/op-node/${OP_VERSION}/op-node-${OP_VERSION}-linux-amd64.tar.gz" \
  -o /tmp/op-node.tar.gz
tar -xzf /tmp/op-node.tar.gz -C /usr/local/bin/ op-node
chmod +x /usr/local/bin/op-node
op-node --version
echo "op-node installed"

# ============================================================
# 6. Generate JWT secret (shared between reth and op-node)
# ============================================================
openssl rand -hex 32 > /data/jwt/jwt.hex
chmod 600 /data/jwt/jwt.hex

# ============================================================
# 7. Create systemd services
# ============================================================

# L1 RPC for op-node derivation (Base needs L1 Ethereum data)
# User should set this to their own L1 RPC (Alchemy/Infura free tier works)
L1_RPC="${L1_RPC:-https://ethereum-rpc.publicnode.com}"

cat > /etc/systemd/system/reth-base.service << 'RETH_SERVICE'
[Unit]
Description=Reth Base Node (OP Stack)
After=network.target
Wants=network-online.target

[Service]
Type=simple
User=ubuntu
ExecStart=/usr/local/bin/reth node \
  --chain base \
  --datadir /data/reth \
  --http \
  --http.addr 0.0.0.0 \
  --http.port 8545 \
  --http.api eth,net,web3,debug,trace,txpool \
  --http.corsdomain "*" \
  --ws \
  --ws.addr 0.0.0.0 \
  --ws.port 8546 \
  --ws.api eth,net,web3,debug,trace,txpool \
  --authrpc.addr 127.0.0.1 \
  --authrpc.port 8551 \
  --authrpc.jwtsecret /data/jwt/jwt.hex \
  --metrics 0.0.0.0:9001 \
  --port 30303 \
  --max-outbound-peers 100 \
  --max-inbound-peers 50 \
  --full \
  --txpool.max-pending-txns 10000 \
  --txpool.max-new-txns 5000 \
  --log.file.directory /data/reth/logs
Restart=always
RestartSec=5
LimitNOFILE=65535
LimitNPROC=65535

[Install]
WantedBy=multi-user.target
RETH_SERVICE

cat > /etc/systemd/system/op-node-base.service << OPNODE_SERVICE
[Unit]
Description=OP Node for Base
After=reth-base.service
Requires=reth-base.service

[Service]
Type=simple
User=ubuntu
ExecStart=/usr/local/bin/op-node \
  --l1=${L1_RPC} \
  --l2=http://127.0.0.1:8551 \
  --l2.jwt-secret=/data/jwt/jwt.hex \
  --network=base-mainnet \
  --rpc.addr=0.0.0.0 \
  --rpc.port=9545 \
  --syncmode=execution-layer \
  --l1.beacon=https://ethereum-beacon-api.publicnode.com \
  --l1.trustrpc \
  --p2p.listen.tcp=9222 \
  --p2p.listen.udp=9222 \
  --log.format=text
Restart=always
RestartSec=5
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
OPNODE_SERVICE

# ============================================================
# 8. Set permissions and enable services
# ============================================================
chown -R ubuntu:ubuntu /data
systemctl daemon-reload
systemctl enable reth-base op-node-base

# ============================================================
# 9. Create helper scripts
# ============================================================
cat > /usr/local/bin/reth-status << 'STATUS'
#!/bin/bash
echo "=== RETH STATUS ==="
systemctl status reth-base --no-pager -l | head -15
echo ""
echo "=== OP-NODE STATUS ==="
systemctl status op-node-base --no-pager -l | head -15
echo ""
echo "=== SYNC STATUS ==="
curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}' | jq .
echo ""
echo "=== LATEST BLOCK ==="
curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' | jq -r '.result' | xargs printf "%d\n"
echo ""
echo "=== DISK USAGE ==="
du -sh /data/reth /data/op-node 2>/dev/null
df -h /data
echo ""
echo "=== PEER COUNT ==="
curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"net_peerCount","params":[],"id":1}' | jq -r '.result' | xargs printf "%d peers\n"
STATUS
chmod +x /usr/local/bin/reth-status

cat > /usr/local/bin/reth-start << 'START'
#!/bin/bash
echo "Starting reth + op-node..."
sudo systemctl start reth-base
sleep 3
sudo systemctl start op-node-base
echo "Started. Check status with: reth-status"
START
chmod +x /usr/local/bin/reth-start

cat > /usr/local/bin/reth-logs << 'LOGS'
#!/bin/bash
echo "=== RETH LOGS (last 50) ==="
journalctl -u reth-base -n 50 --no-pager
echo ""
echo "=== OP-NODE LOGS (last 50) ==="
journalctl -u op-node-base -n 50 --no-pager
LOGS
chmod +x /usr/local/bin/reth-logs

# ============================================================
# 10. Start services
# ============================================================
echo "Starting reth-base..."
systemctl start reth-base
sleep 5
echo "Starting op-node-base..."
systemctl start op-node-base

echo ""
echo "=== SETUP COMPLETE ==="
echo "reth-status  — check sync progress"
echo "reth-logs    — view logs"
echo "reth-start   — restart services"
echo ""
echo "RPC endpoint: http://<this-ip>:8545"
echo "WS endpoint:  ws://<this-ip>:8546"
echo ""
echo "Sync will take ~12-24 hours for full Base chain."
echo "Finished: $(date)"
