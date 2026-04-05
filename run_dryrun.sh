#!/bin/bash
cd /home/ubuntu/base_bot

# Load only TG vars for reporting, unset ARB_CONTRACT to force dry-run
export TG_TOKEN=$(grep TG_TOKEN .env | cut -d= -f2)
export TG_CHAT=$(grep TG_CHAT .env | cut -d= -f2)
unset ARB_CONTRACT
unset PRIVATE_KEY

# Log level: info for main bot, warn for noisy deps
export RUST_LOG="longtail_bot=info"

exec ./target/release/longtail-bot 2>&1 | tee -a /home/ubuntu/base_bot/dryrun.log
