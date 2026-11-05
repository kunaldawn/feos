# syntax=docker/dockerfile:1

# Keep in step with rust-toolchain.toml and `rust-version` in Cargo.toml.
FROM rust:1.99.1-slim-bookworm

ENV DEBIAN_FRONTEND=noninteractive

# ovmf and qemu-efi-aarch64 are the UEFI firmware (OVMF and AAVMF) for the EFI boot
# test, and mtools fills its ESP image without mounting anything.
#
# qemu-system-arm ships qemu-system-aarch64 (used for the `virt` machine);
# qemu-system-x86 ships qemu-system-x86_64 (used for q35). binutils provides
# objcopy for the x86_64 flat multiboot image.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        binutils \
        ca-certificates \
        dosfstools \
        exfatprogs \
        make \
        mtools \
        ovmf \
        python3-minimal \
        qemu-efi-aarch64 \
        qemu-system-arm \
        qemu-system-x86 \
        qemu-system-gui \
    && rm -rf /var/lib/apt/lists/*

# The AArch64 target ships a precompiled `core`. The x86_64 kernel builds its own
# from rust-src, for hard floats: see scripts/cargo-x64.sh.
RUN rustup target add aarch64-unknown-none \
    && rustup component add rust-src

WORKDIR /src
COPY . .

# Optional limits for this build alone, for a machine short of memory:
#   docker build --build-arg CARGO_BUILD_JOBS=2 --build-arg CARGO_PROFILE_RELEASE_LTO=off ...
# Neither stays in the image.
ARG CARGO_BUILD_JOBS
ARG CARGO_PROFILE_RELEASE_LTO

# The test disks are made here so that `make docker-run` starts from a cached
# build instead of relinking both kernels on every start.
RUN chmod +x scripts/*.sh scripts/*.py \
    && ./scripts/make-test-disks.sh \
    && make user-arm user-x64 \
    && cargo build --release --target aarch64-unknown-none \
    && ./scripts/cargo-x64.sh build --release

# `docker run --rm -it feos` builds (cached) and boots the kernel.
# FEOS_ARCH selects arm (default) or x64. FEOS_DISPLAY picks the QEMU backend:
# gtk (window), vnc, or none. When unset the container serves VNC on port 5900
# (serial stays on stdio).
EXPOSE 5900
ENTRYPOINT ["/src/scripts/docker-run.sh"]
