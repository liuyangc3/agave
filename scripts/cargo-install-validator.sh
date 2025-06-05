#!/usr/bin/env bash


cargo build --profile release --bin agave-validator \
  --workspace \
  --exclude solana-accounts-bench --exclude solana-banking-bench --exclude agave-ledger-tool --exclude solana-bench-tps --exclude agave-store-tool --exclude agave-store-histogram --exclude agave-accounts-hash-cache-tool --exclude solana-dos


# copy to bin dir
cp target/release/agave-validator bin/
bin/agave-validator --version