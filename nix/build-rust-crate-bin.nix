{
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "build-rust-crate";
  version = "0.1.0";
  src = ../builder;
  cargoLock.lockFile = ../builder/Cargo.lock;
  doCheck = true;
}
