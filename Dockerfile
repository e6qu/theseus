# syntax=docker/dockerfile:1
#
# A Theseus runtime is an inseparable set: the service binary, the guest
# kernel, and (on arm64) the module built for that exact kernel.  Tutorials
# use this image rather than a checkout of this repository.

FROM rust:1.97.0-bookworm AS build

RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        bc bison busybox-static cpio curl dwarves flex gcc git libclang-dev \
        libelf-dev libseccomp-dev libssl-dev make patch squashfs-tools tree \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN cargo build --manifest-path firecracker/Cargo.toml --release -p firecracker \
    && cargo build --manifest-path cli/Cargo.toml --release --locked \
    && cargo build --manifest-path topology-runner/Cargo.toml --release --locked \
    && cargo build --manifest-path explorer-runner/Cargo.toml --release --locked

# rebuild.sh normally installs its CI-machine dependencies itself.  The image
# above already has the smaller, fixed set needed to produce the tutorial
# kernel, so do not mutate the build image while compiling it.
RUN cd firecracker/resources \
    && THESEUS_SKIP_DEPENDENCIES=1 ./rebuild.sh kernels 6.1 \
    && mkdir -p /out \
    && cp "$(find "$(uname -m)" -maxdepth 1 -name 'vmlinux-6.1*' ! -name '*.config' | head -n 1)" /out/vmlinux \
    && module="$(find "$(uname -m)" -maxdepth 1 -name 'theseus_rng-6.1*.ko' | head -n 1)" \
    && if [ -n "$module" ]; then cp "$module" /out/theseus_rng.ko; fi

FROM debian:bookworm-slim AS runtime

RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        busybox-static cpio curl gcc libseccomp2 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/firecracker/build/cargo_target/release/firecracker /usr/local/bin/firecracker
COPY --from=build /src/cli/target/release/theseus /usr/local/bin/theseus
COPY --from=build /src/topology-runner/target/release/theseus-topology /usr/local/bin/theseus-topology
COPY --from=build /src/explorer-runner/target/release/theseus-explorer /usr/local/bin/theseus-explorer
COPY --from=build /out/ /opt/theseus/

# Deliberately no entrypoint: a tutorial mounts itself at /tutorial and runs
# its own small script as the working directory.
CMD ["/bin/sh"]
