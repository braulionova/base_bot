#!/bin/bash
set -euo pipefail
export PATH="$HOME/.foundry/bin:$PATH"

source .env

echo "=== Deploying UnifiedArb to Base ==="
echo "Wallet: 0xd69f9856a569b1655b43b0395b7c2923a217cfe0"

# Check balance
BAL=$(cast balance 0xd69f9856a569b1655b43b0395b7c2923a217cfe0 --rpc-url https://base.drpc.org --ether)
echo "Balance: $BAL ETH"

if [ "$BAL" = "0.000000000000000000" ]; then
    echo "ERROR: Wallet has 0 ETH. Send at least 0.001 ETH for deployment + operations."
    echo "  Address: 0xd69f9856a569b1655b43b0395b7c2923a217cfe0"
    echo "  Chain: Base (chainId 8453)"
    exit 1
fi

# Deploy
RESULT=$(forge create contracts/UnifiedArb.sol:UnifiedArb \
    --rpc-url https://base.drpc.org \
    --private-key "$PRIVATE_KEY" \
    --via-ir --optimizer-runs 200 \
    --broadcast 2>&1)

echo "$RESULT"

# Extract contract address
NEW_ADDR=$(echo "$RESULT" | grep "Deployed to:" | awk '{print $3}')

if [ -n "$NEW_ADDR" ]; then
    echo ""
    echo "=== Updating .env ==="
    sed -i "s|ARB_CONTRACT=.*|ARB_CONTRACT=$NEW_ADDR|" .env
    echo "ARB_CONTRACT updated to: $NEW_ADDR"
    echo ""
    echo "=== Done! Restart the bot to use the new contract ==="
else
    echo "ERROR: Could not extract contract address"
fi
