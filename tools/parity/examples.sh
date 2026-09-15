#!/usr/bin/env bash
# SPDX-License-Identifier: BSD-2-Clause
#
# The MapLibre GL JS examples, both renderers, gross pixels per example and camera.
#
#   examples.sh [slug...]            replay from the snapshot; every example when none is named
#   PARITY_RECORD=1 examples.sh ...  fetch what the snapshot lacks, and rewrite the manifests
#
# Each `examples/<slug>/fixture.json` is a base style, what the example adds on load, and the
# settled cameras to compare (compose.py says how). The proxy stands in front of every origin the
# example reaches, so a replay touches no network; a URL it does not hold is a MISS and fails the
# example rather than letting it measure something else.
#
# A replay also checks what it served against the example's manifest. A snapshot that has drifted
# from the manifest is reported, because the gross number beside it is no longer the same
# question.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

EX="$PARITY_DIR/examples"
mkdir -p "$PARITY_WORK/examples"
export PARITY_PROXY_LOG="$PARITY_WORK/examples/proxy.log"

listening() { (exec 3<>"/dev/tcp/127.0.0.1/$PARITY_PROXY_PORT") 2>/dev/null; }

# The proxy is always this run's own. One left over from another run logs somewhere else and may be
# recording when this one means to replay, and either would make the report below meaningless.
if listening; then
  echo "something is already listening on $PARITY_PROXY_PORT; stop it or set PARITY_PROXY_PORT" >&2
  exit 1
fi
python3 "$EX/proxy.py" 2>"$PARITY_WORK/examples/proxy.err" &
proxy=$!
trap 'kill "$proxy" 2>/dev/null || true' EXIT
for _ in $(seq 50); do
  listening && break
  kill -0 "$proxy" 2>/dev/null || break
  sleep 0.1
done
listening || {
  echo "proxy did not start:" >&2
  cat "$PARITY_WORK/examples/proxy.err" >&2
  exit 1
}

if [ $# -eq 0 ]; then
  slugs=()
  for f in "$EX"/*/fixture.json; do
    [ -f "$f" ] && slugs+=("$(basename "$(dirname "$f")")")
  done
  set -- "${slugs[@]}"
fi

status=0
for slug in "$@"; do
  fixture="$EX/$slug/fixture.json"
  [ -f "$fixture" ] || {
    echo "no fixture at $fixture" >&2
    status=1
    continue
  }
  : >"$PARITY_PROXY_LOG"
  style="$PARITY_WORK/examples/$slug.json"
  if ! cameras=$(python3 "$EX/compose.py" "$fixture" "$style"); then
    echo "COMPOSE FAILED $slug" >&2
    status=1
    continue
  fi
  while read -r lat lon zoom width height pitch bearing; do
    bash "$PARITY_DIR/parity.sh" "$style" "$lat" "$lon" "$zoom" "$width" "$height" "$pitch" \
      "$bearing" || status=1
  done <<<"$cameras"

  misses=$(grep -c -e '^MISS' -e '^FAIL' "$PARITY_PROXY_LOG" || true)
  served=$(awk '$1 == "HIT" || $1 == "REC" { print $2, $3, $4 }' "$PARITY_PROXY_LOG" | sort -u)
  if [ "${PARITY_RECORD:-}" = 1 ]; then
    printf '%s\n' "$served" >"$EX/$slug/manifest.txt"
    echo "  $slug: recorded $(wc -l <"$EX/$slug/manifest.txt") resources, $misses failed"
    [ "$misses" -eq 0 ] || status=1
  else
    drift=$(comm -3 <(sort -u "$EX/$slug/manifest.txt" 2>/dev/null) <(printf '%s\n' "$served") | wc -l)
    echo "  $slug: $misses missing from the snapshot, $drift lines of manifest drift"
    [ "$misses" -eq 0 ] || status=1
  fi
done
exit "$status"
