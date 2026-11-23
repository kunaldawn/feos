#!/usr/bin/env -S python3 -I
"""The libc crate's feos module against relibc's feos headers (track T3), libc-test
style (what the libc crate's own `ctest` does, without its crates.io dependencies).

    libc_check.py ARCH JSON PROBE_OBJ SYSROOT OUTDIR      # driven by libc-check.sh

Three sides:

- `rustdoc --output-format json` of ports/src/libc for <arch>-unknown-feos (JSON) is the
  list of what the crate's root exports for feos, cfgs resolved: type aliases, structs
  and unions with their public fields, integer constants, C functions and statics.
- The probe (PROBE_OBJ, compiled from `probe.rs`, which `gen-probe` below writes) holds
  rustc's own numbers for each: sizes, alignments, signedness, field offsets and sizes,
  constant values, in one `u64` array read out of the object file.
- The C side: `check.c` asserts each number against relibc's headers with
  `_Static_assert` (sizeof, _Alignof, offsetof, the constants' values), and
  `check.cc` each function's signature (every parameter and the return value as size
  and class: signed or unsigned integer, float, pointer, struct; and variadic or not)
  through C++ templates over the declared function's type. Both compile for
  <arch>-unknown-none-elf against SYSROOT's headers with -fsyntax-only; every error
  line maps back to its item. Each C function must also be defined in SYSROOT's
  libc.a or libm.a.

`allow.txt` next to this script lists the items that may differ, each with its reason;
anything else that differs fails the check.
"""
import json
import os
import re
import struct
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# Headers the checks include: relibc's public ones, without openlibm's internals,
# relibc's per-architecture and Redox-only files and the bits/ fragments.
SKIP_HEADER = re.compile(r"^(bits/|arch/|machine/|redox/|sys/redox\.h|openlibm|math_private|k_log|"
                         r".*_fpmath\.h|fpmath\.h|bsd_cdefs\.h|cdefs-compat\.h|types-compat\.h|"
                         r"dl-tls\.h|stdatomic\.h|stdnoreturn\.h|complex\.h|tgmath\.h|sys/queue\.h)")

# The C type of each class code.
C_OF = {"i1": "signed char", "u1": "unsigned char", "i2": "short", "u2": "unsigned short", "i4": "int",
        "u4": "unsigned int", "i8": "long long", "u8": "unsigned long long", "b1": "_Bool"}

PRIMS = {"i8": "i1", "u8": "u1", "i16": "i2", "u16": "u2", "i32": "i4", "u32": "u4",
         "i64": "i8", "u64": "u8", "isize": "i8", "usize": "u8", "f32": "f4", "f64": "f8",
         "bool": "b1"}


CORE_FFI = {"c_void": "v", "c_schar": "i1", "c_uchar": "u1", "c_short": "i2", "c_ushort": "u2",
            "c_int": "i4", "c_uint": "u4", "c_long": "i8", "c_ulong": "u8", "c_longlong": "i8",
            "c_ulonglong": "u8", "c_float": "f4", "c_double": "f8", "c_size_t": "u8",
            "c_ssize_t": "i8", "c_ptrdiff_t": "i8"}


class Crate:
    def __init__(self, path, arch):
        d = json.load(open(path))
        self.idx = d["index"]
        self.paths = d["paths"]
        self.arch = arch
        self.exports = {}            # name -> item id
        self._walk(str(d["root"]), set())

    def _walk(self, mid, seen):
        if mid in seen:
            return
        seen.add(mid)
        for i in self.idx[mid]["inner"]["module"]["items"]:
            item = self.idx.get(str(i))
            if item is None:
                continue
            kind, inner = next(iter(item["inner"].items()))
            if kind == "use":
                if item["visibility"] != "public" or inner.get("id") is None:
                    continue
                target = str(inner["id"])
                if inner["is_glob"]:
                    if target in self.idx and "module" in self.idx[target]["inner"]:
                        self._walk(target, seen)
                elif target in self.idx:
                    self.exports.setdefault(inner["name"], target)
            elif item["visibility"] == "public" and kind != "module" and item["name"]:
                self.exports.setdefault(item["name"], str(i))

    def item(self, name):
        return self.idx[self.exports[name]]

    def kind(self, name):
        return next(iter(self.item(name)["inner"]))

    def cls(self, t):
        """A Rust type's class code ('i4', 'u8', 'p', 'v', 's:NAME', 'f8'...), or None."""
        if t is None:
            return "v"
        k, v = next(iter(t.items()))
        if k == "primitive":
            return "v" if v == "never" else PRIMS.get(v)
        if k in ("raw_pointer", "function_pointer", "borrowed_ref"):
            return "p"
        if k == "tuple" and not v:
            return "v"
        if k == "resolved_path":
            name = v["path"].split("::")[-1]
            if name == "Option":
                return "p"
            item = self.idx.get(str(v["id"]))
            if item is None:
                # core::ffi's, by architecture.
                ext = self.paths.get(str(v["id"]), {}).get("path", [name])[-1]
                return CORE_FFI.get(ext) or ("u1" if self.arch == "aarch64" else "i1") \
                    if ext == "c_char" else CORE_FFI.get(ext)
            ik, inner = next(iter(item["inner"].items()))
            if ik == "type_alias":
                return self.cls(inner["type"])
            if ik in ("struct", "union"):
                return "s:" + item["name"]
            if ik == "enum":
                return "e:" + item["name"]
        return None


