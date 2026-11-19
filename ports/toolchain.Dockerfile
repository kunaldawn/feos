# The toolchain build image: localhost/feos plus what building relibc, LLVM and
# rustc for feos needs on a Linux host. Built by ports/toolchain-image.sh.
#
# The compilers here (clang, lld, llvm, gcc, g++) are generation 0's host tools
# only: they build the feos toolchain from ports/src, and nothing feos runs is
# taken from them. cbindgen is not installed here: it is built from
# ports/src/cbindgen, offline, like everything else (ports/build-cbindgen.sh).
#
# Everything a build in this image needs from the network is fetched here; the
# builds themselves run with --network none.
FROM localhost/feos

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        autoconf \
        automake \
        autopoint \
        bison \
        clang \
        cmake \
        file \
        flex \
        g++ \
        gcc \
        gcc-aarch64-linux-gnu \
        g++-aarch64-linux-gnu \
        gettext \
        git \
        libc6-dev \
        libtool \
        lld \
        llvm \
        ninja-build \
        patch \
        pkg-config \
        python3 \
        qemu-user \
        expect \
        rsync \
        texinfo \
        xz-utils \
        zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

# std for the Linux targets the host-side tests run relibc and std programs on
# (aarch64 under qemu-user). rust-src (in the base image) is what -Zbuild-std uses.
RUN rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu

# The base image starts QEMU; this one runs whatever command it is given.
ENTRYPOINT []
CMD ["/bin/sh"]
WORKDIR /src
