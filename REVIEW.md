# Repository review — 2026-09-05

Reviewed the application modules, tests, example configuration, CLI/setup paths, local Solidity fixture, CI/security configuration, and the latest Mintbot performance update (`5914122`). All concrete application findings listed below were addressed. Two upstream dependency maintenance notices remain, described separately.

## Fixed findings

| Priority | Finding | Resolution |
|---|---|---|
| P1 | [Untrusted auto-sell approval/transaction target](/Users/nepiy/workspace/auto-seller/src/autosell.rs:1675) | Require canonical Seaport 1.6, a supported exact ABI, zero native value, and a deployed protocol contract before approval. |
| P1 | [Currency symbols did not bind the payment asset](/Users/nepiy/workspace/auto-seller/src/autosell.rs:1615) | Bind each offer currency to a trusted ERC-20 address and compare API decimals with the contract. Ethereum WETH has a documented default; other assets need configuration. |
| P1 | [Fulfillment checks could overlook additional wallet assets](/Users/nepiy/workspace/auto-seller/src/autosell.rs:1827) | Validate every asset, exact NFT quantity, seller identity, payment token, conduit, and matching component. Reject duplicate, omitted, and self-routed components. |
| P1 | [Basic bid routes were confused with listing routes](/Users/nepiy/workspace/auto-seller/src/autosell.rs:1280) | Use Seaport routes 4/5 for selling ERC-721/ERC-1155 into ERC-20 bids; reject the previous 2/3 listing routes. |
| P1 | [Noncanonical ABI could encode a wrong selector](/Users/nepiy/workspace/auto-seller/src/autosell.rs:1652) | Validate full canonical signatures, including uint256 matching item indices. The API cannot select a similarly named function or override the recipient. |
| P1 | [Lost broadcast replies were reported as definite rejection](/Users/nepiy/workspace/auto-seller/src/rpc.rs:661) | Preserve the signed hash for transport, malformed-reply, task, and timeout ambiguity; distinguish explicit already-known RPC replies from unknown-transaction text. |
| P1 | [Unacknowledged replacements disappeared from monitoring](/Users/nepiy/workspace/auto-seller/src/bot.rs:1705) | Monitor an ambiguously submitted replacement hash alongside the original transaction. |
| P1 | [Re-mined receipts inherited the old confirmation window](/Users/nepiy/workspace/auto-seller/src/bot.rs:1705) | Restart confirmation tracking when receipt block identity changes. Receipts missing inclusion data are not accepted. |
| P1 | [Cached nonce could replace another local process transaction](/Users/nepiy/workspace/auto-seller/src/bot.rs:1611) | Always reload the pending nonce under the wallet lock. Keep the lock through mint receipt monitoring and replacements; release it before auto-sell. |
| P2 | [ERC-1155 detection read the sender topic as recipient](/Users/nepiy/workspace/auto-seller/src/autosell.rs:1520) | Read topic 3 for ERC-1155 recipients and ignore zero-amount transfers. Retain the ERC-721 topic layout. |
| P2 | [Malformed batch logs could panic or loop excessively](/Users/nepiy/workspace/auto-seller/src/autosell.rs:1557) | Use checked index conversion/arithmetic, validate both array lengths, and bound iteration by the available payload. |
| P2 | [ABI widths and fee scaling could lose integer precision](/Users/nepiy/workspace/auto-seller/src/arithmetic.rs:6) | Reject values outside declared uint widths and scale fees/gas with integer arithmetic derived from the configured ratio. |
| P2 | [Rounding could approve a below-threshold sale](/Users/nepiy/workspace/auto-seller/src/autosell.rs:610) | Round costs and allocated mint expenses upward; round proceeds downward and validate payout accounting before threshold comparison. |
| P2 | [Auto-sell omitted Ink surcharge costs and approval budgets](/Users/nepiy/workspace/auto-seller/src/autosell.rs:771) | Include buffered Ink L1/operator fees in approval/sale estimates and historical fees in mint/approval cost basis. Check transaction gas caps and balance before sending. |
| P2 | [Receipt timeout handling could continue after uncertain spending](/Users/nepiy/workspace/auto-seller/src/autosell.rs:923) | Bound the entire receipt monitor, retry transient reads, and return an unknown outcome with its hash on timeout/interruption. Never swallow this outcome under the optional-price flag. |
| P2 | [Price clients could leak custom API headers through redirects](/Users/nepiy/workspace/auto-seller/src/pricing.rs:240) | Require secure remote price endpoints, attach keys only to exact secure provider hosts, disable redirects in price/OpenSea clients, and bound price response bodies. |
| P2 | [ETH/WETH snapshots could use inconsistent prices](/Users/nepiy/workspace/auto-seller/src/pricing.rs:71) | Resolve one shared ETH/WETH price per decision and reject conflicting explicit aliases. |
| P2 | [Configuration saving truncated files and followed symlinks](/Users/nepiy/workspace/auto-seller/src/config.rs:307) | Validate first, write an owner-only temporary file, sync, and atomically replace the destination. |
| P2 | [Trigger/manual retry paths could stall or exit early](/Users/nepiy/workspace/auto-seller/src/setup.rs:342) | Keep authenticated manual control available after a closed-mint retry; preserve retry outcomes after reconnect backfill; keep the earliest event confirmation candidate. |
| P2 | [Invalid view return types survived startup validation](/Users/nepiy/workspace/auto-seller/src/trigger.rs:38) | Require one matching bool/uint output and a numeric target that fits the declared width; validate triggers during config loading. |
| P3 | [Local control, setup, and shutdown edge cases](/Users/nepiy/workspace/auto-seller/src/wallet.rs:65) | Make lock waiting cancellable, stop setup on EOF, recognize IPv6 loopback RPC URLs, and avoid reporting “no transaction submitted” after stopping receipt monitoring. |
| P2 | [Normal/aggressive preparation serialized independent work](/Users/nepiy/workspace/auto-seller/src/bot.rs:480) | Overlap OpenSea, fee, balance, and nonce preparation; keep aggressive refreshes responsive and coalesced; add latency regressions, RPC profile selection, endpoint deduplication, and immediate latency reporting. |

