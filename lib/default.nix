# Nix wrapper that connects the cargo-nix-plugin output to buildRustCrate.
#
# Usage (automatic — shells out to cargo during eval):
#   let
#     cargoNix = import ./lib {
#       inherit pkgs;
#       src = ./.;  # workspace root with Cargo.toml + Cargo.lock
#     };
#   in cargoNix.workspaceMembers
#
# Usage (explicit — pure, no subprocess):
#   let
#     cargoNix = import ./lib {
#       inherit pkgs;
#       metadata = builtins.readFile ./metadata.json;
#       cargoLock = builtins.readFile ./Cargo.lock;
#       src = ./.;  # workspace root
#     };
#   in cargoNix.workspaceMembers
#
# Clippy: use `clippy` to get per-workspace-member clippy checks that
# reuse Nix-cached dependency builds:
#   cargoNix.clippy.allWorkspaceMembers  # check all members
#   cargoNix.clippy.workspaceMembers.<name>.build  # check one member

{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  stdenv ? pkgs.stdenv,
  # Optional: output of `cargo metadata --format-version 1 --locked`
  # If omitted, the plugin shells out to cargo automatically.
  metadata ? null,
  # Optional: contents of Cargo.lock (required when metadata is provided)
  # If omitted with metadata=null, read from src/Cargo.lock automatically.
  cargoLock ? null,
  # Required: workspace source root
  src ? null,
  # Optional: function to create buildRustCrate for a given pkgs
  buildRustCrateForPkgs ? pkgs: pkgs.buildRustCrate,
  # Optional: crate overrides
  # If omitted, the default crate overrides from nixpkgs will be used
  crateOverrides ? null,
  # Optional: features to enable
  rootFeatures ? [ "default" ],
  # Optional: target platform description (auto-detected from stdenv)
  target ? null,
  # Optional: extra arguments passed to clippy-driver (e.g. ["-D" "warnings"])
  clippyArgs ? [ ],
}:

