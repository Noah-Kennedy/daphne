# This Dockerfile is a bit convoluted as we are going to great effort to have
# things fall into natural layers, so that in iterative development work,
# e.g. debugging interoperability tests, only things that have changed get
# rebuilt.  This cuts build time on one system from 300+ seconds to 96 sec

FROM rust:1.68-alpine AS builder
WORKDIR /tmp/dap_test
RUN apk add --update \
    bash \
    g++ \
    make \
    npm \
    openssl-dev \
    wasm-pack
RUN npm install -g wrangler@2.12.2

# Pre-install worker-build and Rust's wasm32 target to speed up our custom build command
RUN cargo install --git https://github.com/cloudflare/workers-rs
RUN rustup target add wasm32-unknown-unknown

# Use the fast HTTP-based registry instead of cloning the index from github
RUN mkdir /root/.cargo
COPY docker/config.toml /root/.cargo

# Make an "empty" crate that so we can compile everything that isn't daphne into a layer.
# This means that changing daphne will only rebuild it, not everything.
COPY docker/empty-top-cargo.toml ./Cargo.toml
COPY Cargo.lock ./
RUN mkdir -p ./empty/src; touch ./empty/src/lib.rs
COPY docker/empty-cargo.toml ./empty/Cargo.toml
WORKDIR /tmp/dap_test/empty
RUN cargo build --lib --release --target wasm32-unknown-unknown
COPY docker/empty-wrangler.toml ./empty/wrangler.toml
# Prebuild worker infrastruture
RUN worker-build
# Get back to building daphne!
WORKDIR /tmp/dap_test
COPY docker/top-cargo.toml ./Cargo.toml
COPY daphne ./daphne
COPY daphne_worker ./daphne_worker
COPY daphne_worker_test ./daphne_worker_test
COPY docker/wrangler.toml ./daphne_worker_test/wrangler.toml
WORKDIR /tmp/dap_test/daphne_worker_test
RUN wrangler publish --dry-run

FROM alpine:3.16 AS test
RUN apk add --update npm bash
RUN npm install -g miniflare@2.12.2
COPY --from=builder /tmp/dap_test/daphne_worker_test/wrangler.toml /wrangler.toml
COPY --from=builder /tmp/dap_test/daphne_worker_test/build/worker/* /build/worker/
EXPOSE 8080
# `-B ""` to skip build command.
ENTRYPOINT ["miniflare", "--modules", "--modules-rule=CompiledWasm=**/*.wasm", "/build/worker/shim.mjs", "-B", ""]

FROM test AS helper

ENTRYPOINT ["miniflare", "--modules", "--modules-rule=CompiledWasm=**/*.wasm", "/build/worker/shim.mjs", "-B", "", "-p", "8080", "--wrangler-env=helper"]

FROM test AS leader

ENTRYPOINT ["miniflare", "--modules", "--modules-rule=CompiledWasm=**/*.wasm", "/build/worker/shim.mjs", "-B", "", "-p", "8080", "--wrangler-env=leader"]
