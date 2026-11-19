# ports

Every third-party source the feos toolchain is built from, so that one clone builds
everything with no network: on Linux for generation 0, and on feos itself after that
(`docs/selfhost-plan.md`).

```sh
git clone https://github.com/kunaldawn/feos && cd feos
scripts/ports-init.sh        # the only step that uses the network
```

`scripts/ports-init.sh` is `git submodule update --init --recursive` without rust's own
submodules that no feos build uses (its books, `src/gcc`, `src/tools/enzyme` and
`src/tools/rustc-perf`, several GB between them), and every fetch is shallow at the pinned
commit.

## Sources

Each is a git submodule pinned to one commit. The ones feos patches move to forks under
github.com/kunaldawn as the patches land (`git submodule set-url`); until then they point
at upstream, and feos's changes to them are kept as `patches/<name>.patch`, which
`scripts/ports-init.sh` applies (`patches.sh`; run `ports/patches.sh save` before
committing, see `PATCHES.md`).

| Path | Upstream | Pinned at | Why that pin |
|---|---|---|---|
| `src/rust` | rust-lang/rust | `1.99.1` (`58cf8f9c`) | the host toolchain in `rust-toolchain.toml` |
| `src/rust/src/llvm-project` | rust-lang/llvm-project | `1b9c0d5f` | rust 1.99.1's own submodule pin |
| `src/rust/src/tools/cargo` | rust-lang/cargo | `5f94df47` | rust 1.99.1's pin |
| `src/rust/library/backtrace` | rust-lang/backtrace-rs | `d902726a` | rust 1.99.1's pin |
| `src/relibc` | redox-os/relibc (GitLab) | `master` at `50c054e2` | relibc has no releases; the one `docs/feos-abi-v2.md` maps |
| `src/relibc/openlibm`, `src/relibc/dlmalloc-rs` | redox-os | relibc's pins | |
| `src/libc` | rust-lang/libc | `0.2.189` (`ef0906e2`) | what rust 1.99.1's `library/Cargo.lock` uses |
| `src/gitoxide` | GitoxideLabs/gitoxide | `v0.61.0` (`dd39fb66`) | latest release |
| `src/cpython` | python/cpython | `v3.13.18` (`0d4bd512`) | latest 3.13 release |
| `src/cmake` | Kitware/CMake | `v4.5.0` (`f3a3cb0c`) | latest release |
| `src/ninja` | ninja-build/ninja | `v1.13.2` (`3441b633`) | latest release |
| `src/make` | GNU make (Savannah) | `4.4.1` (`d66a65ad`) | latest release |
| `src/cbindgen` | mozilla/cbindgen | `v0.29.4` (`b826cb89`) | latest release; relibc generates its headers with it |

GNU make from git has no generated `configure`, so its first build needs autoconf and
automake on the host. Once one make exists anywhere, it builds the next.

## Crates

