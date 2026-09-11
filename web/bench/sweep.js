// The four-view zoom sweep, as `crates/tessella-orchestrate/src/sweep.rs` defines it.
//
// A port, and the Rust module is the oracle: the native trace driver prints the values it took
// from `four_views()` and `sweep_zooms()`, and the harness compares these against them number for
// number. The arithmetic is written in the same order as the Rust so the floats agree to the bit
// rather than to a tolerance -- a sweep that visits z11.999999 where the other visits z12 crosses
// an integer zoom one frame apart, and that is a different trace.

/** The bottom of the sweep. `SWEEP_LOW`. */
export const SWEEP_LOW = 8.0;
/** The top of the sweep. `SWEEP_HIGH`. */
export const SWEEP_HIGH = 16.0;

/**
 * The four cameras: one base and three offsets, unequal on the two axes so a transposed cover is
 * not the same set.
 *
 * `four_views()` states its viewports as 1024x768. The benchmark draws four panes of one canvas,
 * so the size is the caller's and is passed in; the centers and offsets are the oracle's.
 *
 * @returns {{latitude: number, longitude: number, zoom: number, width: number, height: number}[]}
 */
export function fourViews(width, height) {
  const base = { longitude: -0.11, latitude: 51.505, zoom: SWEEP_LOW, width, height };
  return [
    base,
    { ...base, longitude: base.longitude + 0.01 },
    { ...base, latitude: base.latitude + 0.006 },
    { ...base, longitude: base.longitude + 0.01, latitude: base.latitude + 0.006 },
  ];
}

/**
 * The zoom of each frame of a z8 -> z16 -> z8 sweep.
 *
 * `steps` frames in each direction, the turn at the top visited once: `2 * steps - 1` frames.
 */
export function sweepZooms(steps) {
  const n = Math.max(steps, 2);
  const span = SWEEP_HIGH - SWEEP_LOW;
  const up = [];
  for (let i = 0; i < n; i++) {
    up.push(SWEEP_LOW + (span * i) / (n - 1));
  }
  return [...up, ...up.slice(0, -1).reverse()];
}
