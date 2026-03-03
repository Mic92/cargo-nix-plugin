//! Nix plugin entry point using the Nix C API.
//!
//! Exports `nix_plugin_entry` which registers the `resolveCargoWorkspace` primop.
//! The primop extracts fields from the input attrset via the C API, calls the Rust
//! resolver, and constructs the result attrset directly (no JSON round-trip on output).

use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::ptr;

use nix_bindings_expr_sys as nix_expr;
use nix_bindings_store_sys as nix_store;
use nix_bindings_util_sys as nix_util;

use crate::cfg_eval::TargetDescription;
use crate::nix_builder;
use crate::nix_val::{NixContext, Value};
use crate::resolve::resolve_workspace;

/// The cargo binary path baked in at build time from the Nix store.
const BUILTIN_CARGO_PATH: &str = match option_env!("CARGO_NIX_PLUGIN_CARGO_PATH") {
    Some(p) => p,
    None => "cargo",
};

// ---------------------------------------------------------------------------
// Input extraction
// ---------------------------------------------------------------------------

struct PluginInput {
    metadata: Option<String>,
    cargo_lock: Option<String>,
    manifest_path: Option<String>,
    target: TargetDescription,
    root_features: Vec<String>,
}

fn extract_input(nx: &NixContext, input: &Value) -> Result<PluginInput, String> {
    let metadata = nx
        .opt_str_attr(input, c"metadata")
        .map_err(|e| e.to_string())?;
    let cargo_lock = nx
        .opt_str_attr(input, c"cargoLock")
        .map_err(|e| e.to_string())?;
    let manifest_path = nx
        .opt_str_attr(input, c"manifestPath")
        .map_err(|e| e.to_string())?;
    let target = extract_target(nx, input)?;
    let root_features = if nx.has_attr(input, c"rootFeatures") {
        nx.str_list_attr(input, c"rootFeatures")
            .map_err(|e| e.to_string())?
    } else {
        vec!["default".to_string()]
    };

    Ok(PluginInput {
        metadata,
        cargo_lock,
        manifest_path,
        target,
        root_features,
    })
}

fn extract_target(nx: &NixContext, attrs: &Value) -> Result<TargetDescription, String> {
    let t = nx.attr(attrs, c"target").map_err(|e| e.to_string())?;
    Ok(TargetDescription {
        name: nx.str_attr(&t, c"name").map_err(|e| e.to_string())?,
        os: nx.str_attr(&t, c"os").map_err(|e| e.to_string())?,
        arch: nx.str_attr(&t, c"arch").map_err(|e| e.to_string())?,
        vendor: nx.str_attr(&t, c"vendor").map_err(|e| e.to_string())?,
        env: nx.str_attr(&t, c"env").map_err(|e| e.to_string())?,
        family: nx.str_list_attr(&t, c"family").map_err(|e| e.to_string())?,
        pointer_width: nx
            .str_attr(&t, c"pointer_width")
            .map_err(|e| e.to_string())?,
        endian: nx.str_attr(&t, c"endian").map_err(|e| e.to_string())?,
        unix: nx.bool_attr(&t, c"unix").map_err(|e| e.to_string())?,
        windows: nx.bool_attr(&t, c"windows").map_err(|e| e.to_string())?,
    })
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

fn validate_and_resolve(input: &PluginInput) -> Result<crate::resolve::WorkspaceResult, String> {
    let (metadata_json, cargo_lock_str) =
        match (&input.metadata, &input.manifest_path) {
            (Some(_), Some(_)) => {
                return Err("Provide either 'metadata' or 'manifestPath', not both.".into())
            }
            (None, None) => return Err(
                "Provide either 'metadata' (explicit JSON) or 'manifestPath' (path to Cargo.toml)."
                    .into(),
            ),
            (Some(metadata), None) => {
                let lock = input
                    .cargo_lock
                    .as_deref()
                    .ok_or("'cargoLock' is required when 'metadata' is provided.")?;
                (metadata.clone(), lock.to_string())
            }
            (None, Some(manifest_path)) => {
                let metadata_json = run_cargo_metadata(BUILTIN_CARGO_PATH, manifest_path)?;
                let dir = std::path::Path::new(manifest_path)
                    .parent()
                    .ok_or_else(|| format!("Cannot determine parent of {manifest_path}"))?;
                let lock = std::fs::read_to_string(dir.join("Cargo.lock"))
                    .map_err(|e| format!("Failed to read Cargo.lock: {e}"))?;
                (metadata_json, lock)
            }
        };
    resolve_workspace(
        &metadata_json,
        &cargo_lock_str,
        &input.target,
        &input.root_features,
    )
}

fn run_cargo_metadata(cargo: &str, manifest: &str) -> Result<String, String> {
    let out = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--manifest-path",
            manifest,
        ])
        .output()
        .map_err(|e| format!("Failed to run '{cargo} metadata': {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed ({}):\n{}\nHint: pass 'metadata' explicitly for offline/pure usage.",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("cargo metadata: invalid UTF-8: {e}"))
}

// ---------------------------------------------------------------------------
// Primop callback
// ---------------------------------------------------------------------------

