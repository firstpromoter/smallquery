FROM rust:1.94-bookworm AS build
WORKDIR /src
# Dependencies first (bundled DuckDB is a ~10 minute C++ build), so source edits reuse the layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src src
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /src/target/release/smallquery /usr/local/bin/smallquery
EXPOSE 9050
ENTRYPOINT ["smallquery"]
CMD ["--port=9050"]
