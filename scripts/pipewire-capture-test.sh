#!/usr/bin/env bash
# Runs the PipeWire half of Wayland capture against a real PipeWire daemon (no desktop,
# no portal): a GStreamer test source provides a video node and the capturer reads it.
# Inside the Linux build image (deploy/linux/Dockerfile.build), from the repo root:
#   docker run --rm -v "$PWD":/src -v rconn-cargo:/usr/local/cargo/registry \
#     -v rconn-rustup:/usr/local/rustup -v rconn-linux-target:/target rconn-linux-build \
#     scripts/pipewire-capture-test.sh
set -euo pipefail
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/rconn-xdg}"
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
DBUS_SESSION_BUS_ADDRESS="$(dbus-daemon --session --fork --print-address)"
export DBUS_SESSION_BUS_ADDRESS
pipewire >/tmp/pipewire.log 2>&1 &
sleep 1
wireplumber >/tmp/wireplumber.log 2>&1 &
sleep 1
gst-launch-1.0 -q videotestsrc pattern=ball is-live=true \
  ! "video/x-raw,format=${FORMAT:-BGRx},width=${WIDTH:-638},height=${HEIGHT:-480},framerate=30/1" \
  ! pipewiresink mode=provide client-name=rconn-test-source \
    stream-properties="props,media.class=Video/Source,node.name=rconn-test-source" \
  >/tmp/gst.log 2>&1 &
node=""
for _ in $(seq 50); do
  node="$(pw-cli ls Node 2>/dev/null | awk '/^\tid /{id=$2} /node.name = "rconn-test-source"/{print id}' | tr -d ,)"
  [ -n "$node" ] && break
  sleep 0.2
done
[ -n "$node" ] || { echo "test source node did not appear"; cat /tmp/gst.log; exit 1; }
echo "test source node $node"
RCONN_PW_TEST_NODE="$node" cargo test -q -p rconn-media --lib captures_a_pipewire_node -- --nocapture
