{
  description = "Nix plugin for resolving Cargo workspaces — replaces generated Cargo.nix";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];

      forAllSystems =
        f: nixpkgs.lib.genAttrs supportedSystems (system: f (import nixpkgs { inherit system; }));

      linuxSystem = "x86_64-linux";
      linuxPkgs = import nixpkgs { system = linuxSystem; };

      # Nix versions to build the plugin against and test with.
      # Each entry maps a suffix to { components, binary } attribute paths
      # under pkgs.nixVersions.
      nixVersions = {
        "2_32" = {
          components = "nixComponents_2_32";
          binary = "nix_2_32";
        };
        "2_33" = {
          components = "nixComponents_2_33";
          binary = "nix_2_33";
        };
      };

      # Build the plugin against a specific nix version's components.
      mkPlugin =
        pkgs: nixComponents:
        pkgs.callPackage ./nix/plugin.nix {
          inherit nixComponents;
        };

      mkPluginSanitized =
        pkgs: nixComponents:
        (mkPlugin pkgs nixComponents).override {
          stdenv = pkgs.llvmPackages.stdenv;
          llvmPackages = pkgs.llvmPackages;
          enableSanitizers = true;
        };

      # Generate test derivations for a given nix version.
      mkTests =
        pkgs: plugin: nix:
        {
          eval-test = pkgs.callPackage ./nix/eval-test.nix {
            inherit plugin nix;
            testFixtures = ./rust/tests/fixtures;
          };

          torture-test = pkgs.callPackage ./tests/torture-test.nix {
            inherit plugin nix;
            testFixtures = ./rust/tests/fixtures;
            wrapperLib = ./lib;
          };

          sample-build-test = pkgs.callPackage ./tests/sample-build-test.nix {
            inherit plugin nix;
            wrapperLib = ./lib;
            sampleProject = ./tests/sample-project;
          };
        };

      # Build packages/tests for every nix version, suffixed with the version.
      # e.g. eval-test-nix_2_32, torture-test-nix_2_33, etc.
      perVersionPackages = pkgs: builtins.foldl' (
        acc: ver:
        let
          cfg = nixVersions.${ver};
          components = pkgs.nixVersions.${cfg.components};
          nix = pkgs.nixVersions.${cfg.binary};
          plugin = mkPlugin pkgs components;
          pluginSanitized = mkPluginSanitized pkgs components;
          tests = mkTests pkgs plugin nix;
          sanitizedTests = mkTests pkgs pluginSanitized nix;
        in
        acc
        // { "cargo-nix-plugin-nix_${ver}" = plugin; }
        // nixpkgs.lib.mapAttrs' (name: drv: nixpkgs.lib.nameValuePair "${name}-nix_${ver}" drv) tests
        // nixpkgs.lib.mapAttrs' (
          name: drv: nixpkgs.lib.nameValuePair "${name}-ubsan-nix_${ver}" drv
        ) sanitizedTests
      ) {} (builtins.attrNames nixVersions);

      # The default nix version used for the top-level plugin package.
      defaultNixComponents = "nixComponents_2_32";
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          defaultPlugin = mkPlugin pkgs pkgs.nixVersions.${defaultNixComponents};
        in
        {
          default = defaultPlugin;
          cargo-nix-plugin = defaultPlugin;
        }
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == linuxSystem) (
          (perVersionPackages linuxPkgs)
          // {
            # Optional: helper for generating metadata JSON explicitly.
            # Not needed when using the automatic subprocess mode (just pass src).
            # Useful for offline/pure evaluation workflows.
            generate-metadata = linuxPkgs.writeShellApplication {
              name = "generate-metadata";
              runtimeInputs = [ linuxPkgs.cargo ];
              text = ''
                exec cargo metadata --format-version 1 --locked "$@"
              '';
            };
          }
        )
      );

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

      apps.${linuxSystem} = {
        generate-metadata = {
          type = "app";
          program = "${self.packages.${linuxSystem}.generate-metadata}/bin/generate-metadata";
        };
      };

      # Checks run against every nix version in the matrix.
      checks.${linuxSystem} = builtins.foldl' (
        acc: ver:
        let
          cfg = nixVersions.${ver};
          components = linuxPkgs.nixVersions.${cfg.components};
          nix = linuxPkgs.nixVersions.${cfg.binary};
          plugin = mkPlugin linuxPkgs components;
          tests = mkTests linuxPkgs plugin nix;
        in
        acc
        // nixpkgs.lib.mapAttrs' (
          name: drv: nixpkgs.lib.nameValuePair "${name}-nix_${ver}" drv
        ) tests
      ) {} (builtins.attrNames nixVersions);

      lib = import ./lib;
    };
}
