#!/usr/bin/env bash
# Fetches fresh Fynd calldata fixtures for the BackrunResolver fork tests.
# Run: bash contracts/script/GetCalldata.sh

set -euo pipefail

FYND_API="https://fynd-test.propellerheads.xyz/v1/quote"
# This must match the resolver address computed in setUp() of the fork test.
RESOLVER="0x5FbDB2315678afecb367f032d93F642f64180aa3"
OUT_DIR="$(dirname "$0")/../test/fixtures"

echo "Fetching WETH→USDC calldata (receiver=$RESOLVER)..."
WETH_USDC=$(curl -sf -X POST "$FYND_API" \
  -H "Content-Type: application/json" \
  -d "{
    \"orders\": [{
      \"token_in\": \"0xC02aAA39b223FE8D0A0e5C4F27eAD9083C756Cc2\",
      \"token_out\": \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\",
      \"amount\": \"1000000000000000000\",
      \"side\": \"sell\",
      \"sender\": \"$RESOLVER\"
    }],
    \"options\": {
      \"timeout_ms\": 5000,
      \"encoding_options\": { \"slippage\": \"0.005\" }
    }
  }")

echo "$WETH_USDC" | python3 -c "
import json,sys
r=json.load(sys.stdin)
o=r['orders'][0]
print('to:', o['transaction']['to'])
print('amountOut:', o['amount_out'])
print('minAmountOut (in calldata):', o['fee_breakdown']['min_amount_received'])
block=o.get('block',{}).get('number','unknown')
print('blockNumber:', block)
data=o['transaction']['data']
with open('$OUT_DIR/weth_usdc_calldata.hex','w') as f:
    f.write(data)
print('Written $OUT_DIR/weth_usdc_calldata.hex')
"

# Amount for surplus calldata: 1% of expected output as placeholder.
# The resolver patches the actual surplus amount at runtime.
SURPLUS_USDC=$(echo "$WETH_USDC" | python3 -c "
import json,sys; r=json.load(sys.stdin); print(int(r['orders'][0]['amount_out'])//100)")

echo "Fetching USDC→WETH surplus calldata (amount=$SURPLUS_USDC, receiver=$RESOLVER)..."
USDC_WETH=$(curl -sf -X POST "$FYND_API" \
  -H "Content-Type: application/json" \
  -d "{
    \"orders\": [{
      \"token_in\": \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\",
      \"token_out\": \"0xC02aAA39b223FE8D0A0e5C4F27eAD9083C756Cc2\",
      \"amount\": \"$SURPLUS_USDC\",
      \"side\": \"sell\",
      \"sender\": \"$RESOLVER\"
    }],
    \"options\": {
      \"timeout_ms\": 5000,
      \"encoding_options\": { \"slippage\": \"0.005\" }
    }
  }")

echo "$USDC_WETH" | python3 -c "
import json,sys
r=json.load(sys.stdin)
data=r['orders'][0]['transaction']['data']
with open('$OUT_DIR/usdc_weth_calldata.hex','w') as f:
    f.write(data)
print('Written $OUT_DIR/usdc_weth_calldata.hex')
"

echo "Done. Fixtures written to $OUT_DIR/"
