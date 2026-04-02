mod build_rust_crate;

use std::process;

use build_rust_crate::config::BuildConfig;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: build-rust-crate <configure|build|install>");
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
        "configure" => build_rust_crate::configure::run(&mut config),
        "build" => build_rust_crate::build::run(&mut config),
        "install" => build_rust_crate::install::run(&config),
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
