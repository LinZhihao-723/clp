# Standalone build for the clp-s C API harness. This is an EXTERNAL consumer of the prebuilt
# libclp-s.so and is intentionally NOT part of the core CMake tree / `task core` build.
cmake_minimum_required(VERSION 3.16)
project(clp_s_c_api_harness C)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)

# Locations of the prebuilt shared library and the public C header. Override on the command line
# with -DCLP_S_LIB_DIR=... / -DCLP_S_INCLUDE_DIR=... if your build tree lives elsewhere.
set(CLP_S_LIB_DIR "/home/lzh/dev/clp/build/core"
    CACHE PATH "Directory containing the prebuilt libclp-s.so")
set(CLP_S_INCLUDE_DIR "/home/lzh/dev/clp/components/core/src"
    CACHE PATH "Include dir so that #include <clp_s/capi.h> resolves")

# Imported target wrapping the prebuilt shared library.
add_library(clp-s SHARED IMPORTED)
set_target_properties(clp-s PROPERTIES
    IMPORTED_LOCATION "${CLP_S_LIB_DIR}/libclp-s.so"
    INTERFACE_INCLUDE_DIRECTORIES "${CLP_S_INCLUDE_DIR}"
)

add_executable(harness harness.c)
target_link_libraries(harness PRIVATE clp-s)

# Ensure the harness finds libclp-s.so at runtime without needing LD_LIBRARY_PATH.
set_target_properties(harness PROPERTIES
    BUILD_RPATH "${CLP_S_LIB_DIR}"
    INSTALL_RPATH "${CLP_S_LIB_DIR}"
)
