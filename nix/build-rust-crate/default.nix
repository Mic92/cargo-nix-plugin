# Code for buildRustCrate, a Nix function that builds Rust code, just
# like Cargo, but using Nix instead.
#
# This version uses __structuredAttrs and a Rust binary (build-rust-crate)
# instead of bash scripts for the configure/build/install phases.

{
  lib,
  stdenv,
  defaultCrateOverrides,
  fetchCrate,
  pkgsBuildBuild,
  rustc,
  cargo,
  libiconv,
  mold ? null,
  # Controls codegen parallelization for all crates.
  defaultCodegenUnits ? 16,
  # Use mold linker for faster linking (null to disable, Linux only)
  defaultMold ? if stdenv.hostPlatform.isLinux then mold else null,
  # The build-rust-crate binary that replaces bash phase scripts.
  buildRustCrateBin,
}:

crate_:
lib.makeOverridable
  (
    {
      rust ? rustc,
      cargo ? cargo,
      release,
      verbose,
      features,
      nativeBuildInputs,
      buildInputs,
      crateOverrides,
      dependencies,
      buildDependencies,
      crateRenames,
      capLints,
      extraRustcOpts,
      extraRustcOptsForBuildRs,
      buildTests,
      preUnpack,
      postUnpack,
      prePatch,
      patches,
      postPatch,
      preConfigure,
      postConfigure,
      preBuild,
      postBuild,
      preInstall,
      postInstall,
    }:

    let
      crate = crate_ // (lib.attrByPath [ crate_.crateName ] (attr: { }) crateOverrides crate_);
      dependencies_ = dependencies;
      buildDependencies_ = buildDependencies;
      # Every input-`crate` key we re-derive below. Anything not listed
      # leaks through extraDerivationAttrs and //-overrides the computed
      # value, which the builder reads from NIX_ATTRS_JSON_FILE.
      processedAttrs = [
        "src"
        "nativeBuildInputs"
        "buildInputs"
        "crateBin"
        "libName"
        "libPath"
        "buildDependencies"
        "dependencies"
        "features"
        "crateRenames"
        "crateName"
        "version"
        "build"
        "authors"
        "edition"
        "buildTests"
        "codegenUnits"
        "capLints"
        "links"
        "extraRustcOpts"
        "extraRustcOptsForBuildRs"
        "extraLinkFlags"
        "release"
        "verbose"
        "procMacro"
        "type"
        "sha256"
        "workspace_member"
        "description"
        "homepage"
        "license"
        "license-file"
        "readme"
        "repository"
        "rust-version"
        "plugin"
      ];
      extraDerivationAttrs = removeAttrs crate processedAttrs;

      # lib.unique is O(n²); lib.uniqueStrings drops store-path context.
      # genericClosure dedups by key while preserving context.
      uniquePaths =
        l:
        map (e: e.key) (
          builtins.genericClosure {
            startSet = map (key: { inherit key; }) l;
            operator = _: [ ];
          }
        );
      nativeBuildInputs_ = nativeBuildInputs;
      buildInputs_ = buildInputs;
      extraRustcOpts_ = extraRustcOpts;
      extraRustcOptsForBuildRs_ = extraRustcOptsForBuildRs;
      capLints_ = capLints;
      buildTests_ = buildTests;

      crateBin' = crate.crateBin or [ ];
      hasCrateBin' = crate ? crateBin;

    in
    stdenv.mkDerivation (
      rec {
        __structuredAttrs = true;

        inherit (crate) crateName;
        inherit
          release
          verbose
          preUnpack
          postUnpack
          prePatch
          patches
          postPatch
          preConfigure
          postConfigure
          preBuild
          postBuild
          preInstall
          postInstall
          buildTests
          ;

        src = crate.src or (fetchCrate { inherit (crate) crateName version sha256; });
        name = "rust_${crate.crateName}-${crate.version}${lib.optionalString buildTests_ "-test"}";
        version = crate.version;
        depsBuildBuild = [ pkgsBuildBuild.stdenv.cc ];
        nativeBuildInputs = [
          rust
          cargo
          buildRustCrateBin
        ]
        ++ lib.optionals (defaultMold != null) [ defaultMold ]
        ++ lib.optionals stdenv.hasCC [ stdenv.cc ]
        ++ lib.optionals stdenv.buildPlatform.isDarwin [ libiconv ]
        ++ (crate.nativeBuildInputs or [ ])
        ++ nativeBuildInputs_;
        buildInputs =
          lib.optionals stdenv.hostPlatform.isDarwin [ libiconv ]
          ++ (crate.buildInputs or [ ])
          ++ buildInputs_;

        # Per-dependency extern info computed at Nix eval time. The same
        # rename logic applies to build-deps; the resolver folds both
        # [dependencies] and [build-dependencies] renames into crateRenames.
        inherit
          (
            let
              normalizeName = lib.replaceStrings [ "-" ] [ "_" ];
              # Returns the alias if a rename actually applies to this dep,
              # null otherwise. `hasAttr` alone over-triggers when only one
              # of two versions is renamed; the un-renamed sibling must keep
              # isRename=false so dep_extern_args can recover the real
              # `--extern` key from the artifact filename.
              findRename = dep:
                let choices = crateRenames.${dep.crateName} or null;
                in
                if choices == null then null
                else if builtins.isList choices then
                  let m = lib.findFirst (c: (!(c ? version) || c.version == dep.version or "")) null choices;
                  in if m == null then null else normalizeName m.rename
                else normalizeName choices;
              mkExtern = dep:
                let r = findRename dep;
                in {
                  externName = if r != null then r else normalizeName dep.libName;
                  isRename = r != null;
                  stdlib = dep.stdlib or false;
                  libOut = toString (lib.getLib dep);
                };
            in
            {
              depExterns = map mkExtern dependencies_;
              buildDepExterns = map mkExtern buildDependencies_;
            }
          )
          depExterns
          buildDepExterns
          ;

        completeDeps =
          let
            deps = map lib.getLib dependencies_;
          in
          uniquePaths (map toString deps ++ lib.concatMap (dep: dep.completeDeps or [ ]) deps);

        completeBuildDeps =
          let
            bdeps = map lib.getLib buildDependencies_;
          in
          uniquePaths (
            map toString bdeps
            ++ lib.concatMap (dep: (dep.completeBuildDeps or [ ]) ++ (dep.completeDeps or [ ])) bdeps
          );

        crateFeatures = lib.optionals (crate ? features) (
          builtins.filter (f: !(lib.hasInfix "/" f || lib.hasPrefix "dep:" f)) (crate.features ++ features)
        );

        libName = if crate ? libName then crate.libName else crate.crateName;
        libPath = lib.optionalString (crate ? libPath) crate.libPath;

        metadata =
          let
            mkRustcFeatureArgs = lib.concatMapStringsSep " " (f: ''--cfg feature=\"${f}\"'');
            depsMetadata = lib.foldl' (str: dep: str + dep.metadata) "" (
              (map lib.getLib dependencies_) ++ (map lib.getLib buildDependencies_)
            );
            hashedMetadata = builtins.hashString "sha256" (
              crateName
              + "-"
              + crateVersion
              + "___"
              + toString (mkRustcFeatureArgs crateFeatures)
              + "___"
              + depsMetadata
              + "___"
              + stdenv.hostPlatform.rust.rustcTarget
            );
          in
          lib.substring 0 10 hashedMetadata;

        build = crate.build or "";
        workspace_member = crate.workspace_member or ".";
        crateBin = crateBin';
        hasCrateBin = hasCrateBin';
        crateAuthors = if crate ? authors && lib.isList crate.authors then crate.authors else [ ];
        crateDescription = crate.description or "";
        crateHomepage = crate.homepage or "";
        crateLicense = crate.license or "";
        crateLicenseFile = crate.license-file or "";
        crateLinks = crate.links or "";
        crateReadme = crate.readme or "";
        crateRepository = crate.repository or "";
        crateRustVersion = crate.rust-version or "";
        crateVersion = crate.version;
        crateType =
          if lib.attrByPath [ "procMacro" ] false crate then
            [ "proc-macro" ]
          else if lib.attrByPath [ "plugin" ] false crate then
            [ "dylib" ]
          else
            (crate.type or [ "lib" ]);
        extraLinkFlags = crate.extraLinkFlags or [ ];
        edition = crate.edition or null;
        codegenUnits = if crate ? codegenUnits then crate.codegenUnits else defaultCodegenUnits;
        extraRustcOpts =
          lib.optionals (crate ? extraRustcOpts) crate.extraRustcOpts
          ++ extraRustcOpts_
          ++ (if edition != null then [ "--edition" edition ] else [ ])
          ++ lib.optionals (defaultMold != null) [ "-C" "link-arg=-fuse-ld=mold" ];
        extraRustcOptsForBuildRs =
          lib.optionals (crate ? extraRustcOptsForBuildRs) crate.extraRustcOptsForBuildRs
          ++ extraRustcOptsForBuildRs_
          ++ (if edition != null then [ "--edition" edition ] else [ ]);
        capLints = capLints_;

        rustcPath = "${rust}";

        # CARGO_CFG_TARGET_* are derived at build time from `rustc --print cfg`,
        # so only the target triple and linker need passing here.
        hostPlatform = {
          rustcTargetSpec = stdenv.hostPlatform.rust.rustcTargetSpec;
          linkerPath =
            if stdenv.hostPlatform.linker == "lld" && rustc ? llvmPackages.lld then
              "${rustc.llvmPackages.lld}/bin/lld"
            else if stdenv.hasCC then
              "${stdenv.cc}/bin/${stdenv.cc.targetPrefix}cc"
            else
              "cc";
        };
        buildPlatform.rustcTargetSpec = stdenv.buildPlatform.rust.rustcTargetSpec;

        configurePhase = ''
          runHook preConfigure
          build-rust-crate configure
          runHook postConfigure
        '';
        buildPhase = ''
          runHook preBuild
          build-rust-crate build
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          build-rust-crate install
          runHook postInstall
        '';

        dontStrip = !release;
        stripExclude = [ "*.rlib" ];

        outputs =
          if buildTests then
            [ "out" ]
          else
            [
              "out"
              "lib"
            ];
        outputDev = if buildTests then [ "out" ] else [ "lib" ];

        # Expose the dependency derivations for downstream introspection
        # (e.g. cross-compilation tests asserting build-dep host platform).
        # Kept out of the env via passthru since __structuredAttrs would
        # otherwise JSON-serialize the full derivations.
        passthru = {
          dependencies = dependencies_;
          buildDependencies = buildDependencies_;
        };

        meta = {
          mainProgram = crateName;
          badPlatforms = [
            lib.systems.inspect.patterns.isMips64n32
          ];
        };
      }
      // extraDerivationAttrs
    )
  )
  {
    rust = crate_.rust or rustc;
    cargo = crate_.cargo or cargo;
    release = crate_.release or true;
    verbose = crate_.verbose or true;
    extraRustcOpts = [ ];
    extraRustcOptsForBuildRs = [ ];
    features = [ ];
    nativeBuildInputs = [ ];
    buildInputs = [ ];
    crateOverrides = defaultCrateOverrides;
    preUnpack = crate_.preUnpack or "";
    postUnpack = crate_.postUnpack or "";
    prePatch = crate_.prePatch or "";
    patches = crate_.patches or [ ];
    postPatch = crate_.postPatch or "";
    preConfigure = crate_.preConfigure or "";
    postConfigure = crate_.postConfigure or "";
    preBuild = crate_.preBuild or "";
    postBuild = crate_.postBuild or "";
    preInstall = crate_.preInstall or "";
    postInstall = crate_.postInstall or "";
    dependencies = crate_.dependencies or [ ];
    buildDependencies = crate_.buildDependencies or [ ];
    capLints = "allow";
    crateRenames = crate_.crateRenames or { };
    buildTests = crate_.buildTests or false;
  }
