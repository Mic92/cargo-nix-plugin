{
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "read-crate-info";
  version = "0.1.0";
  src = ../rust;
  cargoLock.lockFile = ../rust/Cargo.lock;
  cargoBuildFlags = [ "--bin" "read-crate-info" ];
  doCheck = false;
}
