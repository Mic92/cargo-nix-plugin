// Copyright 2026 Anthropic, PBC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#include <nix/expr/eval.hh>
#include <nix/expr/primops.hh>
#include <nix/expr/value.hh>
#include <nix/expr/json-to-value.hh>
#include <nix/expr/value-to-json.hh>
#include <nix/store/local-fs-store.hh>
#include <nlohmann/json.hpp>

// Rust FFI declarations
extern "C" {
    int resolve_cargo_workspace(
        const char *input_json,
        char **out,
        char **err_out
    );
    void free_string(char *s);
    unsigned cargo_nix_api_level(void);
}

using namespace nix;

/**
 * If the store is a chroot store (--store /tmp/foo), remap a logical
 * store path to the real filesystem path so that cargo metadata can
 * read it during eval.
 *
 * Example: /nix/store/xxx-source/Cargo.toml
 *       -> /tmp/foo/nix/store/xxx-source/Cargo.toml
 */
static std::string remapStorePath(Store &store, const std::string &path) {
    auto *localFS = dynamic_cast<LocalFSStore *>(&store);
    if (!localFS)
        return path;

    auto realStoreDir = localFS->getRealStoreDir();
    auto logicalStoreDir = store.storeDir;

    // No remapping needed if real == logical (normal store)
    if (realStoreDir == logicalStoreDir)
        return path;

    // Only remap paths that start with the logical store dir
    if (path.substr(0, logicalStoreDir.size()) != logicalStoreDir)
        return path;

    return realStoreDir + path.substr(logicalStoreDir.size());
}

static void prim_resolveCargoWorkspace(EvalState &state, const PosIdx pos,
                                        Value **args, Value &v) {
    state.forceAttrs(*args[0], pos,
        "while evaluating the argument to builtins.resolveCargoWorkspace");

    // Serialize the entire input attrset to JSON and hand it to Rust
    NixStringContext context;
    auto inputJson = printValueAsJSON(state, true, *args[0], pos, context, false);

    // The Rust side opens these paths with std::fs. Under a chroot store
    // (`--store local?root=/tmp/foo`) the logical /nix/store/... paths
    // don't exist on disk — remap them to the real filesystem location.
    auto remapField = [&](nlohmann::json &v) {
        if (v.is_string())
            v = remapStorePath(*state.store, v.get<std::string>());
    };
    if (inputJson.contains("manifestPath"))
        remapField(inputJson["manifestPath"]);
    // cargoHome can be a store path (e.g. a pre-warmed registry index drv).
    if (inputJson.contains("cargoHome"))
        remapField(inputJson["cargoHome"]);
    // gitSources values are builtins.fetchGit checkouts — always store paths.
    if (inputJson.contains("gitSources") && inputJson["gitSources"].is_object()) {
        for (auto &[_, checkout] : inputJson["gitSources"].items())
            remapField(checkout);
    }

    auto inputStr = inputJson.dump();

    char *resultJson = nullptr;
    char *errorMsg = nullptr;

    int rc = resolve_cargo_workspace(inputStr.c_str(), &resultJson, &errorMsg);

    if (rc != 0) {
        std::string err = errorMsg ? errorMsg : "unknown error";
        if (errorMsg) free_string(errorMsg);
        state.error<EvalError>("resolveCargoWorkspace: %s", err).atPos(pos).debugThrow();
    }

    // Parse the result JSON into a Nix value
    std::string result(resultJson);
    free_string(resultJson);

    parseJSON(state, result, v);
}

static void prim_cargoNixApiLevel(EvalState &, const PosIdx, Value **, Value &v) {
    v.mkInt(cargo_nix_api_level());
}

// Nix >=2.34 renamed PrimOp::fun to PrimOp::impl (see CMakeLists.txt).
static RegisterPrimOp rp(PrimOp {
    .name = "resolveCargoWorkspace",
    .args = {"attrs"},
    .arity = 1,
    .doc = R"(
      Resolve a Cargo workspace into a crate metadata attrset compatible with buildRustCrate.

      Accepts an attrset with:
      - `metadata`: JSON string from `cargo metadata --format-version 1 --locked`
      - `cargoLock`: Contents of `Cargo.lock`
      - `target`: Attrset describing the target platform
      - `rootFeatures` (optional): List of features to enable (defaults to `["default"]`)
    )",
#ifdef NIX_PRIMOP_HAS_IMPL
    .impl = prim_resolveCargoWorkspace,
#else
    .fun = prim_resolveCargoWorkspace,
#endif
});

// Internal probe so lib/ can detect a skewed .so before calling
// resolveCargoWorkspace. Not user API; see cargoNix.{apiLevel,
// resolverApiLevel}.
static RegisterPrimOp rpApiLevel(PrimOp {
    .name = "__cargoNixApiLevel",
    .args = {},
    .arity = 0,
    .doc = "Internal: contract version of the loaded cargo-nix-plugin resolver.",
#ifdef NIX_PRIMOP_HAS_IMPL
    .impl = prim_cargoNixApiLevel,
#else
    .fun = prim_cargoNixApiLevel,
#endif
});
