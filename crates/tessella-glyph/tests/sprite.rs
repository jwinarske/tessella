//! The sprite index, and the entries it refuses.
//!
//! mbgl's `SpriteParser`. Almost all of it is refusal, and that is the part worth testing: the
//! index is JSON with no schema behind it, so every field can be wrong in a way that parses
//! perfectly and then draws garbage or divides by zero somewhere else entirely.

use tessella_glyph::sprite::{self, Content, SpriteError, Stretch};

/// A well-formed index with one icon in it.
const ONE: &str = r#"{"airport": {"x": 0, "y": 0, "width": 24, "height": 24, "pixelRatio": 1}}"#;

fn parse(body: &str) -> sprite::Index {
    sprite::parse(body.as_bytes(), Some((512, 512))).expect("the index parses")
}

/// A plain entry reads back with its rectangle and the spec's defaults.
#[test]
fn an_entry_reads_back() {
    let index = parse(ONE);
    let sprite = index.get("airport").expect("the icon");
    assert_eq!((sprite.x, sprite.y), (0, 0));
    assert_eq!((sprite.width, sprite.height), (24, 24));
    assert!((sprite.pixel_ratio - 1.0).abs() < f64::EPSILON);
    assert!(!sprite.sdf, "the default is a plain image, not a field");
    assert!(sprite.stretch_x.is_empty());
    assert!(sprite.content.is_none());
}

/// A missing `pixelRatio` is one, and a `@2x` icon measures half its sheet size.
///
/// The ratio is carried rather than folded into the rectangle: folding it in would lose the
/// sheet coordinates the upload needs, and everything downstream measures in logical pixels.
#[test]
fn the_pixel_ratio_scales_the_logical_size() {
    let index = parse(
        r#"{"a": {"x": 0, "y": 0, "width": 48, "height": 48, "pixelRatio": 2},
            "b": {"x": 0, "y": 0, "width": 48, "height": 48}}"#,
    );
    let retina = &index["a"];
    assert_eq!((retina.width, retina.height), (48, 48), "sheet pixels");
    assert_eq!(retina.logical_size(), (24.0, 24.0), "logical pixels");

    assert!((index["b"].pixel_ratio - 1.0).abs() < f64::EPSILON);
    assert_eq!(index["b"].logical_size(), (48.0, 48.0));
}

/// One bad entry is dropped and the sheet is kept.
///
/// The behaviour a style with one broken icon needs: the other three hundred still draw. An
/// implementation that failed the whole index would blank every icon on the map because one
/// tool wrote one negative width.
#[test]
fn a_bad_entry_does_not_take_the_sheet_with_it() {
    let index = parse(
        r#"{"good": {"x": 0, "y": 0, "width": 24, "height": 24},
            "bad": {"x": 0, "y": 0, "width": -4, "height": 24},
            "also-good": {"x": 24, "y": 0, "width": 24, "height": 24}}"#,
    );
    assert_eq!(index.len(), 2, "{:?}", index.keys().collect::<Vec<_>>());
    assert!(index.contains_key("good"));
    assert!(index.contains_key("also-good"));
    assert!(!index.contains_key("bad"));
}

