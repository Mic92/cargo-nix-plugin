# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Inner entrypoint for the dynamic-derivations planner.
# Evaluated by `nix-instantiate` *inside* the planner build (via
# recursive-nix), with the resolver output materialized as a JSON file
# so `lib/default.nix` does not need the plugin primop.
#
# `attrSegments`/`drvName`: dynamic derivations require the produced
# `.drv` file to have a store-path basename matching its `name` field.
# The CA text output of the planner is named after the *planner* drv,
# not the crate, so the selected derivation is renamed here to align
# the two.
#
#   nix-instantiate ./dyn-drv-eval.nix \
#     --arg argsFile '"/build/args.json"' \
#     --arg resolvedJson '"/build/resolved.json"' \
#     --argstr attrSegments '["workspaceMembers","foo","build"]' \
#     --argstr drvName 'cargo-nix-plan-workspaceMembers--foo--build'
{
  argsFile,
  resolvedJson,
  attrSegments,
  drvName,
}:

let
  args = builtins.fromJSON (builtins.readFile argsFile);
  pkgs = import args.nixpkgs { inherit (args) system; };
  inherit (pkgs) lib;
  crateOverrides =
    if args.crateOverridesFile != null then import args.crateOverridesFile pkgs else null;

  cargoNix = import (args.libDir + "/default.nix") (
    {
      inherit pkgs;
      src = /. + args.src;
      inherit (args)
        target
        rootFeatures
        noDefaultFeatures
        clippyArgs
        extraCfgs
        extraRegistries
        ;
      resolvedJson = /. + resolvedJson;
    }
    // (if crateOverrides != null then { inherit crateOverrides; } else { })
  );

  selected = lib.attrsets.getAttrFromPath (builtins.fromJSON attrSegments) cargoNix;
in
selected.overrideAttrs (_: {
  name = drvName;
})
