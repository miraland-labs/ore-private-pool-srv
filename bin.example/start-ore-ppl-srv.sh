#!/usr/bin/env bash

# Start ore mining private pool server

set -e

SRV=$HOME/miner/ore-private-pool-srv/target/release/ore-ppl-srv

# MKP="$HOME/.config/solana/id.json"

# default dynamic fee url. Uncomment next line if you plan to enable dynamic-fee mode
# DYNAMIC_FEE_URL="YOUR_RPC_URL_HERE"

BUFFER_TIME=5
RISK_TIME=0
PRIORITY_FEE=1000
PRIORITY_FEE_CAP=100000

EXP_MIN_DIFF=8
MESSAGING_DIFF=25
XTR_FEE_DIFF=25
XTR_FEE_PCT=500

# The command you want to run

# CMD="$HOME/miner/ore-private-pool-srv/target/release/ore-ppl-srv \
#         --buffer-time 5 \
#         --dynamic-fee \
#         --send-tpu-mine-tx \
#         --dynamic-fee-url $DYNAMIC_FEE_URL \
#         --priority-fee 1000 \
#         --priority-fee-cap 100000 \
#         --expected-min-difficulty 8 \
#         --extra-fee-difficulty 25 \
#         --extra-fee-percent 500"

# bash -c "$CMD"

CMD="$SRV \
        --buffer-time $BUFFER_TIME \
        --risk-time $RISK_TIME \
        --priority-fee $PRIORITY_FEE \
        --priority-fee-cap $PRIORITY_FEE_CAP \
        --expected-min-difficulty $EXP_MIN_DIFF \
        --slack-difficulty $MESSAGING_DIFF \
        --extra-fee-difficulty $XTR_FEE_DIFF \
        --extra-fee-percent $XTR_FEE_PCT"

echo "$CMD"
until bash -c "$CMD"; do
    echo "Starting server command failed. Restart..."
    sleep 2
done
