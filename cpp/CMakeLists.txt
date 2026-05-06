cmake_minimum_required(VERSION 3.16)
project(cargo-nix-plugin)

option(ENABLE_SANITIZERS "Build with UBSan (undefined behavior sanitizer)" OFF)

find_package(PkgConfig REQUIRED)
pkg_check_modules(NIX REQUIRED nix-expr>=2.30 nix-store>=2.30)

find_package(nlohmann_json REQUIRED)

# Nix >=2.34 renamed PrimOp::fun to PrimOp::impl.
include(CheckCXXSourceCompiles)
set(CMAKE_REQUIRED_INCLUDES ${NIX_INCLUDE_DIRS})
set(CMAKE_REQUIRED_FLAGS "${NIX_CFLAGS_OTHER}")
check_cxx_source_compiles("
  #include <nix/expr/eval.hh>
  void check() { auto p = &nix::PrimOp::impl; (void)p; }
  int main() { return 0; }
" NIX_PRIMOP_HAS_IMPL)

add_library(cargo_nix_plugin MODULE plugin.cc)
target_include_directories(cargo_nix_plugin PUBLIC ${NIX_INCLUDE_DIRS})
target_compile_options(cargo_nix_plugin PUBLIC ${NIX_CFLAGS_OTHER})

if(NIX_PRIMOP_HAS_IMPL)
  target_compile_definitions(cargo_nix_plugin PRIVATE NIX_PRIMOP_HAS_IMPL)
endif()
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
  if(NOT SANITIZER_RT_LIB)
    # An empty value here is silently accepted by ld between
    # --whole-archive/--no-whole-archive, leaving __ubsan_* unresolved
    # until dlopen. Fail at configure time instead.
    message(FATAL_ERROR "ENABLE_SANITIZERS requires -DSANITIZER_RT_LIB=<path to libclang_rt.ubsan_minimal-<arch>.a>")
  endif()
  target_link_options(cargo_nix_plugin PRIVATE
    "LINKER:--whole-archive"
    "${SANITIZER_RT_LIB}"
    "LINKER:--no-whole-archive")
endif()

if(APPLE)
  set_target_properties(cargo_nix_plugin PROPERTIES
    LINK_FLAGS "-flat_namespace -undefined suppress")
endif()

install(TARGETS cargo_nix_plugin DESTINATION lib/nix/plugins)
