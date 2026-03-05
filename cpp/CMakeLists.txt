cmake_minimum_required(VERSION 3.16)
project(cargo-nix-plugin)

option(ENABLE_SANITIZERS "Build with UBSan (undefined behavior sanitizer)" OFF)

find_package(PkgConfig REQUIRED)
pkg_check_modules(NIX REQUIRED nix-expr>=2.32 nix-store>=2.32)

find_package(nlohmann_json REQUIRED)

add_library(cargo_nix_plugin MODULE plugin.cc)
target_include_directories(cargo_nix_plugin PUBLIC ${NIX_INCLUDE_DIRS})
target_compile_options(cargo_nix_plugin PUBLIC ${NIX_CFLAGS_OTHER})
target_link_libraries(cargo_nix_plugin nlohmann_json::nlohmann_json)

# Link the Rust static library
target_link_directories(cargo_nix_plugin PRIVATE ${RUST_LIB_DIR})
target_link_libraries(cargo_nix_plugin cargo_nix_plugin_core)

if(ENABLE_SANITIZERS)
  # UBSan only — ASan requires whole-program instrumentation which is
  # incompatible with being dlopen'd as a plugin into a non-instrumented host.
  #
  # We exclude -fsanitize=function and -fsanitize=vptr because they require
  # the host binary to also be instrumented (they check RTTI/magic prefixes
  # that non-instrumented code doesn't emit).
  #
  # The minimal UBSan runtime is statically linked into the .so via
  # --whole-archive to ensure all handler symbols are resolved at dlopen time.
  target_compile_options(cargo_nix_plugin PRIVATE
    -fsanitize=undefined -fno-sanitize=function,vptr
    -fsanitize-minimal-runtime -fno-sanitize-recover=all
    -fno-omit-frame-pointer -g)
  target_link_options(cargo_nix_plugin PRIVATE
    "LINKER:--whole-archive"
    "${SANITIZER_RT_DIR}/libclang_rt.ubsan_minimal-x86_64.a"
    "LINKER:--no-whole-archive")
endif()

if(APPLE)
  set_target_properties(cargo_nix_plugin PROPERTIES
    LINK_FLAGS "-flat_namespace -undefined suppress")
endif()

install(TARGETS cargo_nix_plugin DESTINATION lib/nix/plugins)
