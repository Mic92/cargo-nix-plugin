//! Safe(r) wrappers around the Nix C API for reading and building values.
//!
//! Uses auto-generated FFI bindings from `nix-bindings-{expr,store,util}-sys`
//! instead of hand-written extern blocks. Provides `NixContext` (holds ctx +
//! state pointers) with RAII `Value` handles that auto-decref, plus a
//! declarative `nix_attrs!` macro for building attrsets.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_uint, c_void};

use nix_bindings_expr_sys as nix_expr;
use nix_bindings_util_sys as nix_util;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct NixError(pub String);

impl std::fmt::Display for NixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type Result<T> = std::result::Result<T, NixError>;

// ---------------------------------------------------------------------------
// RAII value handle
// ---------------------------------------------------------------------------

/// Owned handle to a Nix value that calls `value_decref` on drop.
pub struct Value {
    ctx: *mut nix_util::c_context,
    ptr: *mut nix_expr::Value,
}

impl Value {
    /// Wrap an already-increffed pointer. Takes ownership of one ref.
    ///
    /// # Safety
    /// `ptr` must be a valid, non-null nix value with an outstanding reference.
    pub unsafe fn from_raw(ctx: *mut nix_util::c_context, ptr: *mut nix_expr::Value) -> Self {
        Self { ctx, ptr }
    }

    pub fn as_ptr(&self) -> *mut nix_expr::Value {
        self.ptr
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        unsafe { nix_expr::value_decref(self.ctx, self.ptr) };
    }
}

// ---------------------------------------------------------------------------
// Context: the main entry point for all Nix C API operations
// ---------------------------------------------------------------------------

/// Bundles a `c_context` + `EvalState` and provides all read/write helpers.
pub struct NixContext {
    pub ctx: *mut nix_util::c_context,
    pub state: *mut nix_expr::EvalState,
}

impl NixContext {
    fn check(&self) -> Result<()> {
        if unsafe { nix_util::err_code(self.ctx) } != nix_util::err_NIX_OK {
            Err(NixError("nix C API error".into()))
        } else {
            Ok(())
        }
    }

    // -- borrowing ---------------------------------------------------------

    /// Wrap a raw pointer we don't own (e.g. a primop arg) into a `Value`.
    /// Increments the refcount so the `Value` drop is balanced.
    ///
    /// # Safety
    /// `ptr` must be a valid nix value that outlives the returned `Value`.
    pub unsafe fn borrow_value(&self, ptr: *mut nix_expr::Value) -> Value {
        unsafe { nix_expr::value_incref(self.ctx, ptr) };
        unsafe { Value::from_raw(self.ctx, ptr) }
    }

    // -- allocation --------------------------------------------------------

    pub fn alloc(&self) -> Result<Value> {
        let v = unsafe { nix_expr::alloc_value(self.ctx, self.state) };
        if v.is_null() {
            return Err(NixError("alloc_value failed".into()));
        }
        Ok(unsafe { Value::from_raw(self.ctx, v) })
    }

    // -- reading -----------------------------------------------------------

    /// Force a value (evaluate thunks).
    pub fn force(&self, v: &Value) -> Result<()> {
        unsafe { nix_expr::value_force(self.ctx, self.state, v.as_ptr()) };
        self.check()
    }

    /// Get a required attribute, force it, and return an owned handle.
    pub fn attr(&self, set: &Value, name: &std::ffi::CStr) -> Result<Value> {
        let p =
            unsafe { nix_expr::get_attr_byname(self.ctx, set.as_ptr(), self.state, name.as_ptr()) };
        if p.is_null() {
            return Err(NixError(format!(
                "missing attribute '{}'",
                name.to_string_lossy()
            )));
        }
        let v = unsafe { Value::from_raw(self.ctx, p) };
        self.force(&v)?;
        Ok(v)
    }

    /// Check whether an attribute exists.
    pub fn has_attr(&self, set: &Value, name: &std::ffi::CStr) -> bool {
        unsafe { nix_expr::has_attr_byname(self.ctx, set.as_ptr(), self.state, name.as_ptr()) }
    }

    /// Extract a Rust `String` from a forced Nix string value.
    pub fn to_string(&self, v: &Value) -> Result<String> {
        let mut buf = String::new();
        let ptr: *mut String = &mut buf;

        unsafe extern "C" fn cb(start: *const c_char, n: c_uint, ud: *mut c_void) {
            let s = unsafe { std::slice::from_raw_parts(start as *const u8, n as usize) };
            unsafe { &mut *(ud as *mut String) }.push_str(&String::from_utf8_lossy(s));
        }

        let rc =
            unsafe { nix_expr::get_string(self.ctx, v.as_ptr(), Some(cb), ptr as *mut c_void) };
        if rc != nix_util::err_NIX_OK {
            return Err(NixError("get_string failed".into()));
        }
        Ok(buf)
    }

    /// Extract a Rust `bool` from a forced Nix bool value.
    pub fn to_bool(&self, v: &Value) -> Result<bool> {
        let b = unsafe { nix_expr::get_bool(self.ctx, v.as_ptr()) };
        self.check()?;
        Ok(b)
    }

    // -- compound readers (attr → Rust) ------------------------------------

    /// `attr(name)` → `String`
    pub fn str_attr(&self, set: &Value, name: &std::ffi::CStr) -> Result<String> {
        self.to_string(&self.attr(set, name)?)
    }

    /// `attr(name)` → `bool`
    pub fn bool_attr(&self, set: &Value, name: &std::ffi::CStr) -> Result<bool> {
        self.to_bool(&self.attr(set, name)?)
    }

