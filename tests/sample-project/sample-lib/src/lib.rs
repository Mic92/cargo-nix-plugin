use serde::{Deserialize, Serialize};

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