def integer_alias(crate, name):
    c = crate.cls(crate.item(name)["inner"]["type_alias"]["type"])
    return c is not None and c[0] in "iu"


def c_type(name):
    """How C names the Rust type NAME."""
    if name in ("sigval",):
        return "union " + name
    if name.endswith("_t") or name.startswith("Elf64_") or name in ("Dl_info", "fd_set", "FILE", "DIR"):
        return name
    return "struct " + name


def collect(crate, allow):
    """The checks, as (key, rust expression, C expression or None)."""
    checks = []
    for name in sorted(crate.exports):
        kind = crate.kind(name)
        item = crate.item(name)
        if kind == "type_alias":
            if integer_alias(crate, name) or crate.cls(item["inner"]["type_alias"]["type"]) == "p":
                checks.append((f"size {name}", f"size_of::<libc::{name}>()", f"sizeof({name})"))
                if integer_alias(crate, name):
                    checks.append((f"signed {name}", f"((-1i64 as libc::{name}) < (0 as libc::{name})) as usize",
                                   f"(({name})-1 < ({name})0)"))
        elif kind in ("struct", "union"):
            ct = c_type(name)
            checks.append((f"size {name}", f"size_of::<libc::{name}>()", f"sizeof({ct})"))
            checks.append((f"align {name}", f"align_of::<libc::{name}>()", f"_Alignof({ct})"))
            inner = item["inner"][kind]
            fields = inner["kind"]["plain"]["fields"] if kind == "struct" and "plain" in inner["kind"] \
                else inner.get("fields", [])
            for f in fields:
                fi = crate.idx[str(f)]
                if fi["visibility"] != "public":
                    continue
                fn = fi["name"]
                checks.append((f"offset {name}.{fn}", f"offset_of!(libc::{name}, {fn})",
                               f"__builtin_offsetof({ct}, {fn})"))
                checks.append((f"fsize {name}.{fn}",
                               f"fsize(|p: *const libc::{name}| unsafe {{ core::ptr::addr_of!((*p).{fn}) }})",
                               f"sizeof((({ct} *)0)->{fn})"))
        elif kind == "constant":
            c = crate.cls(item["inner"]["constant"]["type"])
            if c is None or c not in C_OF:
                continue
            ct = C_OF[c]
            # The C value at the Rust constant's type, and that the type holds it.
            checks.append((f"const {name}", f"libc::{name} as u64 as usize",
                           f"(unsigned long long)({ct})({name}) + "
                           f"((({name}) == ({ct})({name})) ? 0 : 0x5a5a5a5a5a5a5a5aULL)"))
    return checks


def gen_probe(crate, checks, out):
    lines = ["#![no_std]", "#![allow(deprecated, unused_unsafe)]",
             "extern crate libc;", "use core::mem::{align_of, offset_of, size_of};",
             "const fn fsize<T, F>(_f: fn(*const T) -> *const F) -> usize { size_of::<F>() }",
             f"#[unsafe(no_mangle)]\n#[used]\npub static FEOS_LIBC_CHECK: [u64; {len(checks)}] = ["]
    for key, rust, _ in checks:
        lines.append(f"    ({rust}) as u64, // {key}")
    lines.append("];")
    open(out, "w").write("\n".join(lines) + "\n")


