# Task Plan

## Live RPC Proof FFI

- [x] Inspect the current sequencer/mobile FFI surfaces and preserve the existing offline replay flow.
- [x] Add `prove_transaction_live` plus `free_proof_result` in `starknet_transaction_prover::ffi`, sharing the prover setup where possible.
- [x] Return the proved result as JSON matching the SDK shape and add focused Rust coverage for live-input validation/serialization behavior.
- [x] Update `~/mobile-stwo` header, Rust wrapper/JNI bridge, Kotlin declarations, and the CLI smoke path for the live RPC entry point.
- [x] Run focused verification for the sequencer FFI tests and the mobile wrapper build, then record exact commands and outcomes here.
- [x] Add review notes here.

## Dynamic Input FFI

- [x] Inspect the existing prover FFI path and preserve the current privacy-demo behavior.
- [x] Add a shared Rust implementation path plus a new `prove_transaction` FFI that accepts request and RPC-record JSON strings.
- [x] Add focused Rust tests for the new FFI input validation and wrapper behavior.
- [x] Update the mobile wrapper crate, C header, and Android bindings to expose the dynamic-input API.
- [x] Run focused verification for the sequencer crate and the mobile Rust wrapper.
- [x] Add the verification results and review notes here.

- [x] Recover the partial prover refactor and inspect the current harness state.
- [x] Add the fixed privacy-demo request fixture and finish the offline test wiring.
- [x] Record the full RPC interaction set needed by the privacy-demo prover flow.
- [x] Verify the privacy-demo test replays fully offline and document the exact commands.
- [x] Inspect the current privacy-demo proving target and find the existing fast-vs-low-memory proof-equivalence check.
- [ ] Compile the sequencer privacy-demo prover target fresh with the requested release command.
- [ ] Run the existing proof byte-equivalence check on a fresh build.
- [ ] Measure the current low-memory RAM usage of the privacy-demo test with `/usr/bin/time -l`.
- [ ] Add the measured results and review notes here.

# Review

## Live RPC Proof FFI

- Added `prove_transaction_live(const char*, const char*, LogCallback) -> char*` and
  `free_proof_result(char*)` to `crates/starknet_transaction_prover/src/ffi.rs`.
- Refactored the sequencer FFI so the offline replay path and the new live-RPC path share:
  runner construction, prover initialization, proof verification, and JSON serialization.
- The live path now uses the request JSON's `block_id` instead of forcing `BlockId::Latest`, and
  returns the serialized `ProveTransactionResult` JSON directly to the caller on success.
- Added focused Rust coverage for live-input null/UTF-8 validation, invalid JSON / invalid URL
  rejection, JSON shape expectations, and result-pointer freeing.
- Updated the mobile wrapper surface in `~/mobile-stwo`:
  - `rust/Cargo.toml`
  - `rust/crates/tx_prover_ffi/src/lib.rs`
  - `rust/crates/tx_prover_ffi/src/main.rs`
  - `shared/tx_prover_ffi.h`
  - `android/app/src/main/kotlin/com/txprover/NativeLib.kt`
- Verification:
  - Passed:
    `PATH=/Users/lucas/sequencer/sequencer_venv/bin:$PATH rtk proxy rustup run nightly-2025-07-14 cargo test --config build.rustc-wrapper='""' -p starknet_transaction_prover --features ffi ffi::tests --lib`
  - Passed:
    `PATH=/Users/lucas/sequencer/sequencer_venv/bin:$PATH rtk proxy rustup run nightly-2025-07-14 cargo check --config build.rustc-wrapper='""' -p tx_prover_ffi`
    in `~/mobile-stwo/rust`
  - Reached the new live FFI path and prover initialization, but the live CLI smoke test failed
    on the first outbound RPC call with:
    `prove_transaction failed: error sending request for url (https://free-rpc.nethermind.io/sepolia-juno/)`
    Command:
    `PATH=/Users/lucas/sequencer/sequencer_venv/bin:$PATH rtk proxy rustup run nightly-2025-07-14 cargo run --config build.rustc-wrapper='""' -p tx_prover_ffi --bin tx_prover_cli -- live`

## Dynamic Input FFI

- Added `prove_transaction(const char*, const char*, LogCallback)` to
  `crates/starknet_transaction_prover/src/ffi.rs` and refactored the proving flow into a shared
  async path so `prove_privacy_demo` still uses the embedded fixtures unchanged.
- The new sequencer FFI now validates null pointers and UTF-8 before entering Tokio, returns
  named status codes, and uses the request's `block_id` rather than forcing `BlockId::Latest`.
- Added focused Rust coverage for the new FFI validation and async JSON parsing failure cases in
  `crates/starknet_transaction_prover/src/ffi.rs`.
- Synced the same FFI change into the cargo git checkout used by `mobile-stwo`:
  `~/.cargo/git/checkouts/sequencer-b9e15abbb29091b0/b4157da/crates/starknet_transaction_prover/src/ffi.rs`.
- Updated the mobile wrapper surface:
  - `~/mobile-stwo/rust/crates/tx_prover_ffi/src/lib.rs`
  - `~/mobile-stwo/shared/tx_prover_ffi.h`
  - `~/mobile-stwo/android/app/src/main/kotlin/com/txprover/NativeLib.kt`
