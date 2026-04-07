//! C-compatible FFI entry point for the privacy demo prover.
//!
//! Embeds test resources at compile time so the prover can run on iOS without
//! filesystem path resolution.

use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicPtr, Ordering};

use blockifier::blockifier::config::ContractClassManagerConfig;
use blockifier::state::contract_class_manager::ContractClassManager;
use blockifier_reexecution::state_reader::rpc_objects::BlockId;
use blockifier_reexecution::utils::get_chain_info;
use serde::Deserialize;
use starknet_api::core::ChainId;
use starknet_proof_verifier::verify_proof;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;
use url::Url;

use crate::proving::virtual_snos_prover::VirtualSnosProver;
use crate::running::rpc_records::{MockRpcServer, RpcRecords};
use crate::running::runner::{RpcRunnerFactory, RunnerConfig};
use crate::running::storage_proofs::StorageProofConfig;
use crate::running::virtual_block_executor::RpcVirtualBlockExecutorConfig;

/// Embedded test request (privacy demo transaction).
const REQUEST_JSON: &str =
    include_str!("../resources/privacy_demo_prove_transaction_request.json");

/// Embedded RPC records for offline replay.
const RPC_RECORDS_JSON: &str =
    include_str!("../resources/rpc_records/test_prove_privacy_demo_transaction.json");

/// C callback type for streaming log messages to the host (Swift).
pub type LogCallback = extern "C" fn(*const c_char);

fn send_log(cb: LogCallback, msg: &str) {
    if let Ok(c_str) = CString::new(msg) {
        cb(c_str.as_ptr());
    }
}

// --- Tracing bridge: forwards tracing events to the FFI callback ---

/// Global callback pointer shared with the tracing writer.
static GLOBAL_CB: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

fn set_global_cb(cb: LogCallback) {
    GLOBAL_CB.store(cb as *mut (), Ordering::Release);
}

/// Writer that sends each line to the FFI callback.
struct CallbackWriter;

impl std::io::Write for CallbackWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let ptr = GLOBAL_CB.load(Ordering::Acquire);
        if !ptr.is_null() {
            let cb: LogCallback = unsafe { std::mem::transmute(ptr) };
            if let Ok(s) = std::str::from_utf8(buf) {
                let trimmed = s.trim_end_matches('\n');
                if !trimmed.is_empty() {
                    send_log(cb, trimmed);
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// MakeWriter impl so tracing-subscriber can create writers per-event.
struct CallbackMakeWriter;

impl<'a> MakeWriter<'a> for CallbackMakeWriter {
    type Writer = CallbackWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CallbackWriter
    }
}

fn install_tracing(cb: LogCallback) {
    set_global_cb(cb);
    let filter = EnvFilter::try_new("info")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(CallbackMakeWriter)
        .with_target(true)
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .finish();
    // Ignore error if a subscriber is already set (e.g. re-entry).
    let _ = tracing::subscriber::set_global_default(subscriber);
}

#[derive(Deserialize)]
struct ProveTransactionRequest {
    params: ProveTransactionParams,
}

#[derive(Deserialize)]
struct ProveTransactionParams {
    #[allow(dead_code)]
    block_id: BlockId,
    transaction: starknet_api::rpc_transaction::RpcTransaction,
}

/// Run the privacy demo prover end-to-end.
///
/// Returns 0 on success, non-zero on failure. Progress and errors are reported
/// through `cb`.
///
/// # Safety
///
/// `cb` must be a valid function pointer for the lifetime of this call.
#[no_mangle]
pub extern "C" fn prove_privacy_demo(cb: LogCallback) -> i32 {
    install_tracing(cb);

    // Use low-memory proving path (drops FRI intermediates, recomputes during decommit).
    std::env::set_var("STWO_PROVER_MEMORY_MODE", "low_memory");

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            send_log(cb, &format!("Failed to create tokio runtime: {e}"));
            return 1;
        }
    };

    rt.block_on(async { prove_privacy_demo_async(cb).await })
}

async fn prove_privacy_demo_async(cb: LogCallback) -> i32 {
    // --- Parse embedded resources ---
    send_log(cb, "Parsing embedded request JSON...");
    let request: ProveTransactionRequest = match serde_json::from_str(REQUEST_JSON) {
        Ok(r) => r,
        Err(e) => {
            send_log(cb, &format!("Failed to parse request JSON: {e}"));
            return 2;
        }
    };

    send_log(cb, "Parsing embedded RPC records (3.3 MB)...");
    let records: RpcRecords = match serde_json::from_str(RPC_RECORDS_JSON) {
        Ok(r) => r,
        Err(e) => {
            send_log(cb, &format!("Failed to parse RPC records: {e}"));
            return 3;
        }
    };

    // --- Start mock RPC server ---
    send_log(cb, "Starting mock RPC server for offline replay...");
    let server = MockRpcServer::new(&records).await;
    let rpc_url = server.url();
    send_log(cb, &format!("Mock server listening at {rpc_url}"));

    // --- Build runner factory (inlined from test_utils::runner_factory_with_chain_id) ---
    send_log(cb, "Building runner factory...");
    let rpc_url_parsed = match Url::parse(&rpc_url) {
        Ok(u) => u,
        Err(e) => {
            send_log(cb, &format!("Invalid RPC URL: {e}"));
            return 4;
        }
    };

    let contract_class_manager =
        ContractClassManager::start(ContractClassManagerConfig::default());
    let chain_id = ChainId::IntegrationSepolia;
    let chain_info = get_chain_info(&chain_id, None);
    let runner_config = RunnerConfig {
        storage_proof_config: StorageProofConfig { include_state_changes: true },
        virtual_block_executor_config: RpcVirtualBlockExecutorConfig {
            prefetch_state: false,
            ..Default::default()
        },
    };
    let factory =
        RpcRunnerFactory::new(rpc_url_parsed, chain_info, contract_class_manager, runner_config);

    // --- Create prover (prepares recursive prover precomputes) ---
    send_log(cb, "Initializing VirtualSnosProver (preparing precomputes, this may take a while)...");
    let prover = VirtualSnosProver::from_runner(factory);

    // --- Prove ---
    send_log(cb, "Running prove_transaction (OS execution + Stwo proving)...");
    let result = prover
        .prove_transaction(BlockId::Latest, request.params.transaction)
        .await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            send_log(cb, &format!("prove_transaction failed: {e}"));
            return 5;
        }
    };
    send_log(cb, "prove_transaction succeeded.");

    // --- Verify proof ---
    send_log(cb, "Verifying proof...");
    let proof_facts = output.proof_facts.clone();
    let proof = output.proof.clone();
    let verify_result = tokio::task::spawn_blocking(move || verify_proof(proof_facts, proof))
        .await;

    match verify_result {
        Ok(Ok(())) => {
            send_log(cb, "Proof verification succeeded!");
            0
        }
        Ok(Err(e)) => {
            send_log(cb, &format!("Proof verification failed: {e}"));
            6
        }
        Err(e) => {
            send_log(cb, &format!("Proof verification task panicked: {e}"));
            7
        }
    }
}
