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
at upstream.

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

GNU make from git has no generated `configure`, so its first build needs autoconf and
automake on the host. Once one make exists anywhere, it builds the next.

## Crates

`vendor/` holds every crates.io package those builds use, from `cargo vendor` over the
lockfiles of rust's workspace, cargo, `library/` (std, which `-Zbuild-std` and the x86_64
kernel's `core` build also use), `library/stdarch`, `src/bootstrap`,
`rustc_codegen_cranelift` and gitoxide. `ports/.cargo/config.toml` points every cargo
run under `ports/` at it and turns on `net.offline`, and the feos root's
`.cargo/config.toml` does the same for the kernel's builds. feos itself has no crates.io
dependencies.

Packages that no build of ours can reach (Windows-only and wasm-only ones, about 190)
are cut to their `Cargo.toml` and an empty `src/lib.rs` by `prune-vendor.py`, marked by a
`.feos-stub` file. Cargo needs the manifest to resolve the lockfile but never compiles a
dependency gated on another platform. That halves `vendor/`, to about 860 MB.

To re-vendor after moving a pin, from `ports/src/rust`:

```sh
P=$(cd ../.. && pwd)
RUSTC_BOOTSTRAP=1 cargo vendor --locked \
  --sync $P/src/rust/src/tools/cargo/Cargo.toml --sync $P/src/rust/library/Cargo.toml \
  --sync $P/src/rust/library/stdarch/Cargo.toml --sync $P/src/rust/src/bootstrap/Cargo.toml \
  --sync $P/src/rust/compiler/rustc_codegen_cranelift/Cargo.toml \
  --sync $P/src/gitoxide/Cargo.toml $P/vendor
```

Absolute paths, because cargo compares workspace roots by path. Then build the keep list
and prune:

```sh
for m in src/rust/Cargo.toml src/rust/src/tools/cargo/Cargo.toml src/rust/library/Cargo.toml \
         src/rust/library/stdarch/Cargo.toml src/rust/src/bootstrap/Cargo.toml \
         src/rust/compiler/rustc_codegen_cranelift/Cargo.toml src/gitoxide/Cargo.toml; do
  for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    for f in "" --all-features; do
      RUSTC_BOOTSTRAP=1 cargo metadata $f --format-version 1 --locked \
        --manifest-path $P/$m --filter-platform $t
    done
  done
done | python3 -c 'import json,sys
for line in sys.stdin:
    for p in json.loads(line)["packages"]:
        if p.get("source"): print(p["name"] + "\t" + p["version"])' | sort -u > keep.txt
python3 $P/prune-vendor.py $P/vendor keep.txt
```

feos is a `unix` target, so whatever a Linux host reaches is a superset of what a feos
build of the same tools reaches, except crates gated on `target_os = "feos"`, which
don't exist yet. When one does, add it to the keep list.

## Checking that it really is offline

In the `localhost/feos` container, with `--network none` and the tree mounted at `/src`:

```sh
make build-arm build-x64
(cd ports/src/rust/src/bootstrap && cargo build --locked)
(cd ports/src/gitoxide && cargo build --locked --release \
    --no-default-features --features max-pure --bin gix)
```

`gix` is built `max-pure`: pure Rust throughout, rustls for TLS, no OpenSSL or libcurl,
which is also the configuration feos will run. Its default features need the host's
OpenSSL through pkg-config.

The image build (`Dockerfile`) leaves `ports/src/` out of its context but keeps
`ports/vendor`, which the x86_64 kernel's `-Zbuild-std` resolves std's lockfile through.
