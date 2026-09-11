// Percentiles, taken the way the native sweep bench takes them.
//
// `benches/four_view_sweep.rs` indexes the sorted samples at `round((n - 1) * fraction)`, with no
// interpolation and no warm-up discarded. The same rule here, so a p99 on each side of a
// comparison is the same statistic and not merely the same name.

/** @param {number[]} samples */
export function summary(samples) {
  const finite = samples.filter((x) => Number.isFinite(x));
  if (finite.length === 0) {
    return null;
  }
  const sorted = [...finite].sort((a, b) => a - b);
  const at = (fraction) => sorted[Math.round((sorted.length - 1) * fraction)];
  const round = (x) => Math.round(x * 1000) / 1000;
  return {
    n: sorted.length,
    p50: round(at(0.5)),
    p95: round(at(0.95)),
    p99: round(at(0.99)),
    max: round(sorted[sorted.length - 1]),
  };
}
