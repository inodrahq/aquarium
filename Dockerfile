# Copyright (c) Inodra
# SPDX-License-Identifier: Apache-2.0
#
# Build:  docker build -t aquarium .
# Run:    docker run --rm aquarium info
#         docker run --rm aquarium object --id 0x6

FROM rust:1-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/aquarium /usr/local/bin/aquarium
ENTRYPOINT ["aquarium"]
CMD ["info"]
