//! What the glyph manager fetches, and what it declines to fetch twice.
//!
//! The parse is `glyph_pbf`'s business. This is about the bookkeeping around it, which is where
//! a glyph pipeline goes wrong quietly: a manager that re-requests a range on every tile is
//! correct in every observable output and unusable over a real link.

use std::cell::RefCell;
use std::collections::BTreeSet;

use tessella_glyph::manager::{FontStack, GlyphManager, LoadError};
use tessella_glyph::pbf::Range;
use tessella_storage::source::{FetchError, FileSource, Response};

const REAL: &[u8] = include_bytes!("../../../tests/glyph-fixtures/glyphs.pbf");

/// How a scripted origin answers one request.
type Answer = Box<dyn Fn(&str) -> Result<Response, FetchError>>;

/// A source that records what was asked for and answers from a script.
struct Origin {
    asked: RefCell<Vec<String>>,
    answer: Answer,
}

impl Origin {
    fn serving_ascii() -> Self {
        Self {
            asked: RefCell::new(Vec::new()),
            answer: Box::new(|url: &str| {
                // Only the 0-255 range exists; anything else is a 404, as a real origin does
                // for a range the font does not cover.
                let body = if url.contains("0-255") {
                    REAL.to_vec()
                } else {
                    Vec::new()
                };
                Ok(Response {
                    status: if body.is_empty() { 404 } else { 200 },
                    body,
                    ..Response::default()
                })
            }),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl FileSource for Origin {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.asked.borrow_mut().push(url.to_string());
        (self.answer)(url)
    }
}

// `FileSource` is `Send + Sync`; the `RefCell` here is single-threaded test bookkeeping.
unsafe impl Sync for Origin {}
unsafe impl Send for Origin {}

fn manager() -> GlyphManager {
    GlyphManager::new("https://example.com/fonts/{fontstack}/{range}.pbf")
}

fn noto() -> FontStack {
    FontStack::new(["Noto Sans Regular"])
}

/// The URL is the style's template with both tokens filled, and the stack percent-encoded.
///
/// A font stack is `Noto Sans Regular,Arial Unicode MS Regular` — spaces and commas, both of
/// which have to survive into a path segment as escapes rather than as themselves.
#[test]
fn the_url_fills_both_tokens() {
    let manager = manager();
    assert_eq!(
        manager.url_for(
            &noto(),
            Range {
                first: 0,
                last: 255
            }
        ),
        "https://example.com/fonts/Noto%20Sans%20Regular/0-255.pbf"
    );

    // Lowercase hex, which is what mbgl's `percentEncode` emits — the URL is a cache key on
    // both sides, so the case has to agree.
    let two = FontStack::new(["Noto Sans Regular", "Arial Unicode MS Regular"]);
    assert_eq!(
        manager.url_for(
            &two,
            Range {
                first: 256,
                last: 511
            }
        ),
        "https://example.com/fonts/Noto%20Sans%20Regular%2cArial%20Unicode%20MS%20Regular/256-511.pbf"
    );
}

/// A token nothing recognises survives verbatim, braces and all.
///
/// mbgl's `replaceTokens` rule. A URL may legitimately contain braces, and dropping them
/// produces a request that 404s with no clue why.
#[test]
fn an_unknown_token_is_left_alone() {
    let manager = GlyphManager::new("https://example.com/{fontstack}/{whatever}/{range}.pbf");
    assert_eq!(
        manager.url_for(
            &FontStack::new(["A"]),
            Range {
                first: 0,
                last: 255
            }
        ),
        "https://example.com/A/{whatever}/0-255.pbf"
    );
}

/// Codepoints in one range cost one request, however many of them there are.
#[test]
fn one_range_is_fetched_once_for_every_codepoint_in_it() {
    let origin = Origin::serving_ascii();
    let mut manager = manager();

    let text: Vec<u32> = "Hello, world".chars().map(|c| c as u32).collect();
    manager.load(&noto(), text, &origin).expect("loads");

    assert_eq!(origin.asked().len(), 1, "{:?}", origin.asked());
    assert_eq!(manager.requests(), 1);
    assert!(manager.glyph(&noto(), u32::from(b'H')).is_some());
    assert!(manager.glyph(&noto(), u32::from(b'w')).is_some());
}

/// And asking again costs nothing.
///
/// The property that makes this usable: a label appears on many tiles and many views, and each
/// of them asks. A manager without this is correct in every output and unusable over a link.
#[test]
fn a_range_already_held_is_not_fetched_again() {
    let origin = Origin::serving_ascii();
    let mut manager = manager();
    let text: Vec<u32> = "Hello".chars().map(|c| c as u32).collect();

    for _ in 0..20 {
        manager.load(&noto(), text.clone(), &origin).expect("loads");
    }

    assert_eq!(origin.asked().len(), 1, "{:?}", origin.asked());
    assert!(manager.owed(&noto(), text).is_empty());
}

/// A codepoint the font does not have settles its range and is never asked for again.
///
/// The case that separates "not fetched" from "not in the font". Both look like a missing
/// glyph; only one is worth a request. Without the distinction, every label containing one
/// unusual character re-requests its whole range on every tile, forever, and succeeds every
/// time.
#[test]
fn a_codepoint_the_font_lacks_is_asked_for_once() {
    let origin = Origin::serving_ascii();
    let mut manager = manager();

    // In the served range, and genuinely not in this font's file: the fixture carries 191 of
    // the 256, and 0x80 is one of the ones it does not.
    let absent = 0x0080;
    for _ in 0..10 {
        manager.load(&noto(), [absent], &origin).expect("loads");
    }

    assert_eq!(origin.asked().len(), 1, "{:?}", origin.asked());
    assert!(
        manager.glyph(&noto(), absent).is_none(),
        "the font lacks it"
    );
    assert!(
        manager.is_resolved(&noto(), absent),
        "but its fate is known, so nothing should ask again"
    );
}

/// An origin with nothing for a range settles it too.
///
/// A 404 is an answer: this font does not serve those codepoints. Treating it as unknown would
/// spend a round trip on every tile to be told the same thing.
#[test]
fn an_empty_range_settles() {
    let origin = Origin::serving_ascii();
    let mut manager = manager();

    // 0x4e00 is CJK; the fixture origin answers 404 for it.
    for _ in 0..10 {
        manager.load(&noto(), [0x4e00], &origin).expect("loads");
    }

    assert_eq!(origin.asked().len(), 1, "{:?}", origin.asked());
    assert!(manager.is_resolved(&noto(), 0x4e00));
    assert!(manager.glyph(&noto(), 0x4e00).is_none());
}

/// A range that failed to fetch stays owed.
///
/// The other side of the line above. An empty answer is knowledge; a transport error is not,
/// and a manager that settled on one would give up on a font because the network blinked.
#[test]
fn a_failed_range_is_tried_again() {
    let origin = Origin {
        asked: RefCell::new(Vec::new()),
        answer: Box::new(|url: &str| {
            Err(FetchError::Transport {
                url: url.to_string(),
                message: "the link went away".to_string(),
            })
        }),
    };
    let mut manager = manager();

    for _ in 0..3 {
        let failed = manager.load(&noto(), [u32::from(b'A')], &origin);
        assert!(matches!(failed, Err(LoadError::Fetch { .. })), "{failed:?}");
    }

    assert_eq!(
        origin.asked().len(),
        3,
        "each attempt must reach the origin"
    );
    assert!(!manager.is_resolved(&noto(), u32::from(b'A')));
    assert_eq!(manager.owed(&noto(), [u32::from(b'A')]).len(), 1);
}

/// Two stacks are two sets of glyphs, and one does not answer for the other.
///
/// The stack is part of the URL, so `Noto Sans Regular` and `Noto Sans Bold` are different
/// files. A manager keyed on the codepoint alone would serve the bold 'A' where the regular one
/// was asked for — the right letter in the wrong weight, which nothing errors about.
#[test]
fn stacks_do_not_share_glyphs() {
    let origin = Origin::serving_ascii();
    let mut manager = manager();
    let regular = noto();
    let bold = FontStack::new(["Noto Sans Bold"]);

    manager
        .load(&regular, [u32::from(b'A')], &origin)
        .expect("loads");
    assert!(manager.glyph(&regular, u32::from(b'A')).is_some());
    assert!(
        manager.glyph(&bold, u32::from(b'A')).is_none(),
        "a different stack has not been loaded"
    );
    assert!(!manager.is_resolved(&bold, u32::from(b'A')));

    manager
        .load(&bold, [u32::from(b'A')], &origin)
        .expect("loads");
    assert_eq!(origin.asked().len(), 2, "one request per stack");
}

/// Order matters in a stack, because it is what the origin is asked for.
#[test]
fn stack_order_is_part_of_the_identity() {
    let manager = manager();
    let one = FontStack::new(["A", "B"]);
    let other = FontStack::new(["B", "A"]);
    assert_ne!(one, other);
    assert_ne!(
        manager.url_for(
            &one,
            Range {
                first: 0,
                last: 255
            }
        ),
        manager.url_for(
            &other,
            Range {
                first: 0,
                last: 255
            }
        )
    );
}

/// Codepoints above the BMP are not asked for as ranges.
///
/// There is no range file above 65535 — those are the local rasterizer's. Asking would build a
/// URL no origin answers, and spend a 404 per tile to find that out.
#[test]
fn codepoints_above_the_bmp_are_not_requested() {
    let origin = Origin::serving_ascii();
    let mut manager = manager();

    manager.load(&noto(), [0x1_f600], &origin).expect("loads");
    assert!(origin.asked().is_empty(), "{:?}", origin.asked());
    assert!(manager.owed(&noto(), [0x1_f600]).is_empty());
}

/// Eviction drops the stacks a style no longer names.
#[test]
fn eviction_keeps_only_what_is_named() {
    let origin = Origin::serving_ascii();
    let mut manager = manager();
    let regular = noto();
    let bold = FontStack::new(["Noto Sans Bold"]);

    manager
        .load(&regular, [u32::from(b'A')], &origin)
        .expect("loads");
    manager
        .load(&bold, [u32::from(b'A')], &origin)
        .expect("loads");
    assert!(!manager.is_empty());

    let keep: BTreeSet<FontStack> = [regular.clone()].into_iter().collect();
    manager.evict(&keep);

    assert!(manager.glyph(&regular, u32::from(b'A')).is_some());
    assert!(manager.glyph(&bold, u32::from(b'A')).is_none());
    assert!(
        !manager.is_resolved(&bold, u32::from(b'A')),
        "and it is owed again"
    );
}
