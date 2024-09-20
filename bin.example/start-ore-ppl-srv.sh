#!/usr/bin/env bash

# Start ore mining private pool server

set -e

SRV=$HOME/miner/ore-private-pool-srv/target/release/ore-ppl-srv

# MKP="$HOME/.config/solana/id.json"

# default dynamic fee url. Uncomment next line if you plan to enable dynamic-fee mode
# DYNAMIC_FEE_URL="YOUR_RPC_URL_HERE"

BUFFER_TIME=5
RISK_TIME=0
PRIORITY_FEE=100
PRIORITY_FEE_CAP=10000

EXP_MIN_DIFF=8
SLACK_DIFF=25
XTR_FEE_DIFF=29
XTR_FEE_PCT=100

# The command you want to run

# CMD="$HOME/miner/ore-private-pool-srv/target/release/ore-ppl-srv \
#         --buffer-time 5 \
#         --dynamic-fee \
#         --send-tpu-mine-tx \
#         --dynamic-fee-url $DYNAMIC_FEE_URL \
#         --priority-fee 100 \
#         --priority-fee-cap 10000 \
#         --expected-min-difficulty 8 \
#         --extra-fee-difficulty 29 \
#         --extra-fee-percent 100"

# bash -c "$CMD"

CMD="$SRV \
        --buffer-time $BUFFER_TIME \
        --risk-time $RISK_TIME \
        --priority-fee $PRIORITY_FEE \
        --priority-fee-cap $PRIORITY_FEE_CAP \
        --expected-min-difficulty $EXP_MIN_DIFF \
        --slack-difficulty $SLACK_DIFF \
        --extra-fee-difficulty $XTR_FEE_DIFF \
        --extra-fee-percent $XTR_FEE_PCT"

echo $CMD
until bash -c "$CMD"; do
    echo "Starting server command failed. Restart..."
    sleep 2
done
