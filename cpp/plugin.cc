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

    // If manifestPath is a store path and we're using a chroot store,
    // remap it to the real filesystem path before passing to Rust.
    if (inputJson.contains("manifestPath") && inputJson["manifestPath"].is_string()) {
        auto manifest = inputJson["manifestPath"].get<std::string>();
        inputJson["manifestPath"] = remapStorePath(*state.store, manifest);
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

static RegisterPrimOp rp({
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
    .fun = prim_resolveCargoWorkspace,
});