Seaport target, routes, and interfaces were checked against the project’s [deployment documentation](https://github.com/ProjectOpenSea/seaport/blob/main/docs/Deployment.md), [order enums](https://github.com/ProjectOpenSea/seaport-types/blob/main/src/lib/ConsiderationEnums.sol), and [canonical interface](https://raw.githubusercontent.com/ProjectOpenSea/seaport-types/main/src/interfaces/ConsiderationInterface.sol).

## Validation

- Rust 1.94.1: **124 tests passed, zero failed, zero ignored**, including the opt-in Gitleaks regression and normal/aggressive latency regressions.
- `cargo +1.94.1 clippy --locked --all-targets --all-features -- -D warnings`: passed.
- `cargo +1.94.1 fmt --all --check`: passed.
- Gitleaks scan of application source, tests, CI files, public example environment, and project metadata: no leaks found. The real `.env` was not read or copied.
- Refreshed RustSec database: **no known vulnerabilities reported** in the 487-package lockfile. Unused Alloy full features were removed, reducing the lockfile from 499 packages.
- `PERFORMANCE.md` documents the controlled request-ordering comparison and the network-specific `rpc-test --chain-id` probes.

Key regressions exercise real local HTTP mock servers for ambiguous broadcasts, replacement receipts, re-mining, and Ink fee queries; receipt fixtures test real ERC-721/ERC-1155 decoding. Mutated Seaport payloads exercise target, asset, amount, ABI, and fulfillment-component rejection.

## Configuration changes

Auto-sell now accepts canonical Seaport 1.6 only. Configure `auto_sell.currency_token_addresses` for offer assets on the selected chain; only Ethereum WETH has a built-in trusted address. The setup wizard now asks for the offer token. See [auto-sell configuration](/Users/nepiy/workspace/auto-seller/README.md).

All nonce strategies recheck the pending nonce under the wallet lock before signing. ETH/WETH explicit prices must agree. Known Polygon native payments default to POL. Invalid trigger output types and duplicate currency-map symbols are rejected during configuration validation.

## Remaining upstream notices and verification limits

- `paste 1.0.15`: Alloy’s compiled dependency graph uses an unmaintained proc-macro crate. [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436).
- `derivative 2.2.0`: an unmaintained transitive package remains in Cargo.lock; it was not present in the active target’s compiled dependency graph. [RUSTSEC-2024-0388](https://rustsec.org/advisories/RUSTSEC-2024-0388).

These are maintenance advisories, not reported exploitable vulnerabilities. Removing them requires upstream dependency migration or maintaining a fork; they were not hidden with audit exclusions or replaced with an unreviewed fork.

No live wallet transactions or live OpenSea sales were executed. Custom L2 surcharge models other than Ink remain outside the fee estimator. Mock tests cannot establish live contract/API compatibility or guarantee inclusion-time fees.

The repository now includes the Mintbot performance update in the current Git working tree. A reviewable patch against the pre-review local files is available at [auto-seller-review.patch](/tmp/auto-seller-review.patch).
