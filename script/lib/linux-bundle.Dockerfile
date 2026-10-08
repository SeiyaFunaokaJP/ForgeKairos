# The build environment of `script/bundle.ps1 -Os linux` when it runs in a
# container. script/bundle.ps1 stages this file, script/linux and
# rust-toolchain.toml in a context folder and builds the image from there.
ARG BASE_IMAGE=ubuntu:22.04
FROM ${BASE_IMAGE}

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=Etc/UTC \
    RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# script/linux expects these on a desktop install but a base image lacks them.
# rustup goes in first without a toolchain: script/linux would otherwise
# install it with the latest stable, which rust-toolchain.toml never uses.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl gnupg lsb-release \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none

COPY linux /tmp/forgekairos/linux
RUN bash /tmp/forgekairos/linux && rm -rf /var/lib/apt/lists/*

# Installing the pinned toolchain here keeps it out of the cargo volume, so a
# toolchain bump rebuilds this layer instead of leaving a stale one behind.
COPY rust-toolchain.toml /tmp/forgekairos/toolchain/rust-toolchain.toml
RUN cd /tmp/forgekairos/toolchain && rustup toolchain install && rustup show active-toolchain
