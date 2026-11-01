FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/sonora-sessions /sonora-sessions
ENV PORT=8080
# How many people may be connected at once. An idle connection takes about 9 KiB, so 100000 sit
# near 1 GB and leave room for message queues inside a 6 GB container.
ENV MAX_CONNECTIONS=100000
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["/sonora-sessions", "health"]
ENTRYPOINT ["/sonora-sessions"]
