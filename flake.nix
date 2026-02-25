{
  description = "Nix plugin for resolving Cargo workspaces — replaces generated Cargo.nix";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crate2nix-torture = {
      url = "git+ssh://forgejo@git.ntd.one/anthropic/crate2nix-torture.git";
      flake = false;
    };
  };

  outputs =
    { self, nixpkgs, crate2nix-torture }:
    let
      # Plugin targets linux (Nix plugins are .so shared libs)
      pluginSystem = "x86_64-linux";
      pluginPkgs = import nixpkgs { system = pluginSystem; };
      nixComponents = pluginPkgs.nixVersions.nixComponents_2_33;

      # Helper for multi-system outputs (devShells)
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs [
          "x86_64-linux"
          "aarch64-linux"
          "aarch64-darwin"
          "x86_64-darwin"
        ] (system: f (import nixpkgs { inherit system; }));
    in
    {
      packages.${pluginSystem} = {
        default = self.packages.${pluginSystem}.cargo-nix-plugin;

        cargo-nix-plugin = pluginPkgs.callPackage ./nix/plugin.nix {
          inherit nixComponents;
        };

        eval-test = pluginPkgs.callPackage ./nix/eval-test.nix {
          plugin = self.packages.${pluginSystem}.cargo-nix-plugin;
          testFixtures = ./rust/tests/fixtures;
        };

        torture-test = pluginPkgs.callPackage ./tests/torture-test.nix {
          plugin = self.packages.${pluginSystem}.cargo-nix-plugin;
          testFixtures = ./rust/tests/fixtures;
          wrapperLib = ./lib;
        };

        benchmark = pluginPkgs.callPackage ./tests/benchmark.nix {
          plugin = self.packages.${pluginSystem}.cargo-nix-plugin;
          benchFixtures = ./tests/bench-fixtures;
          nixpkgsPath = nixpkgs;
          cargoNixFile = "${crate2nix-torture}/Cargo.nix";
        };

        generate-metadata = pluginPkgs.writeShellApplication {
          name = "generate-metadata";
          runtimeInputs = [ pluginPkgs.cargo ];
          text = ''
            exec cargo metadata --format-version 1 --locked "$@"
          '';
        };
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
      });

      apps.${pluginSystem} = {
        generate-metadata = {
          type = "app";
          program = "${self.packages.${pluginSystem}.generate-metadata}/bin/generate-metadata";
        };
      };

      lib = import ./lib;
    };
}
