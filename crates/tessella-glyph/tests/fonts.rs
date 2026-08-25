//! The store between the two phases: fetch the ranges a layout declared, pack what it asked for.
//!
//! The manager decides what to ask the origin and the atlas decides where a glyph sits; neither
//! is something a bucket builder can shape against. What this checks is the join, and mostly the
//! part that is about *not* doing work: a second tile needing the same letters must cost nothing.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use tessella_glyph::Glyphs;
use tessella_glyph::fonts::{Dependencies, Fonts};
use tessella_glyph::manager::FontStack;
use tessella_storage::source::{FetchError, FileSource, Response};

const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

struct Origin {
    asked: RefCell<Vec<String>>,
    /// Answer every request with a transport error instead.
    broken: bool,
}

impl Origin {
    fn new() -> Self {
        Self {
            asked: RefCell::new(Vec::new()),
            broken: false,
        }
    }

    fn broken() -> Self {
        Self {
            asked: RefCell::new(Vec::new()),
            broken: true,
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl FileSource for Origin {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.asked.borrow_mut().push(url.to_string());
        if self.broken {
            return Err(FetchError::Transport {
                url: url.to_string(),
                message: "the link is down".to_string(),
            });
        }
        // One font, one range. Anything else answers 200 with an empty body, which is an origin
        // saying it has nothing there rather than a failure.
        let body = if url.contains("TestFont") && url.contains("0-255") {
            GLYPHS.to_vec()
        } else {
            Vec::new()
        };
        Ok(Response {
            status: 200,
            body,
            ..Response::default()
        })
    }
}

// `FileSource` is `Send + Sync`; the `RefCell` here is single-threaded test bookkeeping.
unsafe impl Sync for Origin {}
unsafe impl Send for Origin {}

fn store() -> Fonts {
    Fonts::new("https://example.com/fonts/{fontstack}/{range}.pbf")
}

fn wants(text: &str) -> Dependencies {
    let mut out: Dependencies = BTreeMap::new();
    out.insert(
        vec!["TestFont".to_string()],
        text.chars().map(|character| character as u32).collect(),
    );
    out
}

/// A fetched stack shapes: metrics from the range, rectangles from the atlas.
#[test]
fn a_fetched_stack_answers_both_questions() {
    let mut fonts = store();
    let origin = Origin::new();
    let fetched = fonts
        .fetch(&wants("Main Street"), &origin)
        .expect("the origin answers");

    assert_eq!(fetched, 1, "one range covers ASCII");
    assert_eq!(origin.asked().len(), 1);

    let glyphs = fonts.stack(&["TestFont".to_string()]);
    for character in "MainStret".chars() {
        let codepoint = character as u32;
        assert!(
            glyphs.metrics(codepoint).is_some(),
            "{character:?} has no metrics"
        );
        assert!(
            glyphs.rect(codepoint).is_some(),
            "{character:?} was not packed"
        );
    }
}

/// A space has an advance and is not packed.
///
/// Both halves matter. Dropping the advance sets the words run together; packing a zero-area
/// rectangle takes a shelf slot and hands the shaper a rectangle to draw, which is a blank quad
/// per space on every label of the map.
#[test]
fn a_space_advances_without_a_rectangle() {
    let mut fonts = store();
    fonts
        .fetch(&wants("a b"), &Origin::new())
        .expect("the origin answers");

    let glyphs = fonts.stack(&["TestFont".to_string()]);
    let space = u32::from(b' ');
    let (metrics, has_bitmap) = glyphs.metrics(space).expect("a space has metrics");
    assert!(metrics.advance > 0);
    assert!(!has_bitmap);
    assert!(glyphs.rect(space).is_none(), "a space was packed");
}

/// Only what was asked for is packed.
///
/// A range is 256 codepoints and a label uses a handful. Packing the whole range fills the atlas
/// with glyphs nothing draws, and evicts the ones that are drawn.
#[test]
fn only_the_requested_codepoints_are_packed() {
    let mut fonts = store();
    fonts
        .fetch(&wants("abc"), &Origin::new())
        .expect("the origin answers");

    let atlas = fonts.atlas(&["TestFont".to_string()]).expect("an atlas");
    assert_eq!(atlas.len(), 3, "the whole range was packed");

    // The metrics are there for the rest of the range, because the range was parsed. That is the
    // distinction the store rests on: parsed is not the same as packed.
    let glyphs = fonts.stack(&["TestFont".to_string()]);
    assert!(glyphs.metrics(u32::from(b'z')).is_some());
    assert!(glyphs.rect(u32::from(b'z')).is_none());
}

/// A second tile wanting the same letters costs nothing.
///
/// The whole point of the store. Without it every tile re-requests its ranges, and the map works
/// perfectly while spending a round trip per tile forever.
#[test]
fn a_second_tile_asks_for_nothing() {
    let mut fonts = store();
    let origin = Origin::new();
    fonts.fetch(&wants("abc"), &origin).expect("answers");
    let after_first = origin.asked().len();

    let fetched = fonts.fetch(&wants("cba"), &origin).expect("answers");
    assert_eq!(fetched, 0, "the same letters were fetched again");
    assert_eq!(origin.asked().len(), after_first);
}

/// A letter in a range already held is packed without a request.
#[test]
fn a_new_letter_in_a_held_range_needs_no_request() {
    let mut fonts = store();
    let origin = Origin::new();
    fonts.fetch(&wants("abc"), &origin).expect("answers");

    let fetched = fonts.fetch(&wants("xyz"), &origin).expect("answers");
    assert_eq!(fetched, 0, "the range was already held");
    assert_eq!(origin.asked().len(), 1);

    let glyphs = fonts.stack(&["TestFont".to_string()]);
    assert!(glyphs.rect(u32::from(b'x')).is_some(), "x was not packed");
}

/// A codepoint the font does not have is resolved, not owed.
///
/// "Resolved" has to include *known absent*, or a label with one unusual character keeps a tile
/// provisional forever and re-requests its range on every frame.
#[test]
fn an_absent_codepoint_is_resolved() {
    let mut fonts = store();
    let origin = Origin::new();
    // Cyrillic: in no range this origin serves, and outside the one it does.
    let wanted = wants("aд");
    fonts.fetch(&wanted, &origin).expect("answers");

    assert_eq!(origin.asked().len(), 2, "two ranges, one per block");
    assert!(
        fonts.is_resolved(&wanted),
        "an absent glyph left the tile unresolved"
    );

    let glyphs = fonts.stack(&["TestFont".to_string()]);
    assert!(glyphs.metrics('д' as u32).is_none());
    assert!(glyphs.rect('д' as u32).is_none());

    // And asking again spends nothing: the empty answer settled both ranges.
    assert_eq!(fonts.fetch(&wanted, &origin).expect("answers"), 0);
    assert_eq!(origin.asked().len(), 2);
}

/// A transport error is not an answer, and is reported.
///
/// A font stack that never loads draws a map with no labels on it, and nothing else in the frame
/// would say so — so this fails loudly rather than returning an empty store.
#[test]
fn a_broken_origin_is_an_error_and_is_retried() {
    let mut fonts = store();
    let broken = Origin::broken();
    let wanted = wants("abc");

    assert!(fonts.fetch(&wanted, &broken).is_err());
    assert!(!fonts.is_resolved(&wanted), "a failure settled the range");

    // The range stays owed, so a working origin later still gets asked.
    let working = Origin::new();
    assert_eq!(fonts.fetch(&wanted, &working).expect("answers"), 1);
    assert!(fonts.is_resolved(&wanted));
}

/// Two font stacks get two atlases.
///
/// A rectangle is a position in a texture, so the same codepoint in two fonts is two rectangles.
/// One atlas per style would have the second stack read the first's pixels.
#[test]
fn two_stacks_get_two_atlases() {
    let mut fonts = store();
    let mut wanted: Dependencies = BTreeMap::new();
    let letters: BTreeSet<u32> = "abc".chars().map(|character| character as u32).collect();
    wanted.insert(vec!["TestFont".to_string()], letters.clone());
    wanted.insert(vec!["OtherFont".to_string()], letters);

    fonts.fetch(&wanted, &Origin::new()).expect("answers");

    assert!(fonts.atlas(&["TestFont".to_string()]).is_some());
    let other = fonts
        .atlas(&["OtherFont".to_string()])
        .expect("the second stack has an atlas of its own");
    assert!(
        other.is_empty(),
        "the origin serves nothing for the second stack, so it packed {} glyphs",
        other.len()
    );
}

/// Eviction drops a stack's glyphs and its atlas together.
#[test]
fn eviction_takes_the_atlas_with_the_glyphs() {
    let mut fonts = store();
    fonts.fetch(&wants("abc"), &Origin::new()).expect("answers");
    assert!(fonts.atlas(&["TestFont".to_string()]).is_some());

    fonts.evict(&BTreeSet::new());
    assert!(
        fonts.atlas(&["TestFont".to_string()]).is_none(),
        "the atlas outlived the stack that owns it"
    );

    // Keeping the stack keeps both.
    fonts.fetch(&wants("abc"), &Origin::new()).expect("answers");
    let mut keep = BTreeSet::new();
    keep.insert(FontStack::new(["TestFont"]));
    fonts.evict(&keep);
    assert!(fonts.atlas(&["TestFont".to_string()]).is_some());
}

/// Newly packed glyphs are reported as damage, once.
#[test]
fn packing_dirties_the_atlas_once() {
    let mut fonts = store();
    let stack = ["TestFont".to_string()];
    fonts.fetch(&wants("abc"), &Origin::new()).expect("answers");

    assert!(
        !fonts.take_dirty(&stack).is_empty(),
        "packing three glyphs dirtied nothing"
    );
    assert!(
        fonts.take_dirty(&stack).is_empty(),
        "the same rectangles were reported twice"
    );

    // And packing more dirties again.
    fonts.fetch(&wants("xyz"), &Origin::new()).expect("answers");
    assert!(!fonts.take_dirty(&stack).is_empty());
}
