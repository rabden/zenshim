# Builds zenshim from source. Run it headless:
#   docker run -d --name zenshim -p 127.0.0.1:8480:8480 -v zenshim:/data zenshim
#   docker exec zenshim zenshim --show-key
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock marker.txt ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin zenshim \
    && mkdir /data && chown zenshim:zenshim /data
COPY --from=build /src/target/release/zenshim /usr/local/bin/zenshim
USER zenshim
# The config (with the generated gateway key) lives in the volume.
ENV ZENSHIM_CONFIG_DIR=/data HOME=/data ZENSHIM_HOST=0.0.0.0
VOLUME /data
EXPOSE 8480
ENTRYPOINT ["zenshim", "--no-tui", "--no-wizard"]
