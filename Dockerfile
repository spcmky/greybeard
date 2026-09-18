# Build
FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin greybeard

# Runtime — Git for local reviews and CA certs (JWT signing is in-process via jsonwebtoken).
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 10001 -m greybeard
COPY --from=build /app/target/release/greybeard /usr/local/bin/greybeard
USER 10001
WORKDIR /home/greybeard
EXPOSE 8080
ENTRYPOINT ["greybeard"]
CMD ["serve"]
