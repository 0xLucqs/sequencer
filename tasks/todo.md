# Task Plan

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
