//! C-compatible FFI entry points for mobile proving.
//!
//! Embeds test resources at compile time so the prover can run on iOS without
//! filesystem path resolution when using the offline demo path.

use std::ffi::{CStr, CString};
use std::future::Future;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicPtr, Ordering};

use blockifier::blockifier::config::ContractClassManagerConfig;
use blockifier::state::contract_class_manager::ContractClassManager;
use blockifier_reexecution::state_reader::rpc_objects::BlockId;
use blockifier_reexecution::utils::get_chain_info;
#[cfg(feature = "stwo_proving")]
use privacy_prove::{log_recursive_prover_mmap_stats, log_recursive_prover_vm_walk};
use serde::Deserialize;
use starknet_api::core::ChainId;
use starknet_api::rpc_transaction::RpcTransaction;
use starknet_proof_verifier::verify_proof;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;
use url::Url;

use crate::proving::virtual_snos_prover::{ProveTransactionResult, VirtualSnosProver};
use crate::running::rpc_records::{MockRpcServer, RpcRecords};
use crate::running::runner::{RpcRunnerFactory, RunnerConfig, VirtualSnosRunner};
use crate::running::storage_proofs::StorageProofConfig;
use crate::running::virtual_block_executor::RpcVirtualBlockExecutorConfig;

/// Embedded test request (privacy demo transaction).
const REQUEST_JSON: &str = include_str!("../resources/privacy_demo_prove_transaction_request.json");

/// Embedded RPC records for offline replay.
const RPC_RECORDS_JSON: &str =
    include_str!("../resources/rpc_records/test_prove_privacy_demo_transaction.json");

/// C callback type for streaming log messages to the host (Swift/Kotlin).
pub type LogCallback = extern "C" fn(*const c_char);

const STATUS_OK: i32 = 0;
const STATUS_RUNTIME_ERROR: i32 = 1;
const STATUS_REQUEST_JSON_ERROR: i32 = 2;
const STATUS_RPC_RECORDS_JSON_ERROR: i32 = 3;
const STATUS_RPC_URL_ERROR: i32 = 4;
const STATUS_PROVE_TRANSACTION_ERROR: i32 = 5;
const STATUS_PROOF_VERIFICATION_ERROR: i32 = 6;
const STATUS_PROOF_VERIFICATION_TASK_ERROR: i32 = 7;
const STATUS_NULL_REQUEST_JSON: i32 = 8;
const STATUS_NULL_RPC_RECORDS_JSON: i32 = 9;
const STATUS_REQUEST_JSON_UTF8_ERROR: i32 = 10;
const STATUS_RPC_RECORDS_JSON_UTF8_ERROR: i32 = 11;
const STATUS_NULL_RPC_URL: i32 = 12;
const STATUS_RPC_URL_UTF8_ERROR: i32 = 13;
const STATUS_RESULT_JSON_ERROR: i32 = 14;

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

/// MakeWriter impl so tracing-subscriber can create writers per event.
struct CallbackMakeWriter;

impl<'a> MakeWriter<'a> for CallbackMakeWriter {
    type Writer = CallbackWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CallbackWriter
    }
}

fn install_tracing(cb: LogCallback) {
    set_global_cb(cb);
    let filter = EnvFilter::try_new("info").unwrap_or_else(|_| EnvFilter::new("info"));
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
    block_id: BlockId,
    transaction: RpcTransaction,
    chain_id: ChainId,
}

fn install_oom_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Nightly-only: std::alloc::set_alloc_error_hook. On stable this is a no-op
        // fallback — the default handler already prints the size and aborts.
        // We add an eprintln so the message reaches the log callback's stderr capture.
        #[cfg(feature = "__oom_hook")]
        std::alloc::set_alloc_error_hook(|layout| {
            eprintln!("OOM: allocation of {} bytes failed", layout.size());
        });
        let _ = std::panic::set_hook(Box::new(|info| {
            eprintln!("PANIC: {info}");
        }));
    });
}

fn build_runtime(cb: LogCallback) -> Result<tokio::runtime::Runtime, i32> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| {
        send_log(cb, &format!("Failed to create tokio runtime: {e}"));
        STATUS_RUNTIME_ERROR
    })
}

