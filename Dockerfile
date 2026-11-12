FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p mer && cp target/release/mer /mer

FROM debian:bookworm-slim
# gnupg: the collector signs apt metadata with the site key (gpgv, used to
# verify upstream archives, is already part of the base image).
RUN apt-get update \
 && apt-get install -y --no-install-recommends gnupg \
 && rm -rf /var/lib/apt/lists/*
RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin mer \
 && mkdir -p /data /media /keys \
 && chown mer:mer /data /media /keys \
 && chmod 700 /keys
COPY --from=build /mer /usr/local/bin/mer
USER mer
ENV MER_DATA=/data
VOLUME ["/data"]
ENTRYPOINT ["mer"]
CMD ["--help"]
