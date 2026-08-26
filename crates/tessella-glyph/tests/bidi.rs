//! Laying a line out in the order it is *read* rather than the order it is stored.
//!
//! The Unicode bidirectional algorithm, UAX #9. Text arrives in logical order — which for Hebrew
//! and Arabic is right to left — and a shaper that positioned it as stored draws every such label
//! backwards. That is not a missing feature but a wrong answer: every letter is correct and the
//! word is not, which is the kind of defect a reader who does not know the script cannot see.
//!
//! mbgl reaches the same place through ICU's `ubidi_setLine` and `ubidi_getVisualRun`, and its
//! `BiDi.Reverse*` tests are the shape of what is checked here.
//!
//! Arabic *shaping* — the contextual letter forms, mbgl's `applyArabicShaping` — is a separate
//! step and is not ported. Without it Arabic reorders correctly and each letter is drawn in its
//! isolated form rather than joined to its neighbours.

use tessella_glyph::shaping::{Char, reorder};

/// A line of characters, each one unit wide, in logical order.
fn line(text: &str) -> Vec<Char> {
    text.chars()
        .map(|character| Char::new(character as u32, 1.0))
        .collect()
}

/// What a reordered line reads as, left to right on the screen.
fn displayed(text: &str) -> String {
    reorder(&line(text))
        .iter()
        .filter_map(|character| char::from_u32(character.codepoint))
        .collect()
}

/// Latin text is untouched.
///
/// Most labels on most maps, and the case where the algorithm's answer is the identity. Asserted
/// because the fast path returns the input borrowed rather than running the algorithm at all, and
/// a fast path that was subtly not the identity would be worse than no fast path.
#[test]
fn left_to_right_text_is_unchanged() {
    for text in ["Main Street", "A1", "Kraków", "東京", "", " "] {
        assert_eq!(displayed(text), text, "{text:?} moved");
    }
}

/// Hebrew is laid out right to left.
#[test]
fn hebrew_is_reversed() {
    // "שלום" — shin, lamed, vav, final mem, in the order it is stored.
    let stored = "\u{5e9}\u{5dc}\u{5d5}\u{5dd}";
    let shown: Vec<char> = displayed(stored).chars().collect();
    let logical: Vec<char> = stored.chars().collect();

    assert_eq!(
        shown,
        logical.iter().rev().copied().collect::<Vec<char>>(),
        "a right-to-left word was drawn in storage order"
    );
}

/// An embedded Latin word keeps its own direction.
///
/// The assertion that separates a *run-based* reorder from reversing the whole line. Reversing
/// the line puts the Latin word backwards too — which looks nearly right, and is the failure a
/// simpler implementation makes.
#[test]
fn an_embedded_latin_word_is_not_reversed() {
    // Hebrew, space, "Main", space, Hebrew.
    let stored = "\u{5e9}\u{5dc} Main \u{5d5}\u{5dd}";
    let shown = displayed(stored);

    assert!(
        shown.contains("Main"),
        "the embedded word was reversed: {shown:?}"
    );
    assert!(
        !shown.contains("niaM"),
        "the whole line was reversed rather than its runs: {shown:?}"
    );
}

/// A number inside right-to-left text reads left to right.
///
/// mbgl's `BiDi.ReverseArabic` is this case: `سلام۳۹` comes back with the digits still in their
/// own order at the *start* of the display line. Digits are their own bidi class, so an
/// implementation that treated everything non-Latin as right-to-left reverses them.
#[test]
fn digits_inside_right_to_left_text_keep_their_order() {
    // Arabic "سلام" followed by the Eastern Arabic digits ۳۹.
    let stored = "\u{633}\u{644}\u{627}\u{645}\u{6f3}\u{6f9}";
    let shown = displayed(stored);

    assert!(
        shown.starts_with("\u{6f3}\u{6f9}"),
        "the digits did not lead, or were reversed: {shown:?}"
    );
    // And the letters that follow them are in reverse of storage order.
    assert!(
        shown.ends_with("\u{633}"),
        "the first letter stored is not the last shown: {shown:?}"
    );
}

/// Mixed script keeps each run's direction and orders the runs by the paragraph's.
///
/// mbgl's `BiDi.ReverseMixed`: an Arabic phrase followed by a Latin one comes back with the Latin
/// *first*, because the paragraph is right-to-left and the Latin run is the last of it.
#[test]
fn mixed_script_orders_runs_by_the_paragraphs_direction() {
    // "مكتبة Maktabat"
    let stored = "\u{645}\u{643}\u{62a}\u{628}\u{629} Maktabat";
    let shown = displayed(stored);

    assert!(
        shown.starts_with(" Maktabat") || shown.starts_with("Maktabat"),
        "the Latin run did not lead a right-to-left paragraph: {shown:?}"
    );
    assert!(shown.contains("Maktabat"), "{shown:?}");
}

/// Reordering carries each character's advance with it.
///
/// The trap in reordering a *shaped* line rather than a string: the widths belong to the
/// characters, so a reorder that moved codepoints and left the advances behind would set every
/// right-to-left label with its letters spaced by their neighbours' widths.
#[test]
fn the_advances_travel_with_their_characters() {
    let stored = [
        Char::new('\u{5e9}' as u32, 3.0),
        Char::new('\u{5dc}' as u32, 7.0),
        Char::new('\u{5d5}' as u32, 11.0),
    ];
    let shown = reorder(&stored);

    let pairs: Vec<(u32, f32)> = shown
        .iter()
        .map(|character| (character.codepoint, character.advance))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ('\u{5d5}' as u32, 11.0),
            ('\u{5dc}' as u32, 7.0),
            ('\u{5e9}' as u32, 3.0),
        ]
    );
}

/// A line the algorithm leaves alone is not copied.
///
/// Most lines on most maps are Latin, and shaping runs per label per tile. The borrow is the
/// difference between reordering being free for them and costing an allocation each.
#[test]
fn a_left_to_right_line_is_borrowed_rather_than_rebuilt() {
    let latin = line("Main Street");
    assert!(matches!(reorder(&latin), std::borrow::Cow::Borrowed(_)));

    let hebrew = line("\u{5e9}\u{5dc}");
    assert!(matches!(reorder(&hebrew), std::borrow::Cow::Owned(_)));
}
