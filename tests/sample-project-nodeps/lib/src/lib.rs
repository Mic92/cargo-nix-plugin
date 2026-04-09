#[cfg(not(nodeps_build_ok))]
compile_error!("build.rs cargo:rustc-cfg did not reach the lib compile");

pub const BUILD_PKG: &str = env!("NODEPS_BUILD_PKG");

pub fn greet() -> &'static str {
    "Hello from cargo-nix-plugin!"
}

#[cfg(test)]
mod tests {
    #[test]
    fn unit() {
        assert_eq!(super::greet(), "Hello from cargo-nix-plugin!");
    }
}
