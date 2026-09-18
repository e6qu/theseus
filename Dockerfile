# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
#
# A Theseus runtime is an inseparable set: the service binary, the guest
# kernel, and (on arm64) the module built for that exact kernel.  Tutorials
# use this image rather than a checkout of this repository.

FROM rust:1.97.0-bookworm@sha256:8fa55b2f3ddf97471ab6a767bfa3f37e6bad0986ba823e75fea57e2a2a5c3073 AS build

ARG SOURCE_DATE_EPOCH
ARG TARGETARCH
ARG THESEUS_SOURCE_COMMIT=unknown
ARG THESEUS_KERNEL_REVISION=8a40ca92bfa9b706b76287942c89b13884928cb0
ENV SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH \
    THESEUS_SOURCE_COMMIT=$THESEUS_SOURCE_COMMIT \
    THESEUS_KERNEL_REVISION=$THESEUS_KERNEL_REVISION \
    ZERO_AR_DATE=1

RUN printf '%s\n' 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/99snapshot \
    && sed -i \
        -e 's|http://deb.debian.org/debian-security|http://snapshot.debian.org/archive/debian-security/20260713T000000Z|' \
        -e 's|http://deb.debian.org/debian|http://snapshot.debian.org/archive/debian/20260713T000000Z|' \
        /etc/apt/sources.list.d/debian.sources \
    && apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        bc binutils bison busybox-static clang cpio curl dwarves file flex gcc git \
        libclang-dev libclang-rt-dev libelf-dev libseccomp-dev libssl-dev make patch squashfs-tools tree \
        musl-tools \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Each manifest writes to its own target directory. Fail in the build stage
# with a precise error instead of discovering a missing runtime binary only
# when the final image tries to copy it.
RUN ./orchestrator/pivot/build.sh --target "$TARGETARCH" \
    && cargo build --manifest-path firecracker/Cargo.toml --release -p firecracker \
    && cargo build --manifest-path cli/Cargo.toml --release --locked \
    && cargo build --manifest-path topology-runner/Cargo.toml --release --locked \
    && cargo build --manifest-path image-runner/Cargo.toml --release --locked \
    && cargo build --manifest-path explorer-runner/Cargo.toml --release --locked \
    && test -x firecracker/target/release/firecracker \
    && test -x cli/target/release/theseus \
    && test -x topology-runner/target/release/theseus-topology \
    && test -x image-runner/target/release/theseus-image \
    && test -x explorer-runner/target/release/theseus-explorer \
    && mkdir -p /out/instrumentation/c /out/instrumentation/llvm /out/instrumentation/go \
    && cp instrumentation/c/theseus-coverage-cc /out/instrumentation/c/ \
    && cp instrumentation/c/theseus_coverage.c /out/instrumentation/c/ \
    && cp instrumentation/c/theseus-schedule-cc /out/instrumentation/c/ \
    && cp instrumentation/c/theseus_schedule.c /out/instrumentation/c/ \
    && cp instrumentation/llvm/theseus-coverage-clang /out/instrumentation/llvm/ \
    && cp instrumentation/llvm/theseus-coverage-rustc /out/instrumentation/llvm/ \
    && cp instrumentation/llvm/theseus-coverage-inspect /out/instrumentation/llvm/ \
    && cp instrumentation/llvm/theseus_coverage.c /out/instrumentation/llvm/ \
    && cp instrumentation/go/theseus-coverage-inspect /out/instrumentation/go/ \
    && chmod +x /out/instrumentation/c/theseus-coverage-cc \
        /out/instrumentation/c/theseus-schedule-cc \
        /out/instrumentation/llvm/theseus-coverage-clang \
        /out/instrumentation/llvm/theseus-coverage-rustc \
        /out/instrumentation/llvm/theseus-coverage-inspect \
        /out/instrumentation/go/theseus-coverage-inspect \
    && cp orchestrator/pivot.bin /out/pivot \
    && image-runner/target/release/theseus-image pivot > /out/embedded-pivot.json \
    && architecture=$(dpkg --print-architecture) \
    && grep -F "\"architecture\": \"$architecture\"" /out/embedded-pivot.json \
    && pivot_sha256=$(sha256sum /out/pivot | cut -d ' ' -f 1) \
    && grep -F "\"sha256\": \"$pivot_sha256\"" /out/embedded-pivot.json \
    && printf '{\n  "format": "theseus-packaged-pivot-v1",\n  "architecture": "%s",\n  "source_commit": "%s",\n  "sha256": "%s"\n}\n' \
        "$architecture" "$THESEUS_SOURCE_COMMIT" "$pivot_sha256" > /out/pivot.json

# rebuild.sh normally installs its CI-machine dependencies itself.  The image
# above already has the smaller, fixed set needed to produce the tutorial
# kernel, so do not mutate the build image while compiling it.
RUN cd firecracker/resources \
    && THESEUS_SKIP_DEPENDENCIES=1 ./rebuild.sh kernels 6.1 \
    && cp "$(find "$(uname -m)" -maxdepth 1 -name 'vmlinux-6.1*' ! -name '*.config' | head -n 1)" /out/vmlinux \
    && module="$(find "$(uname -m)" -maxdepth 1 -name 'theseus_rng-6.1*.ko' | head -n 1)" \
    && if [ -n "$module" ]; then cp "$module" /out/theseus_rng.ko; fi

FROM golang:1.27.1-bookworm@sha256:648f440f42a0958804efb24df176f806f9d353b41f1c0627f666428e40310f6b AS go-toolchain

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS runtime

RUN printf '%s\n' 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/99snapshot \
    && sed -i \
        -e 's|http://deb.debian.org/debian-security|http://snapshot.debian.org/archive/debian-security/20260824T000000Z|' \
        -e 's|http://deb.debian.org/debian|http://snapshot.debian.org/archive/debian/20260824T000000Z|' \
        /etc/apt/sources.list.d/debian.sources \
    && apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        binutils busybox-static cargo clang cpio curl gcc jq libc6-dev libclang-rt-dev libseccomp2 llvm rustc \
    && rm -rf /var/lib/apt/lists/*

COPY --from=go-toolchain /usr/local/go /usr/local/go
ENV PATH="/usr/local/go/bin:${PATH}"

COPY --from=build /src/firecracker/target/release/firecracker /usr/local/bin/firecracker
COPY --from=build /src/cli/target/release/theseus /usr/local/bin/theseus
COPY --from=build /src/topology-runner/target/release/theseus-topology /usr/local/bin/theseus-topology
COPY --from=build /src/image-runner/target/release/theseus-image /usr/local/bin/theseus-image
COPY --from=build /src/explorer-runner/target/release/theseus-explorer /usr/local/bin/theseus-explorer
COPY --from=build /out/ /opt/theseus/

# Deliberately no entrypoint: a tutorial mounts itself at /tutorial and runs
# its own small script as the working directory.
CMD ["/bin/sh"]
