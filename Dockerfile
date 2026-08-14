FROM rust:1.75-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /build/target/release/tempshare ./tempshare
COPY --from=builder /build/static ./static

ENV TEMPSHARE_STORAGE_DIR=/data/shared_files
ENV TEMPSHARE_DB_PATH=/data/tempshare.db
ENV TEMPSHARE_BIND_ADDR=0.0.0.0:7420
ENV TEMPSHARE_PUBLIC_BIND_ADDR=0.0.0.0:7421
ENV TEMPSHARE_AUTO_TUNNEL=false

VOLUME ["/data"]
EXPOSE 7420 7421

ENTRYPOINT ["./tempshare"]