/// Remove leftover temp files from previous prover runs.
///
/// On iOS, if the app is killed by jetsam (OOM) during a proof, Rust destructors never run and
/// `NamedTempFile`s persist in the app's tmp directory.  These stale files can consume multiple
/// GB and cause the next proof to fail with ENOSPC on mmap spill, followed by an OOM on the
/// heap fallback.  Cleaning the tmp dir before each proof prevents this accumulation.
fn cleanup_stale_tmp_files(cb: LogCallback) {
    let tmp_dir = std::env::temp_dir();
    let removed_bytes = remove_dir_contents(&tmp_dir);
    if removed_bytes > 0 {
        send_log(
            cb,
            &format!(
                "Cleaned {:.1} MB of stale temp files from {}",
                removed_bytes as f64 / (1024.0 * 1024.0),
                tmp_dir.display(),
            ),
        );
    }

    // Walk the app container to find where large data accumulates.  On iOS the container
    // root is two levels above the tmp dir (`.../Application/<UUID>/tmp/` → `.../Application/<UUID>/`).
    let container = tmp_dir.parent().unwrap_or(&tmp_dir);
    report_dir_sizes(cb, container);
}

/// Log byte totals for each immediate subdirectory of `root`.
fn report_dir_sizes(cb: LogCallback, root: &std::path::Path) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let size = dir_size(&path);
            if size > 1024 * 1024 {
                send_log(
                    cb,
                    &format!(
                        "Container dir {:?}: {:.1} MB",
                        path.file_name().unwrap_or_default(),
                        size as f64 / (1024.0 * 1024.0),
                    ),
                );
            }
        }
    }
}

/// Recursively remove all files and subdirectories inside `dir`, but not `dir` itself.
fn remove_dir_contents(dir: &std::path::Path) -> u64 {
    let mut removed = 0u64;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            removed += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let _ = std::fs::remove_file(&path);
        } else if path.is_dir() {
            removed += remove_dir_contents(&path);
            let _ = std::fs::remove_dir(&path);
        }
    }
    removed
}

fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                total += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            } else if p.is_dir() {
                total += dir_size(&p);
            }
        }
    }
    total
}

fn with_prover_runtime<F>(cb: LogCallback, future: F) -> Result<F::Output, i32>
where
    F: Future,
{
    install_tracing(cb);
    install_oom_hook();

    // Remove leftover temp files from previous (possibly OOM-killed) prover runs before
    // allocating any new ones.
    cleanup_stale_tmp_files(cb);

    // Ask the system allocator to return freed pages to the OS.  After a proof run the
    // allocator retains large freed regions; iOS's jetsam counts those against the process's
    // phys_footprint, so a second proof can OOM even though the memory is logically free.
    release_allocator_memory();


    log_memory_state(cb, "before proof");
    log_mmap_state(cb, "ffi:before_proof");
    log_vm_state(cb, "ffi:before_proof");

    // Use low-memory proving path (drops FRI intermediates, recomputes during decommit).
    std::env::set_var("STWO_PROVER_MEMORY_MODE", "low_memory");

    let rt = build_runtime(cb)?;
    let result = rt.block_on(future);
    log_mmap_state(cb, "ffi:after_block_on");
    log_vm_state(cb, "ffi:after_block_on");

    log_memory_state(cb, "after block_on (before rt drop)");
    drop(rt);
    log_memory_state(cb, "after rt drop");
    log_vm_state(cb, "ffi:after_rt_drop");

    release_allocator_memory();
    log_memory_state(cb, "after malloc_zone_pressure_relief");
    log_vm_state(cb, "ffi:after_pressure_relief");

    // Give the kernel a moment to reclaim pages from dropped mmaps/files.
    std::thread::sleep(std::time::Duration::from_secs(1));
    log_memory_state(cb, "after 1s settle");
    log_mmap_state(cb, "ffi:after_1s_settle");
    log_vm_state(cb, "ffi:after_1s_settle");

    Ok(result)
}

/// Tell the system allocator to release as much retained free memory as possible.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn release_allocator_memory() {
    extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // zone = NULL means all zones, goal = 0 means release as much as possible.
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn release_allocator_memory() {}

