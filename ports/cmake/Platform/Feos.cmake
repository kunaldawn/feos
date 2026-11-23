# CMake's platform description for feos (track T5): a Unix with static executables only
# (no shared objects until kernel stage 1f.1). Found through CMAKE_MODULE_PATH, which
# the feos toolchain files set.
include(Platform/UnixPaths)
set_property(GLOBAL PROPERTY TARGET_SUPPORTS_SHARED_LIBS FALSE)
set(CMAKE_DL_LIBS "")
set(CMAKE_EXECUTABLE_SUFFIX "")
set(CMAKE_STATIC_LIBRARY_PREFIX "lib")
set(CMAKE_STATIC_LIBRARY_SUFFIX ".a")
set(CMAKE_FIND_LIBRARY_PREFIXES "lib")
set(CMAKE_FIND_LIBRARY_SUFFIXES ".a")