def read_probe(obj, n):
    data = open(obj, "rb").read()
    shoff, = struct.unpack_from("<Q", data, 0x28)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3a)
    sections = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + i * shentsize) for i in range(shnum)]
    for sh in sections:
        if sh[1] == 2:                       # SHT_SYMTAB
            strtab = sections[sh[6]]
            for off in range(sh[4], sh[4] + sh[5], 24):
                st_name, _info, _other, shndx, value, size = struct.unpack_from("<IBBHQQ", data, off)
                end = data.index(b"\0", strtab[4] + st_name)
                if data[strtab[4] + st_name:end] == b"FEOS_LIBC_CHECK":
                    sec = sections[shndx]
                    assert size == 8 * n, (size, n)
                    # A relocatable object: the value is the offset in its section.
                    return list(struct.unpack_from(f"<{n}Q", data, sec[4] + value))
    sys.exit("libc-check: FEOS_LIBC_CHECK not in the probe object")


def headers(sysroot):
    inc = os.path.join(sysroot, "include")
    out = []
    for root, _dirs, files in os.walk(inc):
        for f in files:
            rel = os.path.relpath(os.path.join(root, f), inc)
            if rel.endswith(".h") and not SKIP_HEADER.match(rel):
                out.append(rel)
    return sorted(out)


def cc(arch, sysroot, src, lang):
    driver = os.path.join(HERE, "..", "..", "toolchain", "bin", f"{arch}-unknown-feos-cc")
    env = dict(os.environ, FEOS_SYSROOT=sysroot)
    extra = ["-x", "c++", "-std=c++17", "-nostdinc++", "-Drestrict=__restrict"] if lang == "c++" \
        else ["-std=gnu11"]
    r = subprocess.run([driver, "-c", "-fsyntax-only", "-ferror-limit=0", "-w", "-Wno-everything",
                        *extra, src], capture_output=True, text=True, env=env)
    return r.stderr


def error_lines(stderr, src, first=0):
    """{line: first message} of the errors in src. An error inside a template above
    `first` (the C++ prelude) counts for the line below it whose instantiation led
    there."""
    out = {}
    pat = re.compile(re.escape(src) + r":(\d+):\d+: (error|note): (.*)")
    pending = None
    for line in stderr.splitlines():
        m = pat.match(line)
        if not m:
            continue
        n, kind, msg = int(m.group(1)), m.group(2), m.group(3)
        if kind == "error":
            pending = None
            if n >= first:
                out.setdefault(n, msg)
            else:
                pending = msg
        elif pending is not None and n >= first:
            out.setdefault(n, pending)
            pending = None
    return out


def check_layout(arch, sysroot, outdir, checks, values, include):
    src = os.path.join(outdir, "check.c")
    lines = ["#define _GNU_SOURCE 1", "#define _DEFAULT_SOURCE 1"] + [f"#include <{h}>" for h in include]
    first = len(lines) + 1
    for (key, _, cexpr), value in zip(checks, values):
        lines.append(f"_Static_assert(({cexpr}) == {value}ULL, \"{key}\");")
    open(src, "w").write("\n".join(lines) + "\n")
    errors = error_lines(cc(arch, sysroot, src, "c"), src)
    header_errors = {k: v for k, v in errors.items() if k < first}
    if header_errors:
        sys.exit(f"libc-check: relibc's headers don't compile together: {header_errors}")
    failures = {}
    by_key = {k: v for (k, _, _), v in zip(checks, values)}
    for line, msg in errors.items():
        key, _, cexpr = checks[line - first]
        value = values[line - first]
        if "incomplete type" in msg and by_key.get("size " + key.split()[1]) == 0:
            continue                 # opaque on both sides (FILE, DIR, ...)
        if "static assertion failed" in msg or "static_assert failed" in msg:
            failures[key] = f"Rust {value}, C differs"
        else:
            failures[key] = f"C: {msg}"
    # The C values of the differing ones, for the report.
    if failures:
        probe = ["#define _GNU_SOURCE 1", "#define _DEFAULT_SOURCE 1"] + [f"#include <{h}>" for h in include]
        probe.append("struct feos_show { int x; };")
        pfirst = len(probe) + 1
        keys = []
        for (key, _, cexpr) in checks:
            if key in failures and not failures[key].startswith("C:"):
                # The error message names the type, so the value: char (*)[VALUE].
                probe.append(f"struct feos_show feos_show_{len(keys)} = (char (*)[({cexpr})])0;")
                keys.append(key)
        psrc = os.path.join(outdir, "values.c")
        open(psrc, "w").write("\n".join(probe) + "\n")
        for line in cc(arch, sysroot, psrc, "c").splitlines():
            m = re.search(r"values\.c:(\d+):.*'char \(\*\)\[(\d+)\]'", line)
            if m and 0 <= int(m.group(1)) - pfirst < len(keys):
                key = keys[int(m.group(1)) - pfirst]
                failures[key] = failures[key].replace("C differs", f"C {m.group(2)}")
    return failures


