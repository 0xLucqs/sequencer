//! CLI entry point for profiling (cargo flamegraph, instruments, etc.).
//!
//! Runs the exact same code path as the iOS FFI: same embedded inputs,
//! same in-process Sierra compile, same low-memory prover.

use std::ffi::CStr;
use std::os::raw::c_char;

use starknet_transaction_prover::ffi::prove_privacy_demo;

extern "C" fn log_callback(msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    unsafe {
        if let Ok(s) = CStr::from_ptr(msg).to_str() {
            println!("{s}");
        }
    }
}

fn main() {
    let code = prove_privacy_demo(log_callback);
    std::process::exit(code);
}
