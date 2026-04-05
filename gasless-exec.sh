#!/bin/bash
set -euo pipefail
# ============================================================
# GASLESS EXECUTION via ERC-2771 Meta-Transaction
# Uses Gelato Relay to submit tx without gas
# ============================================================
#
# How it works:
# 1. You sign the tx data with your private key (no gas needed)
# 2. Gelato Relay submits it to Base and pays the gas
# 3. The contract's profit pays Gelato back via __gelato fee
#
# Free tier: 10 sponsored txs per month
# After that: Gelato takes fee from tx value
# ============================================================

export PATH="$HOME/.foundry/bin:$PATH"
source /home/ubuntu/base_bot/.env

echo "=== Gasless Execution Setup ==="

# Option 1: Use cast to create signed tx, then relay via Gelato API
# Gelato Relay API: https://relay.gelato.digital

# Check if Gelato Relay is available for Base
GELATO_RELAY="https://relay.gelato.digital"
CHAIN_ID=8453

echo "Testing Gelato Relay for Base (chainId: $CHAIN_ID)..."
curl -s "$GELATO_RELAY/relays/v2/supported-chains" 2>/dev/null | python3 -c "
import json, sys
try:
    chains = json.load(sys.stdin)
    if '$CHAIN_ID' in [str(c) for c in chains.get('relays', [])]:
        print('✅ Base supported by Gelato Relay')
    else:
        print('Available chains:', chains)
except:
    print('Could not check Gelato')
"

echo ""
echo "=== Alternative: EIP-712 Signed Message ==="
echo "1. Bot detects opportunity"
echo "2. Signs EIP-712 message with arb params"
echo "3. Anyone with gas calls contract with the signed message"
echo "4. Contract verifies signature, executes arb"
echo ""
echo "This is the same pattern as Uniswap Permit2"