let
  # Build the target description from stdenv if not provided
  defaultTarget = makeDefaultTarget stdenv.hostPlatform;

  makeDefaultTarget = platform: {
    name = platform.rust.rustcTargetSpec or platform.rust.rustcTarget or "x86_64-unknown-linux-gnu";
    os =
      if platform.isLinux then
        "linux"
      else if platform.isDarwin then
        "macos"
      else if platform.isWindows then
        "windows"
      else if platform.isFreeBSD then
        "freebsd"
      else
        "unknown";
    arch =
      if platform.isx86_64 then
        "x86_64"
      else if platform.isAarch64 then
        "aarch64"
      else if platform.isi686 then
        "i686"
      else if platform.isAarch32 then
        "arm"
      else if platform.isRiscV64 then
        "riscv64"
      else
        "unknown";
    vendor =
      if platform.isLinux then
        "unknown"
      else if platform.isDarwin then
        "apple"
      else
        "unknown";
    env =
      if platform.isLinux && platform.isGnu then
        "gnu"
      else if platform.isLinux && platform.isMusl then
        "musl"
      else
        "";
    family =
      if platform.isUnix then
        [ "unix" ]
      else if platform.isWindows then
        [ "windows" ]
      else
        [ ];
    pointer_width =
      if platform.is64bit then
        "64"
      else if platform.is32bit then
        "32"
      else
        "64";
    endian = if platform.isLittleEndian then "little" else "big";
    unix = platform.isUnix;
    windows = platform.isWindows;
  };

  resolvedTarget = if target != null then target else defaultTarget;

  # Call the plugin builtin — auto-detect mode based on metadata presence
  resolved = builtins.resolveCargoWorkspace (
    {
      target = resolvedTarget;
      inherit rootFeatures;
    }
    // (
      if metadata != null then
        {
          inherit metadata cargoLock;
        }
      else
        {
          manifestPath = "${src}/Cargo.toml";
        }
    )
  );

  # Source resolution: given a crate's source info, produce a src path
  # buildRustCrate always needs a src — for crates-io it uses fetchurl
  resolveSrc =
    crateInfo:
    let
      sourceType = crateInfo.source.type or "local";
      # For local crates: compute relative path from workspace root
      # source.path is absolute (e.g. /nix/store/.../harmonia/harmonia-client)
      # workspaceRoot is absolute (e.g. /nix/store/.../harmonia)
      workspaceRoot = resolved.workspaceRoot;
      sourcePath = crateInfo.source.path or workspaceRoot;
      # Strip workspace root prefix to get relative path (e.g. "harmonia-client")
      relPath = lib.removePrefix (workspaceRoot + "/") sourcePath;
      isSubdir = relPath != sourcePath && relPath != "";
    in
    if sourceType == "local" then
      if isSubdir then src + "/${relPath}" else src
    else if sourceType == "crates-io" then
      pkgs.fetchurl {
        name = "${crateInfo.crateName}-${crateInfo.version}.tar.gz";
        url = "https://static.crates.io/crates/${crateInfo.crateName}/${crateInfo.crateName}-${crateInfo.version}.crate";
        sha256 = crateInfo.sha256;
      }
    else if sourceType == "git" then
      builtins.fetchGit {
        url = crateInfo.source.url;
        rev = crateInfo.source.rev;
      }
    else
      src;

  # Build a crate using buildRustCrate
  # Memoization via the `self` pattern (builtByPackageId)
  mkBuiltByPackageIdByPkgs =
    cratePkgs:
    let
      buildRustCrate =
        let
          base = buildRustCrateForPkgs cratePkgs;
        in
        if crateOverrides != null then args: (base args).override { inherit crateOverrides; } else base;

      self = {
        crates = lib.mapAttrs (
          packageId: _: buildCrate self cratePkgs buildRustCrate packageId
        ) resolved.crates;
        target = makeDefaultTarget cratePkgs.stdenv.hostPlatform;
        build = mkBuiltByPackageIdByPkgs cratePkgs.buildPackages;
      };
    in
    self;

  buildCrate =
    self: cratePkgs: buildRustCrate: packageId:
    let
      crateInfo = resolved.crates.${packageId};

      # Resolve a regular dependency to its built derivation.
      # Proc-macro crates must be built for the build platform since they
      # execute as compiler plugins during compilation.
      depDrv =
        dep:
        let
          depCrateInfo = resolved.crates.${dep.packageId} or null;
        in
        if depCrateInfo != null && (depCrateInfo.procMacro or false) then
          self.build.crates.${dep.packageId}
        else
          self.crates.${dep.packageId};

      # Resolve a build-script dependency. Build scripts run on the build
      # platform, so all their dependencies must be built for that platform.
      buildDepDrv = dep: self.build.crates.${dep.packageId};

      # Dependencies are already filtered by the Rust resolver:
      # platform-incompatible and inactive optional deps are excluded.
      dependencies = map depDrv (crateInfo.dependencies or [ ]);
      buildDependencies = map buildDepDrv (crateInfo.buildDependencies or [ ]);

      # Renames: { crate_name = [{ version = "x.y.z"; rename = "alias"; }]; }
      renamedDeps = lib.filter (d: d ? rename && d.rename != null) (
        (crateInfo.dependencies or [ ]) ++ (crateInfo.buildDependencies or [ ])
      );
      crateRenames =
        let
          grouped = lib.groupBy (dep: dep.name) renamedDeps;
          versionAndRename = dep: {
            inherit (dep) rename;
            version = (resolved.crates.${dep.packageId}).version;
          };
        in
        lib.mapAttrs (_name: builtins.map versionAndRename) grouped;

      crateSrc = resolveSrc crateInfo;
    in
    buildRustCrate (
      {
        crateName = crateInfo.crateName;
        version = crateInfo.version;
        edition = crateInfo.edition or "2021";
        sha256 = crateInfo.sha256 or "";
        src = crateSrc;
        authors = crateInfo.authors or [ ];
        inherit dependencies buildDependencies crateRenames;
        features = crateInfo.resolvedDefaultFeatures or [ ];
        procMacro = crateInfo.procMacro or false;
        crateBin = crateInfo.crateBin or [ ];
      }
      // lib.optionalAttrs ((crateInfo.build or null) != null) {
        build = crateInfo.build;
      }
      // lib.optionalAttrs ((crateInfo.libPath or null) != null) {
        libPath = crateInfo.libPath;
      }
      // lib.optionalAttrs ((crateInfo.libName or null) != null) {
        libName = crateInfo.libName;
      }
      // lib.optionalAttrs ((crateInfo.links or null) != null) {
        links = crateInfo.links;
      }
      // lib.optionalAttrs (crateInfo.libCrateTypes or [ ] != [ ]) {
        type = crateInfo.libCrateTypes;
      }
    );

  builtCrates = mkBuiltByPackageIdByPkgs pkgs;

  # --- Clippy support ---
  # clippy-driver is a drop-in replacement for rustc.  We build a small
  # wrapper package that exposes `bin/rustc` → clippy-driver so that
  # buildRustCrate (which calls `noisily rustc …`) runs clippy instead.
  # Dependencies are built with the real rustc (and cached); only
  # workspace members use the clippy wrapper.
  #
  # All workspace members are consistently built with clippy-driver so
  # that inter-workspace-member dependencies have matching rlib metadata.

  clippyRustcWrapper =
    let
      clippy = pkgs.clippy;
      rustc = pkgs.rustc;
      extraArgs = lib.concatMapStringsSep " " lib.escapeShellArg clippyArgs;
    in
    pkgs.runCommand "clippy-as-rustc"
      {
        nativeBuildInputs = [ pkgs.makeWrapper ];
      }
      ''
        mkdir -p $out/bin $out/lib
        # Symlink the real rustc's libs (sysroot) so clippy-driver finds them
        ln -s ${rustc}/lib/* $out/lib/

        # Wrap clippy-driver as "rustc" so buildRustCrate's `noisily rustc`
        # invocations run clippy instead.
        makeWrapper ${clippy}/bin/clippy-driver $out/bin/rustc \
          ${lib.optionalString (clippyArgs != [ ]) ''--add-flags "${extraArgs}"''}

        # Forward other tools from the real toolchain.
        for tool in rustdoc rustfmt; do
          if [ -e ${rustc}/bin/$tool ]; then
            ln -s ${rustc}/bin/$tool $out/bin/$tool
          fi
        done
      '';

  # Build workspace members under clippy, reusing the normal dependency builds.
  # Non-workspace crates are taken directly from builtCrates so they are
  # identical Nix store paths — no redundant rebuilds.
  mkClippyBuiltByPkgs =
    cratePkgs:
    let
      normalBuilt = mkBuiltByPackageIdByPkgs cratePkgs;

      # Normal buildRustCrate for dependencies (fully cached)
      normalBuildRustCrate =
        let
          base = buildRustCrateForPkgs cratePkgs;
        in
        if crateOverrides != null then args: (base args).override { inherit crateOverrides; } else base;

      # Clippy buildRustCrate: use clippy-driver as the compiler
      clippyBuildRustCrate = args: (normalBuildRustCrate args).override { rust = clippyRustcWrapper; };

      workspaceMemberIds = lib.attrValues resolved.workspaceMembers;

      # For clippy crate resolution: workspace members use clippy-driver,
      # everything else reuses the already-cached normal build output.
      self = {
        crates = lib.mapAttrs (
          packageId: _:
          let
            isWorkspaceMember = lib.elem packageId workspaceMemberIds;
          in
          if isWorkspaceMember then
            buildCrate self cratePkgs clippyBuildRustCrate packageId
          else
            normalBuilt.crates.${packageId}
        ) resolved.crates;
        target = makeDefaultTarget cratePkgs.stdenv.hostPlatform;
        # Build-platform crates use clippy for workspace members too,
        # so build scripts see the same rlib metadata as the lib phase.
        build = mkClippyBuiltByPkgs cratePkgs.buildPackages;
      };
    in
    self;

  clippyCrates = mkClippyBuiltByPkgs pkgs;

in
{
  # Public interface matching crate2nix
  workspaceMembers = lib.mapAttrs (name: packageId: {
    inherit packageId;
    build = builtCrates.crates.${packageId};
  }) resolved.workspaceMembers;

  rootCrate =
    if resolved.root != null then
      {
        packageId = resolved.root;
        build = builtCrates.crates.${resolved.root};
      }
    else
      null;

  allWorkspaceMembers = pkgs.symlinkJoin {
    name = "all-workspace-members";
    paths = lib.mapAttrsToList (
      _name: packageId: builtCrates.crates.${packageId}
    ) resolved.workspaceMembers;
  };

  # Clippy: workspace members checked with clippy-driver, dependencies
  # compiled normally (cached).  Build any member to get clippy diagnostics;
  # the build fails if clippy reports errors.
  clippy = {
    workspaceMembers = lib.mapAttrs (name: packageId: {
      inherit packageId;
      build = clippyCrates.crates.${packageId};
    }) resolved.workspaceMembers;

    allWorkspaceMembers = pkgs.symlinkJoin {
      name = "all-workspace-members-clippy";
      paths = lib.mapAttrsToList (
        _name: packageId: clippyCrates.crates.${packageId}
      ) resolved.workspaceMembers;
    };
  };

  # Expose internals for debugging
  inherit resolved;
  inherit builtCrates;
}