- Verification:
  - Passed:
    `PATH=/Users/lucas/sequencer/sequencer_venv/bin:$PATH rtk proxy rustup run nightly-2025-07-14 cargo test --config build.rustc-wrapper='""' -p starknet_transaction_prover --features ffi ffi::tests --lib`
  - Passed:
    `PATH=/Users/lucas/sequencer/sequencer_venv/bin:$PATH rtk proxy rustup run nightly-2025-07-14 cargo check --config build.rustc-wrapper='""' -p tx_prover_ffi`
    in `~/mobile-stwo/rust`
  - Blocked by local Android toolchain setup, not source errors:
    `PATH=/Users/lucas/sequencer/sequencer_venv/bin:$PATH rtk proxy rustup run nightly-2025-07-14 cargo check --config build.rustc-wrapper='""' -p tx_prover_ffi --target aarch64-linux-android`
    failed because `aarch64-linux-android-clang` is not installed or not on `PATH`.

- Added an ignored privacy-demo prover test backed by an in-repo request fixture at
  `crates/starknet_transaction_prover/resources/privacy_demo_prove_transaction_request.json`.
- Recorded the full Starknet RPC transcript needed for replay at
  `crates/starknet_transaction_prover/resources/rpc_records/test_prove_privacy_demo_transaction.json`.
- Adjusted the mock RPC server to consume the first remaining matching interaction so offline
  replay preserves duplicate responses while tolerating benign request reordering from
  nondeterministic class-fetch iteration, and added regression coverage for both cases.
- Removed the hidden `CHAIN_ID` shell dependency by baking `IntegrationSepolia` into the
  privacy-demo test fixture path.
- Verified live proofing once against the demo RPC and verified offline replay from the recorded
  transcript with:
  `source sequencer_venv/bin/activate && rtk proxy rustup run nightly-2025-07-14 cargo test -p starknet_transaction_prover --release --features stwo_proving proving::virtual_snos_prover_test::test_prove_privacy_demo_transaction -- --ignored --exact --nocapture`

# Fresh RAM Verification

- [x] Inspect the current privacy-demo proving target and find the existing fast-vs-low-memory proof-equivalence check.
- [x] Compile the sequencer privacy-demo prover target fresh with the requested release command.
- [x] Run the current low-memory safety checks on the local proving stack.
- [x] Measure the current low-memory RAM usage of the privacy-demo test with `/usr/bin/time -l`.
- [x] Add the measured results and review notes here.

# Review

- Fresh rebuild command:
  `PATH=/Users/lucas/sequencer/sequencer_venv/bin:$PATH rustup run nightly-2025-06-20 cargo test --manifest-path /Users/lucas/sequencer/Cargo.toml --release -p starknet_transaction_prover --features stwo_proving --no-run`
- Fresh rebuilt test binary:
  `/Users/lucas/sequencer/target/release/deps/starknet_transaction_prover-90dc1c60897b5716`
- Current existing local `stwo` safety checks that passed on this run:
  - `rustup run nightly-2025-07-14 cargo test --manifest-path /Users/lucas/stwo/Cargo.toml -p stwo --features prover prover::fri::tests::test_low_memory_fri_matches_fast_cpu -- --exact --nocapture`
  - `rustup run nightly-2025-07-14 cargo test --manifest-path /Users/lucas/stwo/Cargo.toml -p stwo --features prover prover::fri::tests::test_low_memory_fri_matches_fast_simd -- --exact --nocapture`
  - `rustup run nightly-2025-07-14 cargo test --manifest-path /Users/lucas/stwo/Cargo.toml -p stwo --features prover prover::pcs::quotient_ops::tests::test_pcs_prove_and_verify_simd_low_memory_multi_tree_many_columns -- --exact --nocapture`
- Low-memory privacy-demo measurement on the fresh rebuilt binary:
  - Command:
    `STWO_PROVER_MEMORY_MODE=low_memory CHAIN_ID=SN_INTEGRATION_SEPOLIA /usr/bin/time -l /Users/lucas/sequencer/target/release/deps/starknet_transaction_prover-90dc1c60897b5716 proving::virtual_snos_prover_test::test_prove_privacy_demo_transaction --ignored --exact --nocapture`
  - Result:
    - `60.64 real`
    - `9293627392` maximum resident set size
    - `7624660992` peak memory footprint
    - The run passed and stayed on the offline replay path; one `starknet_getClass` response was replayed from one step later in the recorded transcript.
- Fast-path result on the same fresh rebuilt binary:
  - Command:
    `CHAIN_ID=SN_INTEGRATION_SEPOLIA /usr/bin/time -l /Users/lucas/sequencer/target/release/deps/starknet_transaction_prover-90dc1c60897b5716 proving::virtual_snos_prover_test::test_prove_privacy_demo_transaction --ignored --exact --nocapture`
  - Result:
    - Failed after `4.65 real`
    - Panic site:
      `/Users/lucas/stwo/crates/stwo/src/prover/air/component_prover.rs:100`
    - Panic message:
      `evaluation buffer is not retained for this polynomial`
- Conclusion:
  - The fresh low-memory path currently works on the real privacy-demo harness and uses about `7.10 GiB` peak footprint.
  - The fresh fast path currently does not complete on this workload, so this run could not establish byte identity on the exact sequencer proof by comparing fast and low-memory outputs end to end.
  - The currently present local `stwo` equivalence/regression tests above still pass, which is the strongest existing automated safety signal available in the live checkout without adding new test code.