    /// `attr(name)` → `Option<String>` (None if missing).
    pub fn opt_str_attr(&self, set: &Value, name: &std::ffi::CStr) -> Result<Option<String>> {
        if !self.has_attr(set, name) {
            return Ok(None);
        }
        self.str_attr(set, name).map(Some)
    }

    /// `attr(name)` → `Vec<String>`
    pub fn str_list_attr(&self, set: &Value, name: &std::ffi::CStr) -> Result<Vec<String>> {
        let list = self.attr(set, name)?;
        let len = unsafe { nix_expr::get_list_size(self.ctx, list.as_ptr()) };
        self.check()?;
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            let elem = unsafe { nix_expr::get_list_byidx(self.ctx, list.as_ptr(), self.state, i) };
            if elem.is_null() {
                return Err(NixError(format!("list index {i} returned null")));
            }
            let ev = unsafe { Value::from_raw(self.ctx, elem) };
            self.force(&ev)?;
            out.push(self.to_string(&ev)?);
        }
        Ok(out)
    }

    // -- building ----------------------------------------------------------

    /// Create a Nix string value.
    pub fn mk_string(&self, s: &str) -> Result<Value> {
        let v = self.alloc()?;
        let cs = CString::new(s).map_err(|e| NixError(format!("null in string: {e}")))?;
        unsafe { nix_expr::init_string(self.ctx, v.as_ptr(), cs.as_ptr()) };
        self.check()?;
        Ok(v)
    }

    /// Create a Nix bool value.
    pub fn mk_bool(&self, b: bool) -> Result<Value> {
        let v = self.alloc()?;
        unsafe { nix_expr::init_bool(self.ctx, v.as_ptr(), b) };
        self.check()?;
        Ok(v)
    }

    /// Create a Nix null value.
    pub fn mk_null(&self) -> Result<Value> {
        let v = self.alloc()?;
        unsafe { nix_expr::init_null(self.ctx, v.as_ptr()) };
        self.check()?;
        Ok(v)
    }

    /// `Some(s)` → nix string, `None` → nix null.
    pub fn mk_opt_string(&self, s: &Option<String>) -> Result<Value> {
        match s {
            Some(s) => self.mk_string(s),
            None => self.mk_null(),
        }
    }

    /// Build a Nix list from already-constructed `Value` handles.
    pub fn mk_list(&self, items: Vec<Value>) -> Result<Value> {
        let builder = unsafe { nix_expr::make_list_builder(self.ctx, self.state, items.len()) };
        if builder.is_null() {
            return Err(NixError("list builder alloc failed".into()));
        }
        for (i, item) in items.iter().enumerate() {
            unsafe { nix_expr::list_builder_insert(self.ctx, builder, i as u32, item.as_ptr()) };
        }
        let v = self.alloc()?;
        unsafe { nix_expr::make_list(self.ctx, builder, v.as_ptr()) };
        unsafe { nix_expr::list_builder_free(builder) };
        self.check()?;
        Ok(v)
    }

    /// Build a Nix `[ "a" "b" … ]` list.
    pub fn mk_string_list(&self, items: &[String]) -> Result<Value> {
        items
            .iter()
            .map(|s| self.mk_string(s))
            .collect::<Result<Vec<_>>>()
            .and_then(|v| self.mk_list(v))
    }

    /// Build a Nix attrset from `(name, Value)` pairs.
    pub fn mk_attrs(&self, entries: Vec<(&str, Value)>) -> Result<Value> {
        let builder =
            unsafe { nix_expr::make_bindings_builder(self.ctx, self.state, entries.len()) };
        if builder.is_null() {
            return Err(NixError("bindings builder alloc failed".into()));
        }
        for (name, val) in &entries {
            let cn = CString::new(*name).map_err(|e| NixError(format!("null in key: {e}")))?;
            unsafe {
                nix_expr::bindings_builder_insert(self.ctx, builder, cn.as_ptr(), val.as_ptr())
            };
        }
        let v = self.alloc()?;
        unsafe { nix_expr::make_attrs(self.ctx, v.as_ptr(), builder) };
        unsafe { nix_expr::bindings_builder_free(builder) };
        self.check()?;
        Ok(v)
    }

    /// Build `{ key1 = "val1"; key2 = "val2"; … }` from a `BTreeMap`.
    pub fn mk_string_map(&self, map: &BTreeMap<String, String>) -> Result<Value> {
        let entries: Vec<_> = map
            .iter()
            .map(|(k, v)| self.mk_string(v).map(|val| (k.as_str(), val)))
            .collect::<Result<_>>()?;
        self.mk_attrs(entries)
    }

    /// Build `{ key1 = [ … ]; key2 = [ … ]; … }` from a `BTreeMap`.
    pub fn mk_string_list_map(&self, map: &BTreeMap<String, Vec<String>>) -> Result<Value> {
        let entries: Vec<_> = map
            .iter()
            .map(|(k, v)| self.mk_string_list(v).map(|val| (k.as_str(), val)))
            .collect::<Result<_>>()?;
        self.mk_attrs(entries)
    }

    /// Copy a finished value into the primop return slot.
    pub fn copy_to_ret(&self, ret: *mut nix_expr::Value, src: &Value) {
        unsafe { nix_expr::copy_value(self.ctx, ret, src.as_ptr()) };
    }
}

/// Declarative attrset builder.
///
/// ```ignore
/// nix_attrs!(nx,
///     "name" => nx.mk_string("hello")?,
///     "version" => nx.mk_string("1.0")?,
/// )
/// ```
#[macro_export]
macro_rules! nix_attrs {
    ($nx:expr, $( $key:expr => $val:expr ),* $(,)?) => {{
        let entries: Vec<(&str, $crate::nix_val::Value)> = vec![
            $( ($key, $val) ),*
        ];
        $nx.mk_attrs(entries)
    }};
}
