use serde::{Deserialize, Serialize};

// Regression: renamed dependency (my_http = { package = "http" }) must
// be linked via --extern my_http=... so this import resolves.
pub use my_http::StatusCode;

// Regression: fnv's `[lib] path = "lib.rs"` and new_debug_unreachable's
// `[lib] name = "debug_unreachable"` are invisible to the sparse index;
// the native builder must recover both from Cargo.toml at build time.
pub use fnv::FnvHashMap;
#[allow(unused_imports)]
use debug_unreachable::debug_unreachable;

#[derive(Serialize, Deserialize, Debug)]
pub struct Greeting {
    pub message: String,
}

impl Greeting {
    pub fn new(message: &str) -> Self {
        Greeting {
            message: message.to_string(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serialize")
    }
}
