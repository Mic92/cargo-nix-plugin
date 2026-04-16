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
      ];

      forAllSystems =
        f: nixpkgs.lib.genAttrs supportedSystems (system: f (import nixpkgs { inherit system; }));

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
        "2_34" = {
          components = "nixComponents_2_34";
          binary = "nix_2_34";
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
            pluginSrc = ./.;
          };

          sample-build-test = pkgs.callPackage ./tests/sample-build-test.nix {
            inherit plugin nix;
            pluginSrc = ./.;
            sampleProject = ./tests/sample-project;
          };

          offline-build-test = pkgs.callPackage ./tests/offline-build-test.nix {
            inherit plugin nix;
            pluginSrc = ./.;
            sampleProject = ./tests/sample-project;
          };

          remote-sparse-test = pkgs.callPackage ./tests/remote-sparse-test.nix {
            inherit plugin nix;
            pluginSrc = ./.;
            sampleProject = ./tests/sample-project;
          };

          mirror-test = pkgs.callPackage ./tests/mirror-test.nix {
            inherit plugin nix;
            pluginSrc = ./.;
            sampleProject = ./tests/sample-project;
          };

          retry-test = pkgs.callPackage ./tests/retry-test.nix {
            inherit plugin nix;
            pluginSrc = ./.;
            sampleProject = ./tests/sample-project;
          };

          git-source-test = pkgs.callPackage ./tests/git-source-test.nix {
            inherit plugin nix;
            pluginSrc = ./.;
          };

        }
        # `nix build --store local?root=…` needs the bind-mount-based
        # chroot store, which only exists on Linux.
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          chroot-store-test = pkgs.callPackage ./tests/chroot-store-test.nix {
            inherit plugin nix;
            pluginSrc = ./.;
            # nodeps variant: cargo metadata inside the sandbox can't reach
            # crates.io, and the remap logic under test doesn't need it to.
            sampleProject = ./tests/sample-project-nodeps;
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
          tests = mkTests pkgs plugin nix;
          # The UBSan build statically links compiler-rt's minimal
          # runtime via GNU-ld --whole-archive from lib/linux/; no
          # darwin equivalent is wired up, so keep it Linux-only.
          sanitizedTests = nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
            mkTests pkgs (mkPluginSanitized pkgs components) nix
          );
        in
        acc
        // { "cargo-nix-plugin-nix_${ver}" = plugin; }
        // nixpkgs.lib.mapAttrs' (name: drv: nixpkgs.lib.nameValuePair "${name}-nix_${ver}" drv) tests
        // nixpkgs.lib.mapAttrs' (
          name: drv: nixpkgs.lib.nameValuePair "${name}-ubsan-nix_${ver}" drv
        ) sanitizedTests
      ) { } (builtins.attrNames nixVersions);

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
          cargo-nix-prefetch = pkgs.callPackage ./nix/cargo-nix-prefetch.nix { };
          # Optional: helper for generating metadata JSON explicitly.
          # Not needed when using the automatic subprocess mode (just pass src).
          # Useful for offline/pure evaluation workflows.
          generate-metadata = pkgs.writeShellApplication {
            name = "generate-metadata";
            runtimeInputs = [ pkgs.cargo ];
            text = ''
              exec cargo metadata --format-version 1 --locked "$@"
            '';
          };
          # Exercises cargo-nix-prefetch (the standalone binary), not the
          # nix plugin, so it isn't per-nix-version.
          cargo-compat-test = pkgs.callPackage ./tests/cargo-compat-test.nix { };
          build-rust-crate-bin = pkgs.callPackage ./nix/build-rust-crate-bin.nix { };
        }
        // perVersionPackages pkgs
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

      apps = forAllSystems (pkgs: {
        cargo-nix-prefetch = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.cargo-nix-prefetch}/bin/cargo-nix-prefetch";
        };
        generate-metadata = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.generate-metadata}/bin/generate-metadata";
        };
      });

      # Checks run against every nix version in the matrix, on every
      # supported system. Linux gets the UBSan variants on top via
      # perVersionPackages; recursive-nix tests are gated by
      # requiredSystemFeatures so a darwin builder without that feature
      # simply won't be assigned them.
      checks = forAllSystems (
        pkgs:
        builtins.foldl' (
          acc: ver:
          let
            cfg = nixVersions.${ver};
            components = pkgs.nixVersions.${cfg.components};
            nix = pkgs.nixVersions.${cfg.binary};
            plugin = mkPlugin pkgs components;
            tests = mkTests pkgs plugin nix;
          in
          acc
          // nixpkgs.lib.mapAttrs' (
            name: drv: nixpkgs.lib.nameValuePair "${name}-nix_${ver}" drv
          ) tests
        ) { } (builtins.attrNames nixVersions)
      );

      lib = import ./lib;
    };
}
