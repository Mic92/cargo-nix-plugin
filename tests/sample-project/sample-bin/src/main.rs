fn main() {
    let g = sample_lib::Greeting::new("Hello from cargo-nix-plugin!");
    println!("{}", g.to_json());
}