/// Set an error message on the nix context.
unsafe fn set_error(ctx: *mut nix_util::c_context, msg: &str) {
    let c = CString::new(msg).unwrap_or_else(|_| CString::new("(error)").unwrap());
    unsafe { nix_util::set_err_msg(ctx, nix_util::err_NIX_ERR_NIX_ERROR, c.as_ptr()) };
}

unsafe extern "C" fn prim_resolve_cargo_workspace(
    _user_data: *mut c_void,
    ctx: *mut nix_util::c_context,
    state: *mut nix_expr::EvalState,
    args: *mut *mut nix_expr::Value,
    ret: *mut nix_expr::Value,
) {
    let nx = NixContext { ctx, state };

    let input_ptr = unsafe { *args };
    unsafe { nix_expr::value_force(ctx, state, input_ptr) };
    if unsafe { nix_util::err_code(ctx) } != nix_util::err_NIX_OK {
        return;
    }

    let typ = unsafe { nix_expr::get_type(ctx, input_ptr) };
    if typ != nix_expr::ValueType_NIX_TYPE_ATTRS {
        unsafe { set_error(ctx, "resolveCargoWorkspace: expected an attrset") };
        return;
    }

    // Borrow the arg (increfs so our Value drop is balanced)
    let input_val = unsafe { nx.borrow_value(input_ptr) };
    let plugin_input = match extract_input(&nx, &input_val) {
        Ok(i) => i,
        Err(e) => {
            unsafe { set_error(ctx, &format!("resolveCargoWorkspace: {e}")) };
            return;
        }
    };
    drop(input_val); // release our borrow before the heavy work

    let result = match validate_and_resolve(&plugin_input) {
        Ok(r) => r,
        Err(e) => {
            unsafe { set_error(ctx, &format!("resolveCargoWorkspace: {e}")) };
            return;
        }
    };

    match nix_builder::mk_workspace_result(&nx, &result) {
        Ok(v) => nx.copy_to_ret(ret, &v),
        Err(e) => unsafe { set_error(ctx, &format!("resolveCargoWorkspace: {e}")) },
    }
}

// ---------------------------------------------------------------------------
// Plugin entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn nix_plugin_entry() {
    unsafe {
        let ctx = nix_util::c_context_create();
        nix_util::libutil_init(ctx);
        nix_store::libstore_init(ctx);
        nix_expr::libexpr_init(ctx);

        if nix_util::err_code(ctx) != nix_util::err_NIX_OK {
            let msg = nix_util::err_msg(ptr::null_mut(), ctx, ptr::null_mut());
            if !msg.is_null() {
                eprintln!(
                    "cargo-nix-plugin: init failed: {}",
                    CStr::from_ptr(msg).to_string_lossy()
                );
            }
            nix_util::c_context_free(ctx);
            return;
        }

        let name = c"resolveCargoWorkspace";
        let mut arg_names: [*const std::os::raw::c_char; 2] = [c"attrs".as_ptr(), ptr::null()];
        let doc = c"Resolve a Cargo workspace into a crate metadata attrset.\n\nAccepts an attrset with:\n- `metadata`: JSON string from `cargo metadata --format-version 1 --locked`\n- `cargoLock`: Contents of `Cargo.lock`\n- `target`: Attrset describing the target platform\n- `rootFeatures` (optional): List of features to enable (defaults to `[\"default\"]`)";

        let primop = nix_expr::alloc_primop(
            ctx,
            Some(prim_resolve_cargo_workspace),
            1,
            name.as_ptr(),
            arg_names.as_mut_ptr(),
            doc.as_ptr(),
            ptr::null_mut(),
        );

        if primop.is_null() {
            let msg = nix_util::err_msg(ptr::null_mut(), ctx, ptr::null_mut());
            if !msg.is_null() {
                eprintln!(
                    "cargo-nix-plugin: alloc primop failed: {}",
                    CStr::from_ptr(msg).to_string_lossy()
                );
            }
            nix_util::c_context_free(ctx);
            return;
        }

        if nix_expr::register_primop(ctx, primop) != nix_util::err_NIX_OK {
            let msg = nix_util::err_msg(ptr::null_mut(), ctx, ptr::null_mut());
            if !msg.is_null() {
                eprintln!(
                    "cargo-nix-plugin: register primop failed: {}",
                    CStr::from_ptr(msg).to_string_lossy()
                );
            }
        }

        nix_expr::gc_decref(ptr::null_mut(), primop as *const c_void);
        nix_util::c_context_free(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_x86_64() -> TargetDescription {
        TargetDescription {
            name: "x86_64-unknown-linux-gnu".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            vendor: "unknown".into(),
            env: "gnu".into(),
            family: vec!["unix".into()],
            pointer_width: "64".into(),
            endian: "little".into(),
            unix: true,
            windows: false,
        }
    }

    #[test]
    #[ignore]
    fn subprocess_resolves_own_workspace() {
        let input = PluginInput {
            metadata: None,
            cargo_lock: None,
            manifest_path: Some(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml").into()),
            target: linux_x86_64(),
            root_features: vec!["default".into()],
        };
        let result = validate_and_resolve(&input).expect("resolution failed");
        assert!(!result.workspace_members.is_empty());
        assert!(result.crates.values().any(|c| c.crate_name == "serde"));
    }
}
