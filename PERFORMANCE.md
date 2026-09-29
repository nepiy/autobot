# Mint execution latency — September 5, 2026

Auto Seller retains Mintbot's normal and aggressive execution improvements while
preserving the post-mint auto-sell checks. These changes reduce preparation time;
they do not promise earlier blockchain inclusion.

## Changes

- Normal mode starts the OpenSea build, fresh fees, balance, and just-in-time
  nonce lookup concurrently. Gas simulation still uses validated calldata and
  refreshed fees, with the fresh balance used for the final budget check.
- Aggressive mode keeps block and event monitoring responsive while one cache
  refresh is in flight. A trigger overlaps refresh completion with the OpenSea
  build, and failed refreshes cannot authorize signing with an unhealthy cache.
- Nonce selection remains protected by a cross-process wallet lock held through
  broadcast acknowledgement.
- Final bytecode-pin and Ink surcharge checks run concurrently before signing.
- Identical HTTP RPC URLs are deduplicated while distinct paths and queries stay
  independent.
- Latency is printed immediately after RPC acknowledgement, including the
  observed-trigger-to-send interval.

No payment caps, gas limits, provider credentials, or local `.env` values were
changed.

## Controlled preparation comparison

The latency regression test uses local mock services with 100 ms operations. It
compares the old sequential ordering with the optimized concurrent helpers and
does not sign, broadcast, or contact OpenSea.

```bash
cargo +1.94.1 test --locked --lib mocked_opensea_latency_comparison -- --nocapture
```

These measurements exclude real OpenSea variability, signing, propagation, and
block inclusion. A warm aggressive cache has no refresh delay to remove.

The `rpc-test --chain-id 4663`, `57073`, or `999` command benchmarks the same
network-specific RPC profile used by a mint. It performs read-only chain ID,
block-number, balance, and subscription samples.

## Validation

The latency regressions cover concurrent request preparation, responsive block
monitoring, cache cancellation and recovery, stage invalidation, gas/balance
checks, nonce contention, lock release after failure, and endpoint deduplication.