`vendor/` holds every crates.io package those builds use, from `cargo vendor` over the
lockfiles of rust's workspace, cargo, `library/` (std, which `-Zbuild-std` and the x86_64
kernel's `core` build also use), `library/stdarch`, `src/bootstrap`,
`rustc_codegen_cranelift`, gitoxide, cbindgen and relibc. `vendor-git/` holds the git
dependencies relibc takes from gitlab.redox-os.org (an `object` fork, `redox_syscall`
from Redox's kernel repository, `redox-rings`, `redox-buffer-pool`).
`ports/.cargo/config.toml` points every cargo run under `ports/` at both, with a source
replacement for crates.io and one for each git source, and turns on `net.offline`; the
feos root's `.cargo/config.toml` does the same for the kernel's builds. feos itself has
no crates.io dependencies.

The git packages have a directory of their own because relibc's `object` fork is
`object 0.36.7`, the name and version of a crates.io package rust's workspace uses, and
`cargo vendor` won't put two sources' packages of one name and version in one
directory (nor would a directory source tell them apart). So relibc isn't one more
`--sync` of the main run: `vendor.sh` vendors it alone and `vendor-merge.py` sorts the
result, its crates.io packages into `vendor/` (as `name-version`, only those not there
already) and its git packages into `vendor-git/`.

Packages that no build of ours can reach (Windows-only, wasm-only and Redox-only ones, about 190)
are cut to their `Cargo.toml` and an empty `src/lib.rs` by `prune-vendor.py`, marked by a
`.feos-stub` file. Cargo needs the manifest to resolve the lockfile but never compiles a
dependency gated on another platform. That halves `vendor/`, to about 930 MB.

To re-vendor after moving a pin or adding a workspace, run `vendor.sh` (it needs the
network) in the toolchain image (below):

```sh
docker run --rm --name feos-ports-vendor -v "$PWD":/src -v feos-ports-target:/build \
    localhost/feos-toolchain ports/vendor.sh
```

It runs, with absolute paths (cargo compares workspace roots by path):

1. `cargo vendor --locked` from `src/rust`, with a `--sync` for each other main
   workspace (cargo, `library`, `stdarch`, `bootstrap`, cranelift, gitoxide, cbindgen),
   into `vendor/`;
2. `cargo vendor --locked --versioned-dirs` of relibc into a scratch directory, then
   `vendor-merge.py` (above); it prints the git source replacements cargo wants, to
   compare with `ports/.cargo/config.toml`;
3. the keep list: `cargo metadata` (with and without `--all-features`, filtered to
   `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`) of every workspace above,
   the crates.io packages it names; then `prune-vendor.py vendor/ keep.txt`.

A new workspace goes in `MAIN` in `vendor.sh`, or in `APART` if it has git dependencies
that clash like relibc's. Run against the tree as it was, `vendor.sh` changes no file
that was already vendored: the run that added cbindgen and relibc only added
directories.

feos is a `unix` target, so whatever a Linux host reaches is a superset of what a feos
build of the same tools reaches, except crates gated on `target_os = "feos"`, which
don't exist yet. When one does, add it to the keep list.

## Checking that it really is offline

After re-vendoring, `git ls-files --others --ignored --exclude-standard ports/` must
print nothing: an ignore rule that matches a vendored file (the root `target/` once
matched `cc`'s `src/target/`) leaves it out of the commit, and only a fresh clone
notices.

Then, from a fresh clone (`git clone`, `scripts/ports-init.sh`), in the `localhost/feos`
container with `--network none` and the tree mounted at `/src`:

```sh
make build-arm build-x64
(cd ports/src/rust/src/bootstrap && cargo build --locked)
ports/build-cbindgen.sh && ports/build-relibc.sh x86_64-unknown-linux-gnu
(cd ports/src/gitoxide && cargo build --locked --release \
    --no-default-features --features max-pure --bin gix)
```

`gix` is built `max-pure`: pure Rust throughout, rustls for TLS, no OpenSSL or libcurl,
which is also the configuration feos will run. Its default features need the host's
OpenSSL through pkg-config.

The image build (`Dockerfile`) leaves `ports/src/` out of its context but keeps
`ports/vendor`, which the x86_64 kernel's `-Zbuild-std` resolves std's lockfile through.

## The toolchain image and builds

`localhost/feos-toolchain` (`toolchain.Dockerfile`, built by `toolchain-image.sh` on top
of `localhost/feos`) adds what building relibc, LLVM and rustc needs: clang, lld, llvm,
gcc/g++ and the AArch64 cross gcc, cmake, ninja, autotools, pkg-config, full python3,
qemu-user and expect (for tests), git, and std for both Linux triples. Those are
generation 0's host tools only. cbindgen is built from `src/cbindgen`, not installed.

Every build and test runs in it with `--network none`, the tree at `/src` and the
`feos-ports-target` volume at `/build` for output, through `test/container.sh`:

| Script | Builds | Output under `/build` |
|---|---|---|
| `build-cbindgen.sh` | cbindgen from `src/cbindgen` | `bin/cbindgen` |
| `build-relibc.sh TARGET` | relibc's `make all install` for `TARGET` (its Linux platform for now) | `relibc/TARGET/{include,lib}`; cargo's in `relibc-target/` |
| `test/relibc-linux.sh TARGET` | C programs against that sysroot, run; relibc's own test suite | `relibc-test/`, `relibc-suite/` |
| `build-relibc-feos.sh x86_64\|aarch64\|all` | relibc's feos platform, static, for `targets/<arch>-unknown-feos.json` | `relibc/<arch>-unknown-feos/{include,lib}`; cargo's in `relibc-target/` |
| `test/stage-t0.sh` | a base snapshot's kernels with injected programs, booted | `ports-base-target/` |
| `test/build-t1.sh` | stage T1's C programs (`test/c/t1`) with `toolchain/bin/<arch>-unknown-feos-cc` | `t1/<arch>/` |
| `test/stage-t1.sh [REF]` | cbindgen and relibc for feos (no-ops when built and unchanged, so a fresh volume works), the T1 programs injected into a base (default `refs/feos-checkpoints/1c.6`), both ports booted | `ports-base-target/` |
| `test/abi-check.sh [REF]` | host test: relibc's feos constants against a base's `feos-abi` | `abi-check-target/` |
| `test/test-fresh.sh [A B]` | the base at two commits in turn, checking each kernel is its own commit's | `ports-base-target/` |

`docs/toolchain.md` has how each piece is built, the test harness, what's tested and
what waits on which kernel stage. `PATCHES.md` lists every file patched under `src/`,
and `KERNEL-ISSUES.md` the kernel problems found on the way.

## Targets and the C driver

`targets/x86_64-unknown-feos.json` and `targets/aarch64-unknown-feos.json` are the Rust
targets until rustc has them built in (stage T4); relibc builds against them with
`-Zjson-target-spec`. `toolchain/bin/feos-cc`, called as `x86_64-unknown-feos-cc` or
`aarch64-unknown-feos-cc`, is generation 0's C compiler: the host clang and ld.lld with
the feos sysroot, the feos link layout and relibc's crt objects. `docs/toolchain.md`
says what each sets and why.

## Testing on feos

`test/` is the harness for running what the toolchain builds on feos itself. A test
kernel is never built from the working tree (another workflow may be in the middle of
editing it), but from a snapshot of a commit:

```sh
ports/test/container.sh t0 ports/test/stage-t0.sh [REF]   # e.g. refs/feos-checkpoints/1c.6
timeout 570 docker wait feos-ports-t0                      # repeat until it prints the status
ports/test/container.sh --log t0
```

`base.sh REF` makes `target/ports-base` from `git archive`; `inject.sh ARCH SRC DEST`
puts a file into its image (`/bin/NAME` per architecture, through `user/out/<arch>`;
other paths through a per-architecture overlay on `boards/qemu-virt/fs/`);
`build-kernels.sh` builds both kernels (at `/base`, outside the tree, so the tree's
`.cargo/config.toml` doesn't add its rustflags to the base's; `fresh.sh` touches a base
of another commit than the last build, since `git archive`'s mtimes would make cargo
reuse that build, and `kernels/{arm,x64}.ref` say which commit each kernel is from); `boot.py PORT KERNEL
SPEC.toml...` boots one with the base's own `scripts/boot-test.py` machinery and runs the
spec's shell commands, checking output and exit status. Cases that wait on a kernel stage
are marked `pending` and reported as such, never run as passes. See `docs/toolchain.md`.
