# syntax=docker/dockerfile:1

FROM rust:1-slim-bookworm

ENV DEBIAN_FRONTEND=noninteractive

# qemu-system-arm ships qemu-system-aarch64 (used for the `virt` machine).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        make \
        qemu-system-arm \
        qemu-system-gui \
    && rm -rf /var/lib/apt/lists/*

# Bare-metal AArch64 target (Rust ships a precompiled `core` for it).
RUN rustup target add aarch64-unknown-none

WORKDIR /src
COPY . .

RUN chmod +x scripts/*.sh \
    && cargo build --release

# `docker run --rm -it feos` builds (cached) and boots the kernel.
# FEOS_DISPLAY picks the QEMU backend: gtk (window), vnc, or none. When unset
# the container serves VNC on port 5900 (serial stays on stdio).
EXPOSE 5900
ENTRYPOINT ["/src/scripts/docker-run.sh"]
