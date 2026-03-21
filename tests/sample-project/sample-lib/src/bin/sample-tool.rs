fn main() {
    let greeting = sample_lib::Greeting::new("from the sidecar bin");
    println!("{}", greeting.to_json());
}
