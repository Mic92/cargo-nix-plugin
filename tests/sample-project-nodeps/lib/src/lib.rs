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
