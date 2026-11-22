FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p t7-server

FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/t7-server /usr/local/bin/t7-server
VOLUME /data
WORKDIR /data
EXPOSE 9988/tcp
EXPOSE 9988/udp
ENTRYPOINT ["/usr/local/bin/t7-server", "--config", "/data/t7-server.toml", "--bind", "0.0.0.0:9988"]
