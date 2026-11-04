# syntax=docker/dockerfile:1

FROM rust:1-slim-bookworm

ENV DEBIAN_FRONTEND=noninteractive

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
        qemu-system-arm \
        qemu-system-x86 \
        qemu-system-gui \
    && rm -rf /var/lib/apt/lists/*

# Bare-metal targets (Rust ships precompiled `core` for both).
RUN rustup target add aarch64-unknown-none x86_64-unknown-none

WORKDIR /src
COPY . .

RUN chmod +x scripts/*.sh \
    && cargo build --release --target aarch64-unknown-none \
    && cargo build --release --target x86_64-unknown-none

# `docker run --rm -it feos` builds (cached) and boots the kernel.
# FEOS_ARCH selects arm (default) or x64. FEOS_DISPLAY picks the QEMU backend:
# gtk (window), vnc, or none. When unset the container serves VNC on port 5900
# (serial stays on stdio).
EXPOSE 5900
ENTRYPOINT ["/src/scripts/docker-run.sh"]
