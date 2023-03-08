# syntax=docker/dockerfile:1

FROM --platform=$BUILDPLATFORM rust:latest AS builder

WORKDIR /src
COPY src ./src
COPY config ./config
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release

FROM --platform=$BUILDPLATFORM ubuntu:latest
RUN ln -snf /usr/share/zoneinfo/$CONTAINER_TIMEZONE /etc/localtime && echo $CONTAINER_TIMEZONE > /etc/timezone
RUN apt-get update \
    && apt-get install -y bash curl ca-certificates tzdata locales extra-runtime-dependencies \
    && update-ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
	  && localedef -i en_US -c -f UTF-8 -A /usr/share/locale/locale.alias en_US.UTF-8
ENV LANG en_US.utf8

WORKDIR /app
COPY --from=builder /src/config ./config
COPY --from=builder /src/target/release/redlimit ./
ENTRYPOINT ["./redlimit"]
