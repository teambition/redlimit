FROM --platform=$BUILDPLATFORM rust:latest AS builder
WORKDIR /src
COPY src ./src
COPY config ./config
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release

FROM --platform=$BUILDPLATFORM ubuntu:latest
RUN apt-get update && apt-get install -y locales && rm -rf /var/lib/apt/lists/* \
	&& localedef -i en_US -c -f UTF-8 -A /usr/share/locale/locale.alias en_US.UTF-8
ENV LANG en_US.utf8

WORKDIR /app
COPY --from=builder /src/config ./config
COPY --from=builder /src/target/release/redlimit ./
ENTRYPOINT ["./redlimit"]
