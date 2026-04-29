// Copyright 2026 Anthropic, PBC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod build_rust_crate;

use std::process;

use build_rust_crate::config::BuildConfig;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: build-rust-crate <locate|configure|build|install>");
        process::exit(1);
    }

    let json_path = match std::env::var("NIX_ATTRS_JSON_FILE") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("error: NIX_ATTRS_JSON_FILE not set (is __structuredAttrs enabled?)");
            process::exit(1);
        }
    };

    let mut config = match BuildConfig::from_json_file(&json_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to parse structured attrs: {e}");
            process::exit(1);
        }
    };

    let result = match args[1].as_str() {
        "locate" => build_rust_crate::configure::locate(&config),
        "configure" => build_rust_crate::configure::run(&mut config),
        "build" => build_rust_crate::build::run(&mut config),
        "install" => build_rust_crate::install::run(&mut config),
        other => {
            eprintln!("error: unknown subcommand: {other}");
            process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}