SIG_PRELUDE = r"""
template <class T> struct Code { static constexpr int v = __is_integral(T) ? (__is_signed(T) ? 100 : 200) + (int)sizeof(T)
    : __is_floating_point(T) ? 300 + (int)sizeof(T) : (__is_class(T) || __is_union(T)) ? 500 + (int)sizeof(T)
    : __is_enum(T) ? 100 + (int)sizeof(T) : -1; };
template <> struct Code<void> { static constexpr int v = 0; };
template <> struct Code<bool> { static constexpr int v = 601; };
template <class T> struct Code<T *> { static constexpr int v = 400; };
template <class T> struct Code<T *const> { static constexpr int v = 400; };
template <class T> struct Code<T &> { static constexpr int v = 400; };
template <class T> struct Code<const T> { static constexpr int v = Code<T>::v; };
template <class T> struct Code<volatile T> { static constexpr int v = Code<T>::v; };
template <class R, class... A> constexpr int sig_at(R (*)(A...), int i) {
    const int got[] = {0, Code<R>::v, Code<A>::v..., -2};
    return i < (int)(sizeof(got) / sizeof(got[0])) ? got[i] : -3;
}
template <class R, class... A> constexpr int sig_at(R (*)(A..., ...), int i) {
    const int got[] = {1, Code<R>::v, Code<A>::v..., -2};
    return i < (int)(sizeof(got) / sizeof(got[0])) ? got[i] : -3;
}
template <class F, int N> constexpr bool sig_is(F f, const int (&want)[N]) {
    for (int i = 0; i < N; i++)
        if (sig_at(f, i) != want[i]) return false;
    return true;
}
"""


def rust_code(crate, c, sizes):
    """The C++ side's code for a Rust class code."""
    if c == "v":
        return 0
    if c == "p":
        return 400
    if c == "b1":
        return 601
    if c[0] in "iu":
        return (100 if c[0] == "i" else 200) + int(c[1:])
    if c[0] == "f":
        return 300 + int(c[1:])
    if c.startswith("s:"):
        return 500 + sizes[f"size {c[2:]}"]
    if c.startswith("e:"):
        return 104
    raise ValueError(c)


