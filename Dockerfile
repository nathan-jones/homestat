# Standalone image, also used as the Home Assistant add-on image.
#
#   docker build -t homestat .
#   docker run --network host -v homestat:/data homestat
#
# Host networking is required: HomeKit discovers the bridge over mDNS and
# streams video to the viewer over UDP.

FROM rust:1-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake nasm && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Dependencies first, in their own layer, so code changes rebuild only homestat.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY assets assets
COPY src src
RUN touch src/main.rs && cargo build --release --locked && cp target/release/homestat /homestat

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /homestat /usr/local/bin/homestat
ENV HOMESTAT_DATA_DIR=/data
VOLUME /data
# 51826/tcp HomeKit, 8080/tcp web UI (8099 in add-on mode)
EXPOSE 51826 8080
LABEL org.opencontainers.image.title="homestat" \
      org.opencontainers.image.description="Home Assistant, Prometheus and MQTT stats as HomeKit cameras" \
      org.opencontainers.image.source="https://github.com/nathan-jones/homestat"
ENTRYPOINT ["/usr/local/bin/homestat"]
