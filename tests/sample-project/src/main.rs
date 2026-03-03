use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
struct Greeting {
    message: String,
}

fn main() {
    let g = Greeting {
        message: "Hello from cargo-nix-plugin!".to_string(),
    };
    let json = serde_json::to_string(&g).expect("serialize");
    println!("{json}");
}