def check_functions(arch, sysroot, outdir, crate, sizes, include):
    fns = []
    for name in sorted(crate.exports):
        item = crate.item(name)
        if crate.kind(name) != "function":
            continue
        f = item["inner"]["function"]
        if f["has_body"] or "C" not in f["header"]["abi"]:
            continue
        sig = f["sig"]
        codes = [crate.cls(t) for _, t in sig["inputs"]]
        ret = crate.cls(sig["output"])
        if None in codes or ret is None:
            fns.append((name, None))
            continue
        link = name
        for attr in item["attrs"]:
            a = attr.get("other", "") if isinstance(attr, dict) else str(attr)
            m = re.search(r'link_name\s*=\s*"([^"]+)"', a) or re.search(r'LinkName \{ name: "([^"]+)"', a)
            if m:
                link = m.group(1)
        want = [1 if sig["is_c_variadic"] else 0, rust_code(crate, ret, sizes)] + \
            [rust_code(crate, c, sizes) for c in codes] + [-2]
        fns.append((name, (link, want)))
    src = os.path.join(outdir, "check.cc")
    lines = ["#define _GNU_SOURCE 1", "#define _DEFAULT_SOURCE 1"] + [f"#include <{h}>" for h in include]
    lines += SIG_PRELUDE.strip().splitlines()
    first = len(lines) + 1
    keyed = [(n, s) for n, s in fns if s is not None]
    for n, (_link, want) in keyed:
        lines.append(f"static constexpr int want_{n}[] = {{{', '.join(map(str, want))}}}; "
                     f"static_assert(sig_is(&::{n}, want_{n}), \"fn {n}\");")
    open(src, "w").write("\n".join(lines) + "\n")
    stderr = cc(arch, sysroot, src, "c++")
    open(os.path.join(outdir, "check.cc.log"), "w").write(stderr)
    errors = error_lines(stderr, src, first)
    header_errors = {k: v for k, v in errors.items() if k < first}
    if header_errors:
        sys.exit(f"libc-check: relibc's headers don't compile as C++: {list(header_errors.items())[:10]}")
    failures = {f"fn {n}": "a Rust type the check can't classify" for n, s in fns if s is None}
    for line, msg in errors.items():
        n = keyed[line - first][0]
        if "static_assert" in msg or "static assertion" in msg:
            failures[f"fn {n}"] = "signature differs (Rust " + ",".join(map(str, keyed[line - first][1][1])) + ")"
        else:
            failures[f"fn {n}"] = f"C: {msg}"
    # Defined in the libraries?
    defined = set()
    for lib in ("libc.a", "libm.a"):
        out = subprocess.run(["llvm-nm", "--defined-only", "-j", os.path.join(sysroot, "lib", lib)],
                             capture_output=True, text=True).stdout
        defined.update(out.split())
    for n, s in keyed:
        if s[0] not in defined:
            failures.setdefault(f"fn {n}", f"{s[0]} not defined in libc.a or libm.a")
    return len(keyed), failures


def check_statics(crate, sysroot):
    defined = set()
    for lib in ("libc.a", "libm.a"):
        out = subprocess.run(["llvm-nm", "--defined-only", "-j", os.path.join(sysroot, "lib", lib)],
                             capture_output=True, text=True).stdout
        defined.update(out.split())
    failures, n = {}, 0
    for name in sorted(crate.exports):
        if crate.kind(name) == "static":
            n += 1
            if name not in defined:
                failures[f"static {name}"] = "not defined in libc.a"
    return n, failures


def load_allow():
    allow = {}
    for line in open(os.path.join(HERE, "allow.txt")):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        key, _, why = line.partition(":")
        allow[key.strip()] = why.strip()
    return allow


def main():
    if sys.argv[1] == "gen-probe":
        _, _, arch, js, out = sys.argv
        crate = Crate(js, arch)
        checks = collect(crate, {})
        gen_probe(crate, checks, out)
        print(f"libc-check: {arch}: {len(checks)} probes in {out}")
        return
    arch, js, obj, sysroot, outdir = sys.argv[1:]
    crate = Crate(js, arch)
    allow = load_allow()
    checks = collect(crate, allow)
    values = read_probe(obj, len(checks))
    sizes = {k: v for (k, _, _), v in zip(checks, values) if k.startswith("size ")}
    include = headers(sysroot)
    failures = check_layout(arch, sysroot, outdir, checks, values, include)
    nfn, ffail = check_functions(arch, sysroot, outdir, crate, sizes, include)
    nst, sfail = check_statics(crate, sysroot)
    failures.update(ffail)
    failures.update(sfail)
    kinds = {}
    for key, _, _ in checks:
        kinds[key.split()[0]] = kinds.get(key.split()[0], 0) + 1
    kinds["fn"] = nfn
    kinds["static"] = nst
    bad = {k: v for k, v in failures.items() if k not in allow and k.split(".")[0] not in allow}
    allowed = {k: v for k, v in failures.items() if k not in bad}
    total = sum(kinds.values())
    print(f"libc-check: {arch}-unknown-feos: {total} checks ("
          + ", ".join(f"{v} {k}" for k, v in sorted(kinds.items())) + f") against {len(include)} headers")
    for k, v in sorted(allowed.items()):
        print(f"  allowed  {k}: {v}  [{allow.get(k) or allow.get(k.split('.')[0])}]")
    for k, v in sorted(bad.items()):
        print(f"  FAIL     {k}: {v}")
    stale = [k for k in allow if k not in failures and not any(f.split(".")[0] == k for f in failures)]
    for k in stale:
        print(f"  (allow.txt's {k} passes now; take it out)")
    print(f"libc-check: {arch}: {total - len(failures)} passed, {len(allowed)} allowed, {len(bad)} failed")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