/// Log system-available memory for the process.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn log_memory_state(cb: LogCallback, label: &str) {
    extern "C" {
        fn os_proc_available_memory() -> u64;
    }
    let available = unsafe { os_proc_available_memory() };
    send_log(
        cb,
        &format!(
            "MEMORY [{label}] available={:.0} MB",
            available as f64 / (1024.0 * 1024.0),
        ),
    );
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn log_memory_state(_cb: LogCallback, _label: &str) {}

#[cfg(feature = "stwo_proving")]
fn log_mmap_state(cb: LogCallback, label: &str) {
    send_log(cb, &format!("MMAP_CHECKPOINT [{label}]"));
    log_recursive_prover_mmap_stats(label);
}

#[cfg(not(feature = "stwo_proving"))]
fn log_mmap_state(_cb: LogCallback, _label: &str) {}

/// Snapshot task VM accounting + largest contiguous free VA gap.
///
/// Complements `log_mmap_state` (which only sees stwo-tracked mmaps) and `log_memory_state`
/// (which only reports phys_footprint headroom). Use to distinguish VA fragmentation from
/// allocator retention when a later mmap fails with ENOMEM at a state an earlier proof
/// survived.
#[cfg(feature = "stwo_proving")]
fn log_vm_state(cb: LogCallback, label: &str) {
    send_log(cb, &format!("VM_CHECKPOINT [{label}]"));
    log_recursive_prover_vm_walk(label);
}

#[cfg(not(feature = "stwo_proving"))]
fn log_vm_state(_cb: LogCallback, _label: &str) {}

fn parse_input_json<'a>(
    input: *const c_char,
    null_status: i32,
    utf8_status: i32,
    label: &str,
    cb: LogCallback,
) -> Result<&'a str, i32> {
    if input.is_null() {
        send_log(cb, &format!("{label} pointer is null."));
        return Err(null_status);
    }

    unsafe { CStr::from_ptr(input) }.to_str().map_err(|e| {
        send_log(cb, &format!("{label} is not valid UTF-8: {e}"));
        utf8_status
    })
}

fn build_runner_factory(rpc_url: Url, chain_id: ChainId) -> RpcRunnerFactory {
    let contract_class_manager = ContractClassManager::start(ContractClassManagerConfig::default());
    let chain_info = get_chain_info(&chain_id, None);
    let runner_config = RunnerConfig {
        storage_proof_config: StorageProofConfig { include_state_changes: true },
        virtual_block_executor_config: RpcVirtualBlockExecutorConfig {
            prefetch_state: false,
            ..Default::default()
        },
    };

    RpcRunnerFactory::new(rpc_url, chain_info, contract_class_manager, runner_config)
}

async fn prove_and_verify_transaction<R: VirtualSnosRunner>(
    factory: R,
    block_id: BlockId,
    transaction: RpcTransaction,
    cb: LogCallback,
) -> Result<ProveTransactionResult, i32> {
    send_log(
        cb,
        "Initializing VirtualSnosProver (preparing precomputes, this may take a while)...",
    );
    let prover = VirtualSnosProver::from_runner(factory);

    send_log(cb, "Running prove_transaction (OS execution + Stwo proving)...");
    let output = prover.prove_transaction(block_id, transaction).await.map_err(|e| {
        send_log(cb, &format!("prove_transaction failed: {e}"));
        STATUS_PROVE_TRANSACTION_ERROR
    })?;
    send_log(cb, "prove_transaction succeeded.");

    send_log(cb, "Verifying proof...");
    let proof_facts = output.proof_facts.clone();
    let proof = output.proof.clone();
    let verify_result = tokio::task::spawn_blocking(move || verify_proof(proof_facts, proof)).await;

    match verify_result {
        Ok(Ok(())) => {
            send_log(cb, "Proof verification succeeded!");
            Ok(output)
        }
        Ok(Err(e)) => {
            send_log(cb, &format!("Proof verification failed: {e}"));
            Err(STATUS_PROOF_VERIFICATION_ERROR)
        }
        Err(e) => {
            send_log(cb, &format!("Proof verification task panicked: {e}"));
            Err(STATUS_PROOF_VERIFICATION_TASK_ERROR)
        }
    }
}

