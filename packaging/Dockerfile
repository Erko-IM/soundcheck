# The Linux container a Mac builds the Windows installer and the Linux
# packages in, which `make exe` and `make linux` run there. Ubuntu 22.04 like
# the Packages workflow's runner, so the Linux packages run on the same
# distros.
FROM ubuntu:22.04
ARG TARGETARCH
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl file pkg-config libasound2-dev gcc-mingw-w64-x86-64 nsis \
    && rm -rf /var/lib/apt/lists/*

# On an Apple silicon Mac the container is arm64, and the Linux packages are
# for x86_64 PCs, so it cross-builds them, with an x86_64 linker and ALSA.
RUN if [ "$TARGETARCH" = arm64 ]; then \
        dpkg --add-architecture amd64 \
        && sed -i 's/^deb /deb [arch=arm64] /' /etc/apt/sources.list \
        && printf 'deb [arch=amd64] http://%s/ubuntu %s main universe\n' \
            archive.ubuntu.com jammy archive.ubuntu.com jammy-updates security.ubuntu.com jammy-security \
            >/etc/apt/sources.list.d/amd64.list \
        && apt-get update \
        && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
            gcc-x86-64-linux-gnu libc6-dev-amd64-cross libasound2-dev:amd64 \
        && rm -rf /var/lib/apt/lists/*; \
    fi
ENV LINUX_TARGET=x86_64-unknown-linux-gnu \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_LIBDIR_x86_64_unknown_linux_gnu=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig
