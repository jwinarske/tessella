#!/usr/bin/env bash
# SPDX-License-Identifier: BSD-2-Clause
#
# The four-view quad, replayed from a snapshot rather than from an origin.
#
#   quad.sh                 replay; the smoke test's half of `sweep.sh`
#   PARITY_RECORD=1 quad.sh fetch what the snapshot lacks, and rewrite the manifest
#
# `quad_remote.json` reads a planet archive by range, and a dated protomaps build is kept for
# about a week. Pointed straight at one, this test rots on a timer: the archive 404s, every pane
# draws two primitives, and the failure reads as a regression in whatever was being changed --
# which is a bisect, and has been one. See tools/parity/README.md.
#
# So it reads through the same record-and-replay proxy the examples use. What the origin served
# stays outside the tree in `PARITY_EXAMPLES_DATA`; what the tree carries is the manifest below,
# which is URL, range and content hash, and is what makes the snapshot checkable.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

mkdir -p "$PARITY_WORK/examples"
export PARITY_PROXY_LOG="$PARITY_WORK/quad-proxy.log"
: >"$PARITY_PROXY_LOG"

listening() { (exec 3<>"/dev/tcp/127.0.0.1/$PARITY_PROXY_PORT") 2>/dev/null; }

# This run's own proxy, for the reason `examples.sh` gives: one left over from another run may be
# recording where this one means to replay, and the report would mean nothing either way.
if listening; then
  echo "something is already listening on $PARITY_PROXY_PORT; stop it or set PARITY_PROXY_PORT" >&2
  exit 1
fi
python3 "$PARITY_DIR/examples/proxy.py" 2>"$PARITY_WORK/quad-proxy.err" &
proxy=$!
trap 'kill "$proxy" 2>/dev/null || true' EXIT
for _ in $(seq 50); do
  listening && break
  kill -0 "$proxy" 2>/dev/null || break
  sleep 0.1
done
listening || {
  echo "proxy did not start:" >&2
  cat "$PARITY_WORK/quad-proxy.err" >&2
  exit 1
}

(cd "$PARITY_WORK" && ./quad_probe "$PARITY_DIR/scenes/quad_remote.json" mat quad.ppm)

manifest="$PARITY_DIR/quad-manifest.txt"
served=$(awk '$1 == "HIT" || $1 == "REC" { $1 = ""; $2 = ""; print substr($0, 3) }' \
  "$PARITY_PROXY_LOG" | sort -u)
misses=$(grep -c -e '^MISS' -e '^FAIL' "$PARITY_PROXY_LOG" || true)
if [ "${PARITY_RECORD:-}" = 1 ]; then
  printf '%s\n' "$served" >"$manifest"
  echo "  quad: recorded $(wc -l <"$manifest") resources, $misses failed"
else
  drift=$(comm -3 <(sort -u "$manifest" 2>/dev/null) <(printf '%s\n' "$served") | wc -l)
  echo "  quad: $misses missing from the snapshot, $drift lines of manifest drift"
fi
[ "$misses" -eq 0 ]