fn serialize_prove_transaction_result(
    output: &ProveTransactionResult,
    cb: LogCallback,
) -> Result<String, i32> {
    serde_json::to_string(output).map_err(|e| {
        send_log(cb, &format!("Failed to serialize proof result JSON: {e}"));
        STATUS_RESULT_JSON_ERROR
    })
}

/// Run the prover end-to-end using JSON supplied by the FFI caller.
///
/// `request_json` must deserialize as `{"params":{"block_id":...,"transaction":...}}`.
/// `rpc_records_json` must deserialize as `{"interactions":[...]}`.
///
/// Returns 0 on success, non-zero on failure. Progress and errors are reported
/// through `cb`.
///
/// # Safety
///
/// `request_json` and `rpc_records_json` must be valid NUL-terminated strings for the lifetime
/// of this call. `cb` must be a valid function pointer for the lifetime of this call.
#[no_mangle]
pub extern "C" fn prove_transaction(
    request_json: *const c_char,
    rpc_records_json: *const c_char,
    cb: LogCallback,
) -> i32 {
    let request_str = match parse_input_json(
        request_json,
        STATUS_NULL_REQUEST_JSON,
        STATUS_REQUEST_JSON_UTF8_ERROR,
        "request_json",
        cb,
    ) {
        Ok(request_str) => request_str,
        Err(code) => return code,
    };

    let records_str = match parse_input_json(
        rpc_records_json,
        STATUS_NULL_RPC_RECORDS_JSON,
        STATUS_RPC_RECORDS_JSON_UTF8_ERROR,
        "rpc_records_json",
        cb,
    ) {
        Ok(records_str) => records_str,
        Err(code) => return code,
    };

    match with_prover_runtime(cb, prove_transaction_async(request_str, records_str, cb)) {
        Ok(code) => code,
        Err(code) => code,
    }
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
    match with_prover_runtime(cb, prove_transaction_async(REQUEST_JSON, RPC_RECORDS_JSON, cb)) {
        Ok(code) => code,
        Err(code) => code,
    }
}

