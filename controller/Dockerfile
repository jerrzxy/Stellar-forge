# Builds the Stellar-K8s ephemeral testnet operator (StellarTestnet CRD).
#
#   docker build -t <registry>/stellar-testnet-operator:latest controller
#
FROM rust:1-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin stellar-testnet-operator \
    && cp target/release/stellar-testnet-operator /stellar-testnet-operator

# The binary links glibc, so use the distroless image that ships it.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /stellar-testnet-operator /usr/local/bin/stellar-testnet-operator
ENTRYPOINT ["/usr/local/bin/stellar-testnet-operator"]
