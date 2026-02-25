//! C FFI interface for calling from the C++ Nix plugin shim.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use crate::cfg_eval::TargetDescription;
use crate::resolve::resolve_workspace;

/// Input from the Nix side — the entire attrset serialized as JSON.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginInput {
    metadata: String,
    cargo_lock: String,
    target: TargetDescription,
    #[serde(default = "default_root_features")]
    root_features: Vec<String>,
}

fn default_root_features() -> Vec<String> {
    vec!["default".to_string()]
}

/// Resolve a cargo workspace. Input and output are JSON strings.
///
/// # Safety
/// `input_json` must be a valid null-terminated C string.
/// The returned strings must be freed with `free_string`.
#[no_mangle]
pub unsafe extern "C" fn resolve_cargo_workspace(
    input_json: *const c_char,
    out: *mut *mut c_char,
    err_out: *mut *mut c_char,
) -> i32 {
    let input_str = match unsafe { CStr::from_ptr(input_json) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            let msg = CString::new(format!("Invalid UTF-8 in input: {e}")).unwrap();
            unsafe { *err_out = msg.into_raw() };
            return 1;
        }
    };

    let input: PluginInput = match serde_json::from_str(input_str) {
        Ok(v) => v,
        Err(e) => {
            let msg = CString::new(format!("Failed to parse plugin input: {e}")).unwrap();
            unsafe { *err_out = msg.into_raw() };
            return 1;
        }
    };

    match resolve_workspace(&input.metadata, &input.cargo_lock, &input.target, &input.root_features)
    {
        Ok(result) => {
            let json = serde_json::to_string(&result).unwrap();
            let cstr = CString::new(json).unwrap();
            unsafe { *out = cstr.into_raw() };
            0
        }
        Err(e) => {
            let msg = CString::new(e).unwrap();
            unsafe { *err_out = msg.into_raw() };
            1
        }
    }
}

/// Free a string returned by `resolve_cargo_workspace`.
///
/// # Safety
/// The pointer must have been returned by `resolve_cargo_workspace`.
#[no_mangle]
pub unsafe extern "C" fn free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}
