# Dependencies are compiled in their own layer (cargo-chef), so a change to the application
# code recompiles only the application, not every crate it depends on.
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p crewaware
# migrations/ is embedded into the binary at compile time, so it must be in the build context.
COPY . .
RUN cargo build --release -p crewaware

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
 && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 --create-home crewaware
COPY --from=build /src/target/release/crewaware /usr/local/bin/crewaware
USER crewaware
ENV RUST_LOG=info
EXPOSE 8080/tcp
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 CMD curl -fsS http://127.0.0.1:8080/api/public/ready || exit 1
ENTRYPOINT ["/usr/local/bin/crewaware"]
