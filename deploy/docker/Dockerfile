# rconn signaling server (+ built-in TURN relay). Build from the repo root:
#   docker build -f deploy/docker/Dockerfile -t rconn-signaling .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p rconn-signaling \
 && strip target/release/rconn-signaling

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /data rconn \
 && mkdir -p /data /keys && chown rconn /data /keys && chmod 700 /keys
COPY --from=build /src/target/release/rconn-signaling /usr/local/bin/rconn-signaling
USER rconn
# The master key that seals org CA keys lives outside /data (and its backups).
ENV RCONN_DATA_DIR=/data RCONN_MASTER_KEY_FILE=/keys/master.key RCONN_BIND=0.0.0.0:8080
VOLUME /data /keys
# 8080/tcp signaling (put TLS in front), 3478/udp TURN, relay range udp
EXPOSE 8080/tcp 3478/udp 49160-49260/udp
HEALTHCHECK --interval=30s --timeout=3s CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080"]
ENTRYPOINT ["rconn-signaling"]
CMD ["serve"]