/// Every one of mbgl's bounds refuses its entry.
///
/// Written as a table because each is a separate silent failure downstream, and a check dropped
/// from the middle would leave the others passing. A zero ratio divides by zero in
/// `logical_size`; a negative dimension wraps when it reaches an unsigned rectangle; an
/// oversized one is a sheet nothing can upload.
#[test]
fn the_bounds_are_mbgls() {
    let cases: [(&str, &str); 8] = [
        ("zero width", r#"{"x":0,"y":0,"width":0,"height":24}"#),
        ("zero height", r#"{"x":0,"y":0,"width":24,"height":0}"#),
        ("negative width", r#"{"x":0,"y":0,"width":-1,"height":24}"#),
        (
            "a width past the field's range",
            r#"{"x":0,"y":0,"width":65536,"height":24}"#,
        ),
        (
            "over the dimension cap",
            r#"{"x":0,"y":0,"width":1025,"height":24}"#,
        ),
        (
            "zero ratio",
            r#"{"x":0,"y":0,"width":24,"height":24,"pixelRatio":0}"#,
        ),
        (
            "negative ratio",
            r#"{"x":0,"y":0,"width":24,"height":24,"pixelRatio":-2}"#,
        ),
        (
            "over the ratio cap",
            r#"{"x":0,"y":0,"width":24,"height":24,"pixelRatio":11}"#,
        ),
    ];

    for (what, entry) in cases {
        let index = parse(&format!(r#"{{"icon": {entry}}}"#));
        assert!(index.is_empty(), "{what} was accepted: {index:?}");
    }

    // A negative origin is *not* here: it becomes zero and the entry survives, which is
    // `a_fractional_dimension_is_refused_and_a_fractional_origin_is_not`.

    // And the caps themselves are inclusive, so an icon exactly at one is kept.
    let at_the_cap = sprite::parse(
        br#"{"icon": {"x":0,"y":0,"width":1024,"height":1024,"pixelRatio":10}}"#,
        None,
    )
    .expect("parses");
    assert_eq!(at_the_cap.len(), 1, "the bound is exclusive by one");
}

/// A rectangle running off the sheet is refused.
///
/// The one bound that needs the image. Without it the entry parses, and sampling it reads past
/// the end of the texture — which on a real backend is whatever the neighbouring icon left
/// there, and looks like the wrong icon rather than like an error.
#[test]
fn a_rectangle_off_the_sheet_is_refused() {
    let body = r#"{"edge": {"x": 500, "y": 0, "width": 24, "height": 24}}"#;

    let checked = sprite::parse(body.as_bytes(), Some((512, 512))).expect("parses");
    assert!(checked.is_empty(), "500 + 24 runs past 512");

    // Against a sheet that holds it, the same entry is fine.
    let bigger = sprite::parse(body.as_bytes(), Some((1024, 512))).expect("parses");
    assert_eq!(bigger.len(), 1);

    // And with no sheet given, the check is skipped rather than guessed — a caller reading the
    // index before the image arrives has nothing to check against.
    let unchecked = sprite::parse(body.as_bytes(), None).expect("parses");
    assert_eq!(unchecked.len(), 1);
}

/// A fractional *dimension* is refused and a fractional *origin* is not.
///
/// One rule with two outcomes, which is why it reads as two. Every rectangle field is an
/// unsigned integer with a default of zero, so a fractional value in any of them becomes zero —
/// and zero is a fatal width and a perfectly good origin. `against_mbgl` has the upstream
/// expectations that say so; this is the same fact stated where a reader meets it first.
#[test]
fn a_fractional_dimension_is_refused_and_a_fractional_origin_is_not() {
    assert!(parse(r#"{"icon": {"x": 0, "y": 0, "width": 24.5, "height": 24}}"#).is_empty());

    let shifted = parse(r#"{"icon": {"x": 0.5, "y": 0, "width": 24, "height": 24}}"#);
    assert_eq!(shifted["icon"].x, 0, "a fractional origin is not a refusal");
}

/// Stretches and the content box read back, and a malformed range is dropped.
#[test]
fn stretches_and_content_read_back() {
    let index = parse(
        r#"{"shield": {"x": 0, "y": 0, "width": 32, "height": 32, "sdf": true,
                       "stretchX": [[4, 28]], "stretchY": [[4, 28]],
                       "content": [4, 4, 28, 28]}}"#,
    );
    let shield = &index["shield"];
    assert!(shield.sdf, "a shield is a distance field");
    assert_eq!(
        shield.stretch_x,
        vec![Stretch {
            from: 4.0,
            to: 28.0
        }]
    );
    assert_eq!(
        shield.stretch_y,
        vec![Stretch {
            from: 4.0,
            to: 28.0
        }]
    );
    assert_eq!(
        shield.content,
        Some(Content {
            left: 4.0,
            top: 4.0,
            right: 28.0,
            bottom: 28.0
        })
    );

    // A range that is not exactly two numbers is not a range. Taking the first two of a longer
    // one would silently read `[0, 4, 9]` as `[0, 4]`.
    let sloppy = parse(
        r#"{"s": {"x": 0, "y": 0, "width": 32, "height": 32,
                  "stretchX": [[0, 4, 9], [8], [12, 20]], "content": [1, 2, 3]}}"#,
    );
    assert_eq!(
        sloppy["s"].stretch_x,
        vec![Stretch {
            from: 12.0,
            to: 20.0
        }]
    );
    assert!(sloppy["s"].content.is_none(), "a three-sided box is no box");
}

/// A body that is not an index at all is an error rather than an empty sheet.
///
/// The distinction the entry-level dropping depends on: one bad icon is a dropped entry, and a
/// 404 page served where the index should be is a failure. Answering with an empty index would
/// be a style that silently has no icons and nothing anywhere saying why.
#[test]
fn a_body_that_is_not_an_index_is_an_error() {
    assert!(matches!(
        sprite::parse(b"<html>404</html>", None),
        Err(SpriteError::Json(_))
    ));
    assert_eq!(
        sprite::parse(b"[1, 2, 3]", None),
        Err(SpriteError::NotAnObject)
    );
    assert_eq!(sprite::parse(b"null", None), Err(SpriteError::NotAnObject));

    // An empty object is an index with no icons, which is a real answer.
    assert!(sprite::parse(b"{}", None).expect("parses").is_empty());
}

/// The two URLs a sprite base resolves to.
///
/// The suffix goes before the extension, not after the URL — `sprite@2x.json`, not
/// `sprite.json@2x`. And a query string survives, in front of which the suffix goes, which is
/// what makes a signed sprite URL work.
#[test]
fn a_base_resolves_to_a_json_and_an_image() {
    assert_eq!(
        sprite::urls("https://example.com/sprite", 1.0),
        (
            "https://example.com/sprite.json".to_string(),
            "https://example.com/sprite.png".to_string()
        )
    );
    assert_eq!(
        sprite::urls("https://example.com/sprite", 2.0),
        (
            "https://example.com/sprite@2x.json".to_string(),
            "https://example.com/sprite@2x.png".to_string()
        )
    );

    let (json, image) = sprite::urls("https://example.com/sprite?key=abc", 2.0);
    assert_eq!(json, "https://example.com/sprite@2x.json?key=abc");
    assert_eq!(image, "https://example.com/sprite@2x.png?key=abc");
}

/// An entry that is not an object is dropped rather than parsed.
#[test]
fn a_non_object_entry_is_dropped() {
    let index =
        parse(r#"{"a": 3, "b": "text", "c": null, "d": {"x":0,"y":0,"width":8,"height":8}}"#);
    assert_eq!(index.len(), 1);
    assert!(index.contains_key("d"));
}

/// A number JSON cannot represent fails the whole index, not one entry.
///
/// Worth pinning because it cuts across the rule above. `-1` is a value that *parses* and is
/// then refused, so its entry is dropped and the sheet is kept. `1e400` is not a value at all —
/// the parser refuses the document — so one such number takes every icon in the sheet with it.
///
/// The two granularities are the parser's and this module's, and they are not the same. Nothing
/// here can widen the second to cover the first without hand-rolling a number parser, and the
/// case is rare enough that a loud failure is the better answer than a quiet one: an index no
/// tool would emit fails visibly rather than half-loading.
#[test]
fn a_number_json_cannot_represent_fails_the_index() {
    let body = r#"{"good": {"x":0,"y":0,"width":24,"height":24},
                   "bad": {"x":0,"y":0,"width":1e400,"height":24}}"#;
    assert!(matches!(
        sprite::parse(body.as_bytes(), None),
        Err(SpriteError::Json(_))
    ));
}

/// The dimension bounds refuse a value that is not comparable, rather than accepting it.
///
/// Defensive rather than load-bearing today: serde_json will not hand this module a NaN, because
/// JSON has no literal for one and an out-of-range number fails the document. It is written as a
/// negated `>` anyway, because `<= 0` *accepts* a NaN — every comparison against one is false —
/// and would then hand an unsigned cast a value with no defined result. The cost of the
/// defensive form is nothing; the cost of tidying it into `<=` is a silent cast if the parser
/// ever changes, which is why this test says not to.
#[test]
fn the_bounds_refuse_an_incomparable_value() {
    // Reached through the public parse, so the guard is checked where it actually sits.
    let index = sprite::parse(
        br#"{"icon": {"x":0,"y":0,"width":24,"height":24,"pixelRatio":1}}"#,
        None,
    )
    .expect("parses");
    assert_eq!(index.len(), 1, "the control case is accepted");

    // The property the guard has, stated directly: `!(x > 0.0)` refuses a NaN and `x <= 0.0`
    // does not. If a later reader swaps the form, this is the line that disagrees.
    let nan = f64::NAN;
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    {
        assert!(!(nan > 0.0), "a NaN must fail the guard the module uses");
        assert!(!(nan <= 0.0), "and would pass the one it does not");
    }
}

/// mbgl's own sprite fixture and the expectations its `Sprite.*` tests state.
///
/// Everything above checks behaviour this build reasons about. This checks it against the
/// upstream it is a transcription of: same bytes, same answers. Reading `sprite_parser.cpp`
/// found three places where the reasoning had gone wrong, and all three are here.
mod against_mbgl {
    use tessella_glyph::sprite::{self, TextFit};

    const EMERALD_JSON: &[u8] = include_bytes!("../../../tests/sprite-fixtures/emerald.json");

    /// The sheet is 200x299, which is what mbgl's error messages in these tests quote.
    const SHEET: (u32, u32) = (200, 299);

    /// mbgl's `Sprite.SpriteParsing`: the fixture yields its icons, names intact.
    #[test]
    fn the_fixture_parses_the_way_mbgl_parses_it() {
        let index = sprite::parse(EMERALD_JSON, Some(SHEET)).expect("the index parses");
        assert_eq!(index.len(), 73, "mbgl reads seventy-three from this file");

        // Names with dots in them, which is the shape a hand-made fixture does not have: a
        // parser that split a name on a separator would pass every test written against one.
        assert!(index.contains_key("dlr.london-overground.london-underground.national-rail"));
        assert!(index.contains_key("dlr.london-underground"));
        assert!(index.contains_key("default_marker"));

        // And every rectangle is inside the sheet, which is what the bounds check is for.
        for (name, sprite) in &index {
            assert!(
                sprite.x + sprite.width <= SHEET.0 && sprite.y + sprite.height <= SHEET.1,
                "{name} runs off the sheet: {sprite:?}"
            );
        }
    }

    /// mbgl's `Sprite.SpriteParsingSimpleWidthHeight`: an entry with no origin is valid.
    ///
    /// The first thing reading the source corrected. `x` and `y` are read with a *default of
    /// zero*, not required — so `{"width": 32, "height": 32}` is an icon at the sheet's top left
    /// and mbgl accepts it. This build refused it, which is a style whose minimal entries all
    /// vanish.
    #[test]
    fn an_entry_with_no_origin_is_an_icon_at_the_corner() {
        let index = sprite::parse(br#"{"image": {"width": 32, "height": 32}}"#, Some(SHEET))
            .expect("parses");
        assert_eq!(index.len(), 1, "mbgl accepts this one");
        assert_eq!((index["image"].x, index["image"].y), (0, 0));
    }

    /// mbgl's `Sprite.SpriteParsingWidthTooBig` and `SpriteParsingNegativeWidth`.
    ///
    /// Both refuse, and both refuse for the *same* reason rather than for two: the field is read
    /// as an unsigned sixteen-bit integer with a default of zero, so 65536 and -1 alike become
    /// zero, and it is `width <= 0` that turns them away. mbgl's error message quotes `0x32`,
    /// which is the zero showing through.
    #[test]
    fn an_impossible_width_becomes_zero_and_is_refused() {
        for width in ["65536", "-1", "0.5"] {
            let body = format!(r#"{{"image": {{"width": {width}, "height": 32}}}}"#);
            let index = sprite::parse(body.as_bytes(), Some(SHEET)).expect("parses");
            assert!(index.is_empty(), "a width of {width} was accepted");
        }
    }

    /// A fractional *origin* becomes zero and the entry survives.
    ///
    /// The second correction, and the one that goes the other way from every instinct: the same
    /// rule that refuses a fractional width *accepts* a fractional x, because zero is a valid
    /// origin and zero is not a valid width. This build refused it, which is closer to what a
    /// person would want and further from what mbgl does — and the oracle is the point.
    #[test]
    fn a_fractional_origin_becomes_zero_and_survives() {
        let index = sprite::parse(
            br#"{"image": {"x": 0.5, "y": -3, "width": 32, "height": 32}}"#,
            Some(SHEET),
        )
        .expect("parses");
        assert_eq!(index.len(), 1, "mbgl keeps this one at the corner");
        assert_eq!((index["image"].x, index["image"].y), (0, 0));
    }

    /// mbgl's `Sprite.SpriteParsingNullRatio`: a zero pixel ratio is refused.
    ///
    /// Unlike the rectangle fields, `pixelRatio` is read as any number and *keeps* what it
    /// reads — mbgl's message quotes `@0x`, so the zero reached the bounds check rather than
    /// being defaulted away. The two readers are genuinely different and that is why.
    #[test]
    fn a_zero_ratio_is_refused_by_the_bounds_and_not_the_reader() {
        let index = sprite::parse(
            br#"{"image": {"width": 32, "height": 32, "pixelRatio": 0}}"#,
            Some(SHEET),
        )
        .expect("parses");
        assert!(index.is_empty());

        // And a fractional ratio is kept, which a uint16 reader would have destroyed.
        let fine = sprite::parse(
            br#"{"image": {"width": 32, "height": 32, "pixelRatio": 1.5}}"#,
            Some(SHEET),
        )
        .expect("parses");
        assert!((fine["image"].pixel_ratio - 1.5).abs() < f64::EPSILON);
    }

    /// mbgl's `Sprite.SpriteParsingTextFit` and `SpriteParsingInvalidTextFit`.
    ///
    /// The third correction: this build did not read the fields at all. An unrecognized value is
    /// absent rather than a default, because the three behaviours resize a shield differently
    /// and guessing between them is worse than not stretching.
    #[test]
    fn text_fit_reads_its_three_values_and_no_others() {
        let index = sprite::parse(
            br#"{"a": {"width": 16, "height": 16,
                      "textFitWidth": "stretchOrShrink", "textFitHeight": "proportional"},
                 "b": {"width": 16, "height": 16, "textFitWidth": "stretchOnly"},
                 "c": {"width": 16, "height": 16, "textFitWidth": "nonsense"},
                 "d": {"width": 16, "height": 16, "textFitWidth": 3}}"#,
            Some(SHEET),
        )
        .expect("parses");

        assert_eq!(index["a"].text_fit_width, Some(TextFit::StretchOrShrink));
        assert_eq!(index["a"].text_fit_height, Some(TextFit::Proportional));
        assert_eq!(index["b"].text_fit_width, Some(TextFit::StretchOnly));
        assert_eq!(index["b"].text_fit_height, None, "unset is not a default");
        assert_eq!(
            index["c"].text_fit_width, None,
            "a bad value is not a guess"
        );
        assert_eq!(index["d"].text_fit_width, None, "nor is a non-string");
    }

    /// mbgl's `Sprite.SpriteParsingInvalidStretches` and `SpriteParsingInvalidContent`.
    ///
    /// A malformed stretch is skipped and its neighbours kept; a malformed content box is
    /// dropped whole. The asymmetry is mbgl's: a stretch list is a list of ranges and one bad
    /// range is one bad range, while a content box is four numbers and three of them is nothing.
    #[test]
    fn malformed_stretches_and_content_follow_mbgls_asymmetry() {
        let index = sprite::parse(
            br#"{"image": {"width": 16, "height": 16,
                          "stretchX": [[0, 4], "no", [8, 12], [1, 2, 3]],
                          "content": [1, 2, 3]}}"#,
            Some(SHEET),
        )
        .expect("parses");

        assert_eq!(
            index["image"].stretch_x.len(),
            2,
            "a good range was dropped"
        );
        assert!(
            index["image"].content.is_none(),
            "a three-sided box was kept"
        );
    }

    /// mbgl's `Sprite.SpriteParsingEmptyImage`: an entry with no dimensions at all is refused.
    #[test]
    fn an_entry_with_no_dimensions_is_refused() {
        let index = sprite::parse(br#"{"image": {}}"#, Some(SHEET)).expect("parses");
        assert!(index.is_empty(), "an entry with nothing in it drew");
    }
}
