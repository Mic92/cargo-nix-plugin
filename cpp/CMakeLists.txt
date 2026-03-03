cmake_minimum_required(VERSION 3.16)
project(cargo-nix-plugin)

find_package(PkgConfig REQUIRED)
pkg_check_modules(NIX REQUIRED nix-expr>=2.32)

find_package(nlohmann_json REQUIRED)

add_library(cargo_nix_plugin MODULE plugin.cc)
target_include_directories(cargo_nix_plugin PUBLIC ${NIX_INCLUDE_DIRS})
target_compile_options(cargo_nix_plugin PUBLIC ${NIX_CFLAGS_OTHER})
target_link_libraries(cargo_nix_plugin nlohmann_json::nlohmann_json)

# Link the Rust static library
target_link_directories(cargo_nix_plugin PRIVATE ${RUST_LIB_DIR})
target_link_libraries(cargo_nix_plugin cargo_nix_plugin_core)

if(APPLE)
  set_target_properties(cargo_nix_plugin PROPERTIES
    LINK_FLAGS "-flat_namespace -undefined suppress")
endif()

install(TARGETS cargo_nix_plugin DESTINATION lib/nix/plugins)
