# Stable
FROM rust:latest as builder

RUN cargo --version

# Build Dependencies
WORKDIR /build
COPY . .
RUN cargo test
RUN cargo build --release

FROM debian:stable-slim AS run
RUN apt-get update && apt-get install -y openssl ca-certificates
COPY --from=builder /build/target/release/dumb-caching-proxy dumb-caching-proxy

ENV TINI_VERSION v0.19.0
ADD https://github.com/krallin/tini/releases/download/${TINI_VERSION}/tini /tini
RUN chmod +x /tini
ENTRYPOINT ["/tini", "--"]
CMD ["./dumb-caching-proxy"]
