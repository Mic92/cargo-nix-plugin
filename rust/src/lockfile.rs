//! Parse Cargo.lock for sha256 checksums and convert hex to SRI format.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use std::collections::HashMap;

/// Parsed lockfile checksums: (name, version) -> SRI hash
pub type LockfileHashes = HashMap<(String, String), String>;

#[derive(Deserialize)]
struct CargoLock {
    package: Vec<LockPackage>,
}

#[derive(Deserialize)]
struct LockPackage {
    name: String,
    version: String,
    #[serde(default)]
    checksum: Option<String>,
}

/// Parse a Cargo.lock string and extract checksums by (name, version),
/// converting hex sha256 to SRI format.
pub fn parse_lockfile(cargo_lock: &str) -> LockfileHashes {
    let lock: CargoLock = toml::from_str(cargo_lock).expect("failed to parse Cargo.lock");
    let mut hashes = LockfileHashes::new();
    for pkg in &lock.package {
        if let Some(checksum) = &pkg.checksum {
            if !checksum.is_empty() && checksum != "<none>" {
                hashes.insert(
                    (pkg.name.clone(), pkg.version.clone()),
                    hex_to_sri(checksum),
                );
            }
        }
    }
    hashes
}

/// Convert a hex-encoded sha256 to SRI format (sha256-<base64>).
pub fn hex_to_sri(hex: &str) -> String {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("invalid hex"))
        .collect();
    format!("sha256-{}", STANDARD.encode(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_to_sri_known_value() {
        let sri = hex_to_sri("087113bd50d9adce24850eed5d0476c7d199d532fce8fab5173650331e09033a");
        assert_eq!(sri, "sha256-CHETvVDZrc4khQ7tXQR2x9GZ1TL86Pq1FzZQMx4JAzo=");
    }

    #[test]
    fn hex_to_sri_all_zeros() {
        let sri = hex_to_sri("0000000000000000000000000000000000000000000000000000000000000000");
        assert_eq!(sri, "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }

    #[test]
    fn parse_lockfile_extracts_known_checksums() {
        let lock = r#"
version = 4

[[package]]
name = "abnf"
version = "0.13.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "087113bd50d9adce24850eed5d0476c7d199d532fce8fab5173650331e09033a"
dependencies = [
 "abnf-core",
]

[[package]]
name = "serde"
version = "1.0.210"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "c8e3592472072e6e22e0a54d5904d9febf8508f65fb8552499a1abc7d1078c3a"
"#;
        let hashes = parse_lockfile(lock);
        assert_eq!(
            hashes.get(&("abnf".to_string(), "0.13.0".to_string())),
            Some(&"sha256-CHETvVDZrc4khQ7tXQR2x9GZ1TL86Pq1FzZQMx4JAzo=".to_string())
        );
        assert!(hashes.contains_key(&("serde".to_string(), "1.0.210".to_string())));
    }

    #[test]
    fn parse_lockfile_local_crates_have_no_entry() {
        let lock = r#"
version = 4

[[package]]
name = "my-local-crate"
version = "0.1.0"
dependencies = [
 "serde",
]

[[package]]
name = "serde"
version = "1.0.210"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "c8e3592472072e6e22e0a54d5904d9febf8508f65fb8552499a1abc7d1078c3a"
"#;
        let hashes = parse_lockfile(lock);
        assert!(!hashes.contains_key(&("my-local-crate".to_string(), "0.1.0".to_string())));
        assert!(hashes.contains_key(&("serde".to_string(), "1.0.210".to_string())));
    }

    #[test]
    fn parse_lockfile_v3_format() {
        let lock = r#"
version = 3

[[package]]
name = "itoa"
version = "1.0.11"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "49f1f14873335454500d59611f1cf4a4b0f786f9ac11f4312a78e4cf2566695b"
"#;
        let hashes = parse_lockfile(lock);
        assert!(hashes.contains_key(&("itoa".to_string(), "1.0.11".to_string())));
    }

    #[test]
    fn parse_lockfile_torture_workspace() {
        let lock = include_str!("../tests/fixtures/Cargo.lock");
        let hashes = parse_lockfile(lock);
        assert!(
            hashes.len() > 1500,
            "expected many checksums, got {}",
            hashes.len()
        );
        assert!(hashes.contains_key(&("abnf".to_string(), "0.13.0".to_string())));
        assert!(!hashes.contains_key(&("internal-crate-001".to_string(), "0.1.0".to_string())));
    }
}
