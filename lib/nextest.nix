# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Run pre-built test binaries through cargo-nextest's reuse-build path
# (--cargo-metadata plus --binaries-metadata). No cargo is needed.
#
# cargo-metadata.json is synthesized here at eval time from the
# resolver output. It contains only workspace members, in the shape of
# a `--no-deps` invocation. Auto-discovered lib and bin targets are
# mirrored via pathExists/readDir. Those only depend on file
# existence, so source edits do not change the JSON. Paths use a fake
# root for reproducibility, and --workspace-remap points nextest back
# at the real tree.
#
# binaries-metadata.json comes from the builder
# (builder/src/build_rust_crate/nextest.rs). It is installed next to
# the test binaries in $out/nix-support/.
{
  pkgs,
  lib,
  resolved,
  # crateInfo -> workspace-relative dir ("." for the root); matches the
  # workspace_member the builder writes into package ids.
  defaultMemberDir,
}:
let
  # Must match FAKE_ROOT in builder/src/build_rust_crate/nextest.rs.
  fakeRoot = "/cargo-nix-plugin-metadata/ws";
  underscore = lib.replaceStrings [ "-" ] [ "_" ];

  # Constant package fields cargo_metadata requires but nextest ignores.
  packageDefaults = {
    license = null;
    license_file = null;
    description = null;
    source = null;
    dependencies = [ ];
    metadata = null;
    publish = null;
    categories = [ ];
    keywords = [ ];
    readme = null;
    repository = null;
    homepage = null;
    documentation = null;
    default_run = null;
    rust_version = null;
  };

  memberOf =
    memberDir: name: packageId:
    let
      info = resolved.crates.${packageId};
      dir = info.source.path;
      member = memberDir info;
      fakeDir = if member == "." then fakeRoot else "${fakeRoot}/${member}";
      id = "path+file://${fakeDir}#${name}@${info.version}";
      target =
        kind: crateTypes: tname: srcRel: extra:
        {
          kind = [ kind ];
          crate_types = crateTypes;
          name = tname;
          src_path = "${fakeDir}/${srcRel}";
          edition = info.edition;
          doc = true;
          doctest = false;
          test = true;
        }
        // extra;

      libCrateTypes = if info.libCrateTypes == [ ] then [ "lib" ] else info.libCrateTypes;
      libPath =
        if info.libPath or null != null then
          info.libPath
        else if builtins.pathExists (dir + "/src/lib.rs") then
          "src/lib.rs"
        else
          null;
      libTargets = lib.optional (libPath != null) (
        target "lib" libCrateTypes (info.libName or (underscore name)) libPath { doctest = true; }
      );

      explicitBins = map (b: target "bin" [ "bin" ] b.name b.path { }) info.crateBin;
      # Cargo dedupes auto-discovered bins against explicit [[bin]]
      # entries by name or by path.
      explicitNames = map (b: b.name) info.crateBin;
      explicitPaths = map (b: b.path) info.crateBin;
      autoBin =
        tname: srcRel:
        lib.optional (!(builtins.elem tname explicitNames || builtins.elem srcRel explicitPaths)) (
          target "bin" [ "bin" ] tname srcRel { }
        );
      binDirTargets =
        let
          d = dir + "/src/bin";
          entries = if builtins.pathExists d then builtins.readDir d else { };
        in
        lib.concatMap (
          f:
          if entries.${f} == "regular" && lib.hasSuffix ".rs" f then
            autoBin (lib.removeSuffix ".rs" f) "src/bin/${f}"
          else if entries.${f} == "directory" && builtins.pathExists (d + "/${f}/main.rs") then
            autoBin f "src/bin/${f}/main.rs"
          else
            [ ]
        ) (builtins.attrNames entries);
      binTargets =
        lib.optionals (builtins.pathExists (dir + "/src/main.rs")) (autoBin name "src/main.rs")
        ++ binDirTargets
        ++ explicitBins;

      testTargets = map (t: target "test" [ "bin" ] t.name t.path { doc = false; }) info.testTargets;
    in
    {
      inherit id;
      package = packageDefaults // {
        inherit name id;
        inherit (info)
          version
          edition
          authors
          ;
        # Empty: guppy validates feature values that name dependencies
        # (`dep:foo`, `foo/bar`) against `dependencies`, which is empty.
        features = { };
        targets = libTargets ++ binTargets ++ testTargets;
        manifest_path = "${fakeDir}/Cargo.toml";
        links = info.links or null;
      };
    };

  # Overrides for consumers whose --workspace-remap target is not
  # rooted at workspaceRoot (memberDir) or whose runner covers a
  # subset of the workspace (workspaceMembers).
  mkMetadataFile =
    {
      memberDir ? defaultMemberDir,
      workspaceMembers ? resolved.workspaceMembers,
    }:
    let
      members = lib.attrValues (lib.mapAttrs (memberOf memberDir) workspaceMembers);
    in
    pkgs.writeText "cargo-metadata.json" (
      builtins.toJSON {
        packages = map (m: m.package) members;
        workspace_members = map (m: m.id) members;
        workspace_default_members = map (m: m.id) members;
        resolve = null;
        target_directory = "${fakeRoot}/target";
        build_directory = "${fakeRoot}/target";
        version = 1;
        workspace_root = fakeRoot;
        metadata = null;
      }
    );

  metadataFile = mkMetadataFile { };

  # Like runTests, but via cargo-nextest. That adds per-test-process
  # isolation, retries, and .config/nextest.toml profiles.
  mkRun =
    {
      name,
      # The member's `buildTests = true` derivation.
      testsDrv,
      # Remap target containing the workspace manifests and an
      # optional .config/nextest.toml.
      workspaceSrc,
      # Extra `cargo-nextest run` args, e.g. ["--profile" "ci"].
      extraArgs ? [ ],
    }@args:
    pkgs.stdenvNoCC.mkDerivation {
      name = "${name}-nextest";

      inherit (testsDrv) nativeCheckInputs;
      nativeBuildInputs = [ pkgs.cargo-nextest ];

      dontUnpack = true;
      dontConfigure = true;
      dontBuild = true;
      doCheck = true;

      checkPhase = ''
        runHook preCheck

        export CARGO_TARGET_TMPDIR="$(mktemp -d)"
        export RUST_BACKTRACE=''${RUST_BACKTRACE-1}
        # nextest prints the report to stderr. tee keeps a copy that
        # becomes $out, and pipefail preserves nextest's exit code.
        set -o pipefail
        cargo-nextest nextest run \
          --binaries-metadata ${testsDrv}/nix-support/binaries-metadata.json \
          --cargo-metadata ${metadataFile} \
          --workspace-remap ${workspaceSrc} \
          ${lib.escapeShellArgs extraArgs} |& tee nextest.log

        runHook postCheck
      '';

      installPhase = ''
        runHook preInstall
        cp nextest.log $out
        runHook postInstall
      '';

      passthru = {
        inherit testsDrv;
        withArgs = extra: mkRun (args // { extraArgs = extra; });
      };
    };
in
{
  inherit metadataFile mkMetadataFile;

  inherit mkRun;
}
