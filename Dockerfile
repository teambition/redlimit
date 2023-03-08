# syntax=docker/dockerfile:1

FROM --platform=$BUILDPLATFORM rust:latest AS builder
RUN apt-get update && apt-get install -y \
    g++-x86-64-linux-gnu libc6-dev-amd64-cross \
    g++-aarch64-linux-gnu libc6-dev-arm64-cross && \
    rm -rf /var/lib/apt/lists/*
RUN rustup target add \
    x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
RUN rustup toolchain install \
    stable-x86_64-unknown-linux-gnu stable-aarch64-unknown-linux-gnu
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CXX_x86_64_unknown_linux_gnu=x86_64-linux-gnu-g++ \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    CARGO_INCREMENTAL=0

WORKDIR /src
COPY src ./src
COPY config ./config
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release

FROM --platform=$BUILDPLATFORM ubuntu:latest
RUN ln -snf /usr/share/zoneinfo/$CONTAINER_TIMEZONE /etc/localtime && echo $CONTAINER_TIMEZONE > /etc/timezone
RUN apt-get update && apt-get install -y bash curl ca-certificates tzdata locales \
    && update-ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
	&& localedef -i en_US -c -f UTF-8 -A /usr/share/locale/locale.alias en_US.UTF-8
ENV LANG en_US.utf8

WORKDIR /app
COPY --from=builder /src/config ./config
COPY --from=builder /src/target/release/redlimit ./
ENTRYPOINT ["./redlimit"]