/// Prove a transaction against a live RPC node.
///
/// Returns a heap-allocated JSON string on success. The caller must free the returned pointer with
/// [`free_proof_result`]. Returns null on failure. Progress and errors are reported through `cb`.
///
/// # Safety
///
/// `request_json` and `rpc_url` must be valid NUL-terminated strings for the lifetime of this
/// call. `cb` must be a valid function pointer for the lifetime of this call.
#[no_mangle]
pub extern "C" fn prove_transaction_live(
    request_json: *const c_char,
    rpc_url: *const c_char,
    cb: LogCallback,
) -> *mut c_char {
    let request_str = match parse_input_json(
        request_json,
        STATUS_NULL_REQUEST_JSON,
        STATUS_REQUEST_JSON_UTF8_ERROR,
        "request_json",
        cb,
    ) {
        Ok(request_str) => request_str,
        Err(_) => return std::ptr::null_mut(),
    };

    let rpc_url_str = match parse_input_json(
        rpc_url,
        STATUS_NULL_RPC_URL,
        STATUS_RPC_URL_UTF8_ERROR,
        "rpc_url",
        cb,
    ) {
        Ok(rpc_url_str) => rpc_url_str,
        Err(_) => return std::ptr::null_mut(),
    };

    let result_json =
        match with_prover_runtime(cb, prove_transaction_live_async(request_str, rpc_url_str, cb)) {
            Ok(Some(result_json)) => result_json,
            Ok(None) | Err(_) => return std::ptr::null_mut(),
        };

    match CString::new(result_json) {
        Ok(result_json) => result_json.into_raw(),
        Err(e) => {
            send_log(cb, &format!("Failed to convert proof result into a C string: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// Free a result string returned by [`prove_transaction_live`].
#[no_mangle]
pub extern "C" fn free_proof_result(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            drop(CString::from_raw(ptr));
        }
    }
}

async fn prove_transaction_async(request_str: &str, records_str: &str, cb: LogCallback) -> i32 {
    send_log(cb, "Parsing request JSON...");
    let request: ProveTransactionRequest = match serde_json::from_str(request_str) {
        Ok(r) => r,
        Err(e) => {
            send_log(cb, &format!("Failed to parse request JSON: {e}"));
            return STATUS_REQUEST_JSON_ERROR;
        }
    };
    let ProveTransactionParams { block_id, transaction, chain_id } = request.params;

    send_log(cb, "Parsing RPC records JSON...");
    let records: RpcRecords = match serde_json::from_str(records_str) {
        Ok(r) => r,
        Err(e) => {
            send_log(cb, &format!("Failed to parse RPC records: {e}"));
            return STATUS_RPC_RECORDS_JSON_ERROR;
        }
    };

    send_log(cb, "Starting mock RPC server for offline replay...");
    let server = MockRpcServer::new(&records).await;
    let rpc_url = server.url();
    send_log(cb, &format!("Mock server listening at {rpc_url}"));

    send_log(cb, "Building runner factory...");
    let rpc_url = match Url::parse(&rpc_url) {
        Ok(rpc_url) => rpc_url,
        Err(e) => {
            send_log(cb, &format!("Invalid RPC URL: {e}"));
            return STATUS_RPC_URL_ERROR;
        }
    };

    let factory = build_runner_factory(rpc_url, chain_id);
    match prove_and_verify_transaction(factory, block_id, transaction, cb).await {
        Ok(_) => STATUS_OK,
        Err(code) => code,
    }
}

async fn prove_transaction_live_async(
    request_str: &str,
    rpc_url_str: &str,
    cb: LogCallback,
) -> Option<String> {
    send_log(cb, "Parsing request JSON...");
    let request: ProveTransactionRequest = match serde_json::from_str(request_str) {
        Ok(r) => r,
        Err(e) => {
            send_log(cb, &format!("Failed to parse request JSON: {e}"));
            return None;
        }
    };
    let ProveTransactionParams { block_id, transaction, chain_id } = request.params;

    send_log(cb, "Parsing live RPC URL...");
    let rpc_url = match Url::parse(rpc_url_str) {
        Ok(rpc_url) => rpc_url,
        Err(e) => {
            send_log(cb, &format!("Invalid RPC URL: {e}"));
            return None;
        }
    };

    send_log(cb, &format!("Using live RPC node at {rpc_url}, chain_id: {chain_id}, block_id: {block_id:?}"));
    let factory = build_runner_factory(rpc_url, chain_id);
    let output = match prove_and_verify_transaction(factory, block_id, transaction, cb).await {
        Ok(output) => output,
        Err(_) => return None,
    };

    serialize_prove_transaction_result(&output, cb).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use starknet_api::core::EthAddress;
    use starknet_api::transaction::fields::{Proof, ProofFacts};
    use starknet_api::transaction::{L2ToL1Payload, MessageToL1};
    use starknet_api::{contract_address, felt};

    use super::*;

    extern "C" fn noop_log_callback(_msg: *const c_char) {}

    fn sample_prove_transaction_result() -> ProveTransactionResult {
        ProveTransactionResult {
            proof: Proof::from(vec![1_u8, 2, 3, 4]),
            proof_facts: ProofFacts::from(vec![felt!("0x1"), felt!("0x2")]),
            l2_to_l1_messages: vec![MessageToL1 {
                from_address: contract_address!("0x123"),
                to_address: EthAddress::try_from(felt!("0x456")).unwrap(),
                payload: L2ToL1Payload(vec![felt!("0x7"), felt!("0x8")]),
            }],
        }
    }

    fn proof_result_ptr_to_string(ptr: *mut c_char) -> String {
        let result = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned();
        free_proof_result(ptr);
        result
    }

    #[test]
    fn prove_transaction_rejects_null_request_pointer() {
        let rpc_records_json = CString::new(RPC_RECORDS_JSON).unwrap();

        let result =
            prove_transaction(std::ptr::null(), rpc_records_json.as_ptr(), noop_log_callback);

        assert_eq!(result, STATUS_NULL_REQUEST_JSON);
    }

    #[test]
    fn prove_transaction_rejects_null_rpc_records_pointer() {
        let request_json = CString::new(REQUEST_JSON).unwrap();

        let result = prove_transaction(request_json.as_ptr(), std::ptr::null(), noop_log_callback);

        assert_eq!(result, STATUS_NULL_RPC_RECORDS_JSON);
    }

    #[test]
    fn prove_transaction_rejects_invalid_request_utf8() {
        let invalid_request_json = [0x80_u8, 0];
        let rpc_records_json = CString::new(RPC_RECORDS_JSON).unwrap();

        let result = prove_transaction(
            invalid_request_json.as_ptr().cast(),
            rpc_records_json.as_ptr(),
            noop_log_callback,
        );

        assert_eq!(result, STATUS_REQUEST_JSON_UTF8_ERROR);
    }

    #[test]
    fn prove_transaction_live_rejects_null_request_pointer() {
        let rpc_url = CString::new("https://example.com").unwrap();

        let result = prove_transaction_live(std::ptr::null(), rpc_url.as_ptr(), noop_log_callback);

        assert!(result.is_null());
    }

    #[test]
    fn prove_transaction_live_rejects_null_rpc_url_pointer() {
        let request_json = CString::new(REQUEST_JSON).unwrap();

        let result =
            prove_transaction_live(request_json.as_ptr(), std::ptr::null(), noop_log_callback);

        assert!(result.is_null());
    }

    #[test]
    fn prove_transaction_live_rejects_invalid_request_utf8() {
        let invalid_request_json = [0x80_u8, 0];
        let rpc_url = CString::new("https://example.com").unwrap();

        let result = prove_transaction_live(
            invalid_request_json.as_ptr().cast(),
            rpc_url.as_ptr(),
            noop_log_callback,
        );

        assert!(result.is_null());
    }

    #[test]
    fn prove_transaction_live_rejects_invalid_rpc_url_utf8() {
        let request_json = CString::new(REQUEST_JSON).unwrap();
        let invalid_rpc_url = [0x80_u8, 0];

        let result = prove_transaction_live(
            request_json.as_ptr(),
            invalid_rpc_url.as_ptr().cast(),
            noop_log_callback,
        );

        assert!(result.is_null());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prove_transaction_async_rejects_invalid_request_json() {
        let result = prove_transaction_async("not json", RPC_RECORDS_JSON, noop_log_callback).await;

        assert_eq!(result, STATUS_REQUEST_JSON_ERROR);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prove_transaction_async_rejects_invalid_rpc_records_json() {
        let result = prove_transaction_async(REQUEST_JSON, "not json", noop_log_callback).await;

        assert_eq!(result, STATUS_RPC_RECORDS_JSON_ERROR);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prove_transaction_live_async_rejects_invalid_request_json() {
        let result =
            prove_transaction_live_async("not json", "https://example.com", noop_log_callback)
                .await;

        assert!(result.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prove_transaction_live_async_rejects_invalid_rpc_url() {
        let result =
            prove_transaction_live_async(REQUEST_JSON, "not a url", noop_log_callback).await;

        assert!(result.is_none());
    }

    #[test]
    fn serialize_prove_transaction_result_matches_sdk_shape() {
        let json = serialize_prove_transaction_result(
            &sample_prove_transaction_result(),
            noop_log_callback,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["proof"], Value::String("AQIDBA==".to_owned()));
        assert_eq!(value["proof_facts"], json!(["0x1", "0x2"]));
        assert_eq!(
            value["l2_to_l1_messages"],
            json!([{
                "from_address": "0x123",
                "to_address": "0x456",
                "payload": ["0x7", "0x8"]
            }])
        );
    }

    #[test]
    fn free_proof_result_accepts_null() {
        free_proof_result(std::ptr::null_mut());
    }

    #[test]
    fn free_proof_result_releases_owned_string() {
        let ptr = CString::new("{\"proof\":\"AQ==\"}").unwrap().into_raw();

        free_proof_result(ptr);
    }

    #[test]
    fn prove_transaction_live_result_can_be_freed_after_reading() {
        let json = serialize_prove_transaction_result(
            &sample_prove_transaction_result(),
            noop_log_callback,
        )
        .unwrap();
        let ptr = CString::new(json.clone()).unwrap().into_raw();

        let round_trip = proof_result_ptr_to_string(ptr);

        assert_eq!(round_trip, json);
    }
}
