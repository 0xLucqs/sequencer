use std::fs;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::Mutex;

use apollo_infra_utils::path::resolve_project_relative_path;
use cairo_vm::vm::runners::cairo_pie::CairoPie;
use privacy_circuit_verify::{verify_recursive_circuit, PrivacyProofOutput};
use privacy_prove::{prepare_recursive_prover_precomputes, RecursiveProverPrecomputes};
use starknet_api::transaction::fields::VIRTUAL_SNOS;
use starknet_proof_verifier::ProgramOutput;

use crate::proving::prover::prove;

/// Test resource file names.
const CAIRO_PIE_FILE: &str = "cairo_pie_10_transfers.zip";
const EXPECTED_PROOF_FACTS_FILE: &str = "proof_facts_10_transfers.json";

static MEMORY_MODE_ENV_LOCK: Mutex<()> = Mutex::new(());

fn resolve_resource_path(file_name: &str) -> std::path::PathBuf {
    let path: std::path::PathBuf =
        ["crates", "starknet_transaction_prover", "resources", file_name].iter().collect();
    resolve_project_relative_path(&path.to_string_lossy())
        .unwrap_or_else(|_| panic!("Failed to resolve path for {file_name}"))
}

fn prepare_precomputes() -> Arc<RecursiveProverPrecomputes> {
    prepare_recursive_prover_precomputes().expect("Failed to prepare precomputes")
}

struct MemoryModeGuard {
    previous: Option<OsString>,
}

impl Drop for MemoryModeGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            unsafe { std::env::set_var("STWO_PROVER_MEMORY_MODE", previous) };
        } else {
            unsafe { std::env::remove_var("STWO_PROVER_MEMORY_MODE") };
        }
    }
}

fn set_memory_mode(value: &str) -> MemoryModeGuard {
    let previous = std::env::var_os("STWO_PROVER_MEMORY_MODE");
    unsafe { std::env::set_var("STWO_PROVER_MEMORY_MODE", value) };
    MemoryModeGuard { previous }
}

/// Integration test that verifies proving works with a real Cairo PIE.
///
/// Run with:
/// ```shell
/// rustup run nightly-2025-07-14 cargo test -p starknet_transaction_prover --release --features \
///     stwo_proving test_prove_cairo_pie_10_transfers
/// ```
#[tokio::test]
async fn test_prove_cairo_pie_10_transfers() {
    let cairo_pie_path = resolve_resource_path(CAIRO_PIE_FILE);
    let expected_program_output_path = resolve_resource_path(EXPECTED_PROOF_FACTS_FILE);

    // Read CairoPie from zip file.
    let cairo_pie =
        CairoPie::read_zip_file(&cairo_pie_path).expect("Failed to read Cairo PIE from zip file");

    // Prepare precomputes and prove the Cairo PIE.
    let precomputes = prepare_precomputes();
    let output = prove(cairo_pie, precomputes).await.expect("Failed to prove Cairo PIE");

    // Verify the proof using the circuit verifier.
    let output_preimage: Vec<starknet_types_core::felt::Felt> = output.program_output.0.to_vec();
    let proof_output = PrivacyProofOutput { proof: output.proof.0.to_vec(), output_preimage };
    verify_recursive_circuit(&proof_output).expect("Failed to verify proof");

    // Read expected program output.
    let expected_program_output_str = fs::read_to_string(&expected_program_output_path)
        .expect("Failed to read expected program output file");
    let expected_program_output: ProgramOutput = serde_json::from_str(&expected_program_output_str)
        .expect("Failed to parse expected program output");

    // Compare program output.
    assert_eq!(
        output.program_output, expected_program_output,
        "Generated program output does not match expected program output"
    );
}

#[tokio::test]
async fn test_recursive_proof_matches_low_memory_cairo_pie_10_transfers() {
    let _env_lock = MEMORY_MODE_ENV_LOCK.lock().unwrap();
    let cairo_pie_path = resolve_resource_path(CAIRO_PIE_FILE);

    let fast_output = {
        let _guard = set_memory_mode("fast");
        let cairo_pie =
            CairoPie::read_zip_file(&cairo_pie_path).expect("Failed to read Cairo PIE from zip file");
        let precomputes = prepare_precomputes();
        prove(cairo_pie, precomputes)
            .await
            .expect("Failed to prove Cairo PIE in fast mode")
    };

    let low_memory_output = {
        let _guard = set_memory_mode("low_memory");
        let cairo_pie =
            CairoPie::read_zip_file(&cairo_pie_path).expect("Failed to read Cairo PIE from zip file");
        let precomputes = prepare_precomputes();
        prove(cairo_pie, precomputes)
            .await
            .expect("Failed to prove Cairo PIE in low-memory mode")
    };

    assert_eq!(
        fast_output.proof.0, low_memory_output.proof.0,
        "recursive proof bytes differ between fast and low-memory modes"
    );
    assert_eq!(
        fast_output.program_output, low_memory_output.program_output,
        "program output differs between fast and low-memory modes"
    );
}

/// Regenerates the example proof fixtures used by `apollo_transaction_converter` tests.
///
/// Run manually with:
/// ```bash
/// cargo test -p starknet_transaction_prover --features stwo_proving -- --ignored regenerate_proof_fixtures
/// ```
#[tokio::test]
#[ignore]
async fn regenerate_proof_fixtures() {
    let cairo_pie_path = resolve_resource_path(CAIRO_PIE_FILE);
    let cairo_pie =
        CairoPie::read_zip_file(&cairo_pie_path).expect("Failed to read Cairo PIE from zip file");

    let precomputes = prepare_precomputes();
    let output = prove(cairo_pie, precomputes).await.expect("Failed to prove Cairo PIE");

    // Save proof as raw binary.
    let raw_bytes: Vec<u8> = output.proof.0.to_vec();
    let proof_path = resolve_transaction_converter_resource("example_proof.bin");
    fs::write(&proof_path, &raw_bytes).expect("Failed to write proof file");
    println!("Wrote proof to {}", proof_path.display());

    // Save proof facts as JSON.
    let proof_facts = output
        .program_output
        .try_into_proof_facts(VIRTUAL_SNOS)
        .expect("Failed to convert program output to proof facts");
    let proof_facts_json =
        serde_json::to_string_pretty(&proof_facts).expect("Failed to serialize proof facts");
    let proof_facts_path = resolve_transaction_converter_resource("example_proof_facts.json");
    fs::write(&proof_facts_path, proof_facts_json).expect("Failed to write proof facts file");
    println!("Wrote proof facts to {}", proof_facts_path.display());
}

fn resolve_transaction_converter_resource(file_name: &str) -> std::path::PathBuf {
    let relative_path: std::path::PathBuf =
        ["crates", "apollo_transaction_converter", "resources", file_name].iter().collect();
    resolve_project_relative_path(&relative_path.to_string_lossy())
        .unwrap_or_else(|_| panic!("Failed to resolve path for {file_name}"))
}
