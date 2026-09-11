// The benchmark's acceptance table, evaluated against one run's report.
//
// Shared by the page, for a run in whatever browser somebody opened it in, and by the harness,
// which adds the one gate a page cannot evaluate alone: determinism, which needs the native trace.
//
// What is not measured is said, in a line of its own. An absent line reads as a pass.

/** The frame budget, in milliseconds: one 60 Hz vsync. */
export const BUDGET_MS = 16.7;

/**
 * @param {object} report  what `bench.worker.js` returned
 * @param {{budget?: boolean}} [options]  whether the budget gates; it is only meaningful on a
 *   named machine, so a CI run reports it and does not gate on it
 * @returns {{property: string, statement: string, value: unknown, pass: boolean|null,
 *            gating: boolean}[]}
 */
export function evaluate(report, { budget = false } = {}) {
  const frameP99 = report.timings.frame_cpu_ms?.p99 ?? null;
  const rows = [
    {
      property: "frame budget",
      statement: `sum(tick_ms) + absorb_ms + draw_ms < ${BUDGET_MS} at p99, clock B`,
      value: frameP99,
      pass: report.clock === "B" && frameP99 !== null ? frameP99 < BUDGET_MS : null,
      gating: budget && report.clock === "B",
    },
    {
      property: "tick status",
      statement: "every tick answered OK, or a full ring or region",
      value: report.tick_errors,
      pass: report.tick_errors === 0,
      gating: true,
    },
    {
      property: "coverage",
      statement: "zero sweep frames with any covered[i] == false",
      value: report.uncovered_frames,
      pass: report.uncovered_frames === 0,
      gating: true,
    },
    {
      property: "ring occupancy",
      statement: "ring_full == 0 and region_full == 0 across the sweep",
      value: { ring_full: report.ring_full, region_full: report.region_full },
      pass: report.ring_full === 0 && report.region_full === 0,
      gating: true,
    },
    {
      property: "flatness",
      statement: "origin fetches == distinct URLs asked (fetch level)",
      value: {
        origin_fetches: report.fetch.origin_fetches,
        distinct_urls: report.fetch.distinct_urls,
      },
      pass: report.fetch.origin_fetches === report.fetch.distinct_urls && report.fetch.failed === 0,
      gating: true,
    },
    {
      property: "blank frames",
      statement: "blank == 0 across the sweep",
      value: report.blank,
      pass: report.blank === 0,
      gating: true,
    },
  ];
  if (report.pixels) {
    rows.push({
      property: "pixels",
      statement: "no clear-color pixel in any pane of the last frame",
      value: report.pixels.holes,
      pass: report.pixels.holes.every((n) => n === 0),
      gating: true,
    });
  }
  return rows;
}

/** The measurements this run does not make, each as the line that says so. */
export function absent(report) {
  const lines = [];
  if (!report.pixels) {
    lines.push("PIXELS NOT CHECKED: no webgl2 here");
  }
  lines.push("GPU TIME NOT MEASURED");
  lines.push("FLATNESS: fetch-level only");
  lines.push("BUILD-LEVEL FLATNESS: NOT MEASURED (four independent maps)");
  lines.push("SYMBOL POPS: NOT MEASURED (fills only)");
  return lines;
}
