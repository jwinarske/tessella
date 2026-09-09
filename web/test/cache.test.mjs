// The cache's own rules, without a browser or a network.
//
// What it is checking is not "does it remember" -- that much is a Map -- but the two things a
// cache is wrong about when it is wrong: serving something the origin said had expired, and
// keeping something the origin never said it could keep.

import { test } from "node:test";
import assert from "node:assert/strict";

import { caching, freshUntil, MemoryStore } from "../cache.js";

/// A fetch that counts its calls and answers with whatever headers the test names.
function origin(headers, body = new Uint8Array([1, 2, 3])) {
  const state = { calls: 0 };
  return [
    async (url) => {
      state.calls++;
      return { status: 200, body, headers: new Headers(headers), url };
    },
    state,
  ];
}

test("a fresh answer is served from the store rather than fetched again", async () => {
  const [fetchOne, state] = origin({ "cache-control": "max-age=60" });
  let now = 1_000_000;
  const fetch = caching(fetchOne, new MemoryStore(), () => now);

  const first = await fetch("https://o.invalid/a");
  assert.equal(first.cached, false);
  assert.equal(state.calls, 1);

  const second = await fetch("https://o.invalid/a");
  assert.equal(second.cached, true, "the second call went to the origin");
  assert.equal(state.calls, 1);
  assert.deepEqual([...second.body], [1, 2, 3], "the body did not survive the round trip");
});

test("an answer past its max-age is fetched again", async () => {
  const [fetchOne, state] = origin({ "cache-control": "max-age=60" });
  let now = 1_000_000;
  const fetch = caching(fetchOne, new MemoryStore(), () => now);

  await fetch("https://o.invalid/a");
  now += 61_000;
  const again = await fetch("https://o.invalid/a");
  assert.equal(again.cached, false, "a stale entry was served");
  assert.equal(state.calls, 2);
});

test("an answer with no freshness is never kept", async () => {
  // Guessing a lifetime for a resource that did not state one is how a map draws last week's
  // tiles. Serving it and forgetting costs a fetch; the alternative costs correctness.
  const [fetchOne, state] = origin({});
  const fetch = caching(fetchOne, new MemoryStore(), () => 0);
  await fetch("https://o.invalid/a");
  await fetch("https://o.invalid/a");
  assert.equal(state.calls, 2);
});

test("no-store is honored even with a max-age beside it", async () => {
  const [fetchOne, state] = origin({ "cache-control": "no-store, max-age=600" });
  const fetch = caching(fetchOne, new MemoryStore(), () => 0);
  await fetch("https://o.invalid/a");
  await fetch("https://o.invalid/a");
  assert.equal(state.calls, 2);
});

test("a 404 is cached like any other answer the origin stood behind", async () => {
  // An absent tile is an edge of coverage, and re-asking for it every tick is the cost this
  // avoids. It is only kept because the origin said it could be.
  let calls = 0;
  const fetchOne = async () => {
    calls++;
    return {
      status: 404,
      body: new Uint8Array(0),
      headers: new Headers({ "cache-control": "max-age=300" }),
    };
  };
  const fetch = caching(fetchOne, new MemoryStore(), () => 0);
  assert.equal((await fetch("https://o.invalid/gone")).status, 404);
  const again = await fetch("https://o.invalid/gone");
  assert.equal(again.status, 404);
  assert.equal(again.cached, true);
  assert.equal(calls, 1);
});

test("a truncated entry is a miss rather than a short body", async () => {
  // An OPFS write is not atomic: a tab closed mid-write leaves a short file. Without the length
  // in the frame that is a tile with its end cut off, which decodes as something.
  const store = new MemoryStore();
  const [fetchOne, state] = origin({ "cache-control": "max-age=60" });
  const fetch = caching(fetchOne, store, () => 0);
  await fetch("https://o.invalid/a");

  const held = await store.get("https://o.invalid/a");
  await store.put("https://o.invalid/a", held.subarray(0, held.length - 1));

  const again = await fetch("https://o.invalid/a");
  assert.equal(again.cached, false, "a half-written entry was served");
  assert.equal(state.calls, 2);
});

test("freshness is read the way the spec orders it", () => {
  const at = 1_000_000;
  assert.equal(freshUntil({ headers: new Headers({ "cache-control": "max-age=30" }) }, at), at + 30_000);
  // `Cache-Control` wins over `Expires`, which is the order mbgl reads them in too.
  const both = new Headers({ "cache-control": "max-age=30", expires: "Thu, 01 Jan 2099 00:00:00 GMT" });
  assert.equal(freshUntil({ headers: both }, at), at + 30_000);
  const only = new Headers({ expires: "Thu, 01 Jan 2099 00:00:00 GMT" });
  assert.ok(freshUntil({ headers: only }, at) > at);
  assert.equal(freshUntil({ headers: new Headers({ expires: "not a date" }) }, at), 0);
});
