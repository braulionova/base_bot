#!/bin/bash
# ============================================================
# KEEPER BOT: Calls keeperDirect/keeperTriangular on our contract
# Anyone with gas can run this and earn 10% of arb profits
# ============================================================
#
# Usage: Share this with anyone who has ETH on Base.
# They run it, our contract does the arb, they get 10% profit.
# We get 90% profit. Win-win.
#
# The main longtail-bot detects opportunities and writes them
# to /tmp/keeper-opps.json. This script reads and executes.
# ============================================================

echo "=== KEEPER BOT ==="
echo "Watching for opportunities from longtail-bot..."
echo "You pay gas, you get 10% of profit."
echo ""

CONTRACT="0xNEW_CONTRACT_ADDRESS"  # Update after deploy
RPC="${EXTRA_RPC_URLS%%,*}"  # Uses first RPC from env

while true; do
    if [ -f /tmp/keeper-opps.json ]; then
        # Read and execute opportunities
        cat /tmp/keeper-opps.json
        # cast send would go here with the keeper's private key
        rm -f /tmp/keeper-opps.json
    fi
    sleep 1
done
