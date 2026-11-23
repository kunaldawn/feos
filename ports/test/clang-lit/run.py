#!/usr/bin/env python3
"""Run clang lit tests (clang/test/...) against T4's LLVM build, which was configured
without tests (LLVM_INCLUDE_TESTS off, so no llvm-lit or lit.site.cfg.py of its own).

    ports/test/clang-lit/run.py [LIT ARGS...] TEST...
    ports/test/clang-lit/run.py -v ports/src/rust/src/llvm-project/clang/test/Driver/feos.c

This writes the site configuration cmake would have written (lit.site.cfg.py.in's
values, read from the build's CMakeCache.txt) and runs lit from llvm/utils/lit with it
mapped onto clang/test/lit.cfg.py, as llvm-lit does. In the toolchain image, after
ports/build-clang.sh (FileCheck, not, count and split-file come from the same build).
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PORTS = os.path.dirname(os.path.dirname(HERE))
SRC = os.path.join(PORTS, "src", "rust", "src", "llvm-project")
OUT = os.environ.get("PORTS_BUILD", "/build")
BUILD = os.path.join(OUT, "rust-build", "x86_64-unknown-linux-gnu", "llvm", "build")
SITE_DIR = os.path.join(OUT, "clang-lit")


def cache():
    values = {}
    with open(os.path.join(BUILD, "CMakeCache.txt")) as f:
        for line in f:
            if ":" in line and "=" in line and not line.startswith(("//", "#")):
                key, rest = line.split(":", 1)
                values[key] = rest.split("=", 1)[1].rstrip("\n")
    return values


def main():
    c = cache()
    on = lambda k: c.get(k, "OFF").upper() in ("ON", "TRUE", "1", "YES", "FORCE_ON")
    obj = os.path.join(SITE_DIR, "clang")
    os.makedirs(os.path.join(obj, "test"), exist_ok=True)
    site = os.path.join(obj, "test", "lit.site.cfg.py")
    values = {
        "llvm_src_root": os.path.join(SRC, "llvm"),
        "llvm_obj_root": BUILD,
        "llvm_tools_dir": os.path.join(BUILD, "bin"),
        "llvm_libs_dir": os.path.join(BUILD, "lib"),
        "llvm_shlib_dir": os.path.join(BUILD, "lib"),
        "llvm_plugin_ext": ".so",
        "lit_tools_dir": "",
        "errc_messages": "",
        "clang_obj_root": obj,
        "clang_src_dir": os.path.join(SRC, "clang"),
        "clang_tools_dir": os.path.join(BUILD, "bin"),
        "clang_lib_dir": os.path.join(BUILD, "lib"),
        "host_triple": "x86_64-unknown-linux-gnu",
        "target_triple": "x86_64-unknown-linux-gnu",
        "llvm_use_sanitizer": "",
        "clang_default_cxx_stdlib": c.get("CLANG_DEFAULT_CXX_STDLIB", ""),
        "clang_vendor_uti": "org.llvm.clang",
        "host_arch": "x86_64",
        "perl_executable": "",
        "python_executable": sys.executable,
        "llvm_external_lit": "",
    }
    flags = {
        "have_zlib": on("LLVM_ENABLE_ZLIB"),
        "have_zstd": on("LLVM_ENABLE_ZSTD"),
        "clang_objc_rewriter": False,
        "clang_default_pie_on_linux": on("CLANG_DEFAULT_PIE_ON_LINUX"),
        "clang_staticanalyzer": on("CLANG_ENABLE_STATIC_ANALYZER"),
        "clang_staticanalyzer_z3": False,
        "clang_staticanalyzer_z3_mock": False,
        "clang_enable_cir": False,
        "clang_examples": False,
        "llvm_examples": False,
        "enable_shared": False,
        "enable_backtrace": True,
        "enable_threads": True,
        "build_shared_libs": False,
        "link_llvm_dylib": False,
        "link_clang_dylib": False,
        "reverse_iteration": False,
        "has_plugins": False,
        "standalone_build": False,
        "ppc_linux_default_ieeelongdouble": False,
        "have_llvm_driver": False,
        "spirv_tools_tests": False,
        "use_xcselect": False,
    }
    with open(site, "w") as f:
        f.write("import os, sys\n")
        for k, v in values.items():
            f.write(f"config.{k} = {v!r}\n")
        for k, v in flags.items():
            f.write(f"config.{k} = {v!r}\n")
        f.write("config.clang_lit_site_cfg = __file__\n")
        f.write(f"config.substitutions.append(('%llvm-version-major', {c['CMAKE_PROJECT_VERSION_MAJOR']!r}))\n")
        f.write("import lit.llvm\nlit.llvm.initialize(lit_config, config)\n")
        f.write(f"lit_config.load_config(config, {os.path.join(SRC, 'clang', 'test', 'lit.cfg.py')!r})\n")

    sys.path.insert(0, os.path.join(SRC, "llvm", "utils", "lit"))
    from lit.main import main as lit_main
    config_map = {os.path.normcase(os.path.abspath(os.path.join(SRC, "clang", "test", "lit.cfg.py"))):
                  os.path.abspath(site)}
    lit_main({"config_map": config_map, "build_mode": "."})


if __name__ == "__main__":
    main()
