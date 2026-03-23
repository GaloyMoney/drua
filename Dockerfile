FROM rust:1.85-slim-bookworm AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY cli/ cli/
COPY domain/ domain/
COPY graphql/ graphql/
COPY web/ web/
COPY .sqlx/ .sqlx/

ENV SQLX_OFFLINE=true
RUN cargo build --release --bin galoy-agents

FROM debian:bookworm-slim

ARG COMMITHASH
ARG BUILDTIME

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/galoy-agents /usr/local/bin/

LABEL org.opencontainers.image.revision=$COMMITHASH
LABEL org.opencontainers.image.created=$BUILDTIME

USER 1000

ENTRYPOINT ["galoy-agents"]
