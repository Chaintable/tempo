# Generic writer v1.14 / T11 integration

2026-09-08. Base: `origin/debank=24340a9907046f8cb70197e8faa3e39867809fa2`, upstream v1.14.0 `1ec5653e39c97e02807dddf5e7106e979f3ad909`. Existing PR #10 uses a normal merge; no history rewrite.

## Changes

Resolved 12 merge conflicts. Formal upstream protocol code wins where PR #10 had no independent edits. Retained the full block executor, native inspector, receipt/gas checks, canonical changeset verification and trace-module registration from PR #10. Reth's removed `StateProviderTraitObjWrapper` is replaced with the boxed provider directly; no per-transaction replay fallback is introduced.

Relative to `origin/debank`, Cargo.lock adds only six dependency edges to debank-rpc. Protocol package versions remain those of v1.14.0.

## Local validation

Rust/Cargo 1.96.1 on macOS arm64; offline locked checks/builds. Raw evidence: `target/v114-validation/`.

- Workspace check passed (3m10s); binary build passed (3m56s).
- debank-rpc32 + consensus290 + evm101 + precompiles1006 = 1429 passed, 0 failed, 1 pre-existing upstream ignored.
- Separate T10 and pure T11 dev genesis configurations, both with T12 disabled, accepted the same signed TIP-20 transfer. Transaction gas: 291946 / 292018.
- `trace_debankBlock(1)` completed receipt/gas/canonical-diff checks on both nodes; each emitted two receipt events, one call trace and nonempty state-diff RLP. Raw requests and responses: `writer-smoke.json`.
- `decimals()` returned 6 under both forks; appending 32 bytes succeeded under T10 and reverted under T11. `pre_traceMany` gas: 271170 / 271194, exactly +24.
- Temporary nodes were stopped. No remote or production services were modified.

## Remaining acceptance

This is local integration evidence, not full pipeline acceptance. CI/images, independent oracle differential tests, database compatibility rehearsal, historical/live pipeline and a fresh fixed-image 24-hour run remain required. Real mainnet T11 boundary is pending activation.

User-deferred native storage flags remain unchanged: dynamic Tempo precompiles are absent from the inspector's static address set, so the sample transfer reports false trace storage flags despite correct canonical state diff. A temporary fix was withdrawn after checking the exclusions; its test/build logs are not final-candidate evidence. The Leafage storage-wipe consumer, N−1/N initial-state convention and other explicitly deferred trace/cancellation items remain excluded.
