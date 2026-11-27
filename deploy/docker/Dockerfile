# syntax=docker/dockerfile:1
# rconn signaling server (+ built-in TURN relay, + the /admin portal when portal/ exists).
# Build from the repo root:
#   docker build -f deploy/docker/Dockerfile -t rconn-signaling .
#   docker build -f deploy/docker/Dockerfile --build-arg PORTAL_UI=off ...   # no Node at all
# PORTAL_UI=on (default) builds portal/ with Node when portal/package.json exists and embeds
# it (cargo feature portal-ui); without portal/ the server builds exactly as before.
ARG PORTAL_UI=on

# The admin portal SPA. Only this stage uses Node; PORTAL_UI=off never pulls the image.
FROM node:22-bookworm-slim AS portal-on
WORKDIR /portal
RUN --mount=type=bind,target=/ctx \
    mkdir -p /out \
 && if [ -f /ctx/portal/package.json ]; then \
      cp -a /ctx/portal/. . \
      && rm -rf node_modules dist \
      && npm ci --no-audit --no-fund \
      && npm run build \
      && cp -a dist/. /out/; \
    else echo "no portal/package.json: building without the admin portal"; fi

FROM rust:1-bookworm AS portal-off
RUN mkdir -p /out

FROM portal-${PORTAL_UI} AS portal

FROM rust:1-bookworm AS build
# Parallel rustc jobs (0: cargo's default, one per CPU).
ARG CARGO_BUILD_JOBS=0
WORKDIR /src
COPY . .
COPY --from=portal /out /portal-dist
RUN features="" \
 && if [ -f /portal-dist/index.html ] \
      && grep -q '^portal-ui *=' crates/rconn-signaling/Cargo.toml; then \
      rm -rf portal/dist && mkdir -p portal && cp -a /portal-dist portal/dist \
      && features="--features portal-ui"; \
    fi \
 && if [ "$CARGO_BUILD_JOBS" != 0 ]; then export CARGO_BUILD_JOBS; else unset CARGO_BUILD_JOBS; fi \
 && echo "cargo features: ${features:-none}" \
 && cargo build --release --locked -p rconn-signaling $features \
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
HEALTHCHECK --interval=30s --timeout=3s CMD ["/bin/bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080"]
ENTRYPOINT ["rconn-signaling"]
CMD ["serve"]
