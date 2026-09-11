// Fetching for several hosted maps at once, and never fetching the same thing twice.
//
// Four maps whose views overlap ask for the same tiles, each through its own ticket. They share
// built tiles -- a map that wants a tile another has already built draws it without asking -- but
// four maps reaching the same new tile in the same frame each ask before any of them has it, and
// each map's fetch table is its own. So this is where those four requests become one fetch. One
// URL in flight answers every ticket that names it, from whichever map; one that has landed is
// answered out of the cache in front of the origin.
//
// That is flatness at the fetch level. Whether the four maps then built the tile once is the
// producer's to say, and no count of builds crosses the ABI, so the report says it is not
// measured rather than letting this number stand for both.
//
// # One body at a time
//
// Every map in the instance shares one scratch block for the bytes of an answer. The copy into
// it and the call that takes them are one synchronous step in `TessellaMap.answer`, and this is
// the only caller, so nothing can interleave -- the assertion below is what keeps a later change
// from making it otherwise.

/**
 * @typedef {{status: number, body: Uint8Array, cached?: boolean}} Answer
 * @typedef {{map: number, ticket: bigint, url: string}} Ask
 */

export class FetchHost {
  /**
   * @param {(url: string) => Promise<Answer>} fetchOne  the cache-wrapped fetch
   * @param {{rewrite?: (url: string) => string}} [options]
   */
  constructor(fetchOne, { rewrite = (url) => url } = {}) {
    this.fetchOne = fetchOne;
    this.rewrite = rewrite;
    /** URL -> the fetch answering it, while any ticket for it is undelivered. */
    this.inFlight = new Map();
    /** Tickets taken and not yet answered, in the order they were taken. */
    this.queue = [];
    /** Every URL any map ever asked for. */
    this.distinct = new Set();
    this.answering = false;
    this.totals = { tickets: 0, fetches: 0, coalesced: 0, cacheHits: 0, failed: 0 };
    this.frame = FetchHost.#zero();
  }

  static #zero() {
    return { tickets: 0, fetches: 0, coalesced: 0, cacheHits: 0 };
  }

  /** The counters since the last call, which is one frame's worth. */
  takeFrameCounters() {
    const frame = this.frame;
    this.frame = FetchHost.#zero();
    return frame;
  }

  /**
   * Takes every map's requests and starts whatever is not already under way.
   *
   * @param {import("../map.js").TessellaMap[]} maps
   */
  collect(maps) {
    maps.forEach((map, index) => {
      for (const [ticket, url] of map.takeRequests()) {
        this.frame.tickets++;
        this.totals.tickets++;
        this.distinct.add(url);
        let entry = this.inFlight.get(url);
        if (entry) {
          this.frame.coalesced++;
          this.totals.coalesced++;
        } else {
          entry = this.#start(url);
          this.inFlight.set(url, entry);
        }
        entry.waiting++;
        this.queue.push({ map: index, ticket, url });
      }
    });
  }

  #start(url) {
    const entry = { waiting: 0, answer: null, done: false, promise: null };
    // Started inside a promise, so a fetch that throws before it has one fails its tickets like
    // any other fetch that did not happen, rather than out of `collect` with tickets half queued.
    entry.promise = Promise.resolve()
      .then(() => this.fetchOne(this.rewrite(url)))
      .then(
        (answer) => {
          entry.answer = answer;
          entry.done = true;
          // Counted when it lands, since only then is it known whether the cache or the origin
          // answered.
          if (answer.cached) {
            this.frame.cacheHits++;
            this.totals.cacheHits++;
          } else {
            this.frame.fetches++;
            this.totals.fetches++;
          }
        },
        () => {
          entry.answer = null;
          entry.done = true;
          this.totals.failed++;
        },
      );
    return entry;
  }

  /** True while any ticket is waiting on a fetch. */
  get busy() {
    return this.queue.length > 0;
  }

  /** Resolves once every fetch now under way has landed. */
  async settle() {
    await Promise.all([...this.inFlight.values()].map((entry) => entry.promise));
  }

  /**
   * Answers every ticket whose fetch has landed, in the order the tickets were taken.
   *
   * Take order rather than landing order: with every fetch landed, which is what run A waits
   * for, this is the same sequence of answers every time, and a native run answering in the same
   * order gets the same stream.
   *
   * @param {import("../map.js").TessellaMap[]} maps
   */
  deliver(maps) {
    if (this.answering) {
      throw new Error("a second answer began while one was in the scratch block");
    }
    this.answering = true;
    try {
      const waiting = [];
      for (const ask of this.queue) {
        const entry = this.inFlight.get(ask.url);
        if (!entry.done) {
          waiting.push(ask);
          continue;
        }
        const map = maps[ask.map];
        if (entry.answer === null) {
          map.fail(ask.ticket);
        } else {
          map.answer(ask.ticket, entry.answer.status, entry.answer.body ?? new Uint8Array(0));
        }
        if (--entry.waiting === 0) {
          this.inFlight.delete(ask.url);
        }
      }
      this.queue = waiting;
    } finally {
      this.answering = false;
    }
  }
}
