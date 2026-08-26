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

/// Arabic contextual shaping — mbgl's `applyArabicShaping`, against its own expected strings.
///
/// Arabic is written joined, and which of a letter's four shapes is drawn depends on whether the
/// letters either side join to it. Text is *stored* as unjoined base letters, so a renderer that
/// drew them as stored produces something a reader can decipher and no reader would call written
/// Arabic.
///
/// mbgl's test file says the expected strings "may appear to be backwards ... because whatever
/// you're viewing the text with is applying the bidirectional algorithm a second time", and that
/// they are presentation forms. They are written here as escapes for that reason: the bytes are
/// the assertion, and a copied glyph is not.
mod arabic {
    use tessella_glyph::arabic::shape;

    fn shaped(text: &str) -> Vec<u32> {
        let codepoints: Vec<u32> = text.chars().map(|character| character as u32).collect();
        shape(&codepoints)
    }

    fn expect(text: &str, forms: &[u32]) {
        assert_eq!(
            shaped(text),
            forms.to_vec(),
            "shaping {:?}",
            text.escape_unicode().to_string()
        );
    }

    /// mbgl `BiDi.ArabicShaping`: "اليمن" — alef, lam, yeh, meem, noon.
    ///
    /// Every joined form in one word. The alef is right-joining so it stays isolated and the lam
    /// after it takes an *initial* rather than medial form — the case that separates a shaper
    /// which reads joining types from one that joins everything to everything.
    #[test]
    fn a_word_takes_its_joined_forms() {
        expect(
            "\u{627}\u{644}\u{64a}\u{645}\u{646}",
            &[0xFE8D, 0xFEDF, 0xFEF4, 0xFEE4, 0xFEE6],
        );
    }

    /// mbgl `BiDi.Tashkeel`: "سلام۳۹" ends in a lam-alef ligature and two digits.
    ///
    /// Two letters become one code point, so the output is shorter than the input. The digits
    /// are not Arabic letters and pass through untouched.
    #[test]
    fn lam_alef_becomes_one_ligature() {
        let out = shaped("\u{633}\u{644}\u{627}\u{645}\u{6f3}\u{6f9}");
        assert_eq!(out.len(), 5, "the ligature did not consume the alef");
        // Seen initial, the lam-alef ligature in its *final* form because the seen joins forward
        // into it, then meem *isolated* — the alef is right-joining, so the ligature does not
        // join forward and the meem after it starts alone.
        assert_eq!(out, vec![0xFEB3, 0xFEFC, 0xFEE1, 0x06F3, 0x06F9]);
    }

    /// mbgl `BiDi.MixedShaping`: Latin beside Arabic is left alone.
    #[test]
    fn latin_passes_through_untouched() {
        let out = shaped("\u{645}\u{643} Maktabat");
        assert_eq!(
            &out[2..],
            &[0x20, 0x4D, 0x61, 0x6B, 0x74, 0x61, 0x62, 0x61, 0x74]
        );
        // And the Arabic before it still joined: meem initial, kaf final.
        assert_eq!(&out[..2], &[0xFEE3, 0xFEDA]);
    }

    /// A diacritic does not break the join around it.
    ///
    /// Transparent joining. A letter's context has to look *past* any number of marks, and a
    /// shaper that stopped at the first one unjoins every voweled word — which is most of a
    /// Qur'anic text and none of a road sign, so it survives casual testing.
    #[test]
    fn a_diacritic_does_not_break_a_join() {
        // Beh, fatha, beh: the marks sits between two letters that must still join.
        let bare = shaped("\u{628}\u{628}");
        let voweled = shaped("\u{628}\u{64e}\u{628}");

        // Initial then final: a medial form needs a join on *both* sides, and the second beh has
        // nothing after it.
        assert_eq!(bare, vec![0xFE91, 0xFE90]);
        assert_eq!(
            voweled,
            vec![0xFE91, 0x064E, 0xFE90],
            "the mark unjoined the word"
        );
    }

    /// A right-joining letter never takes an initial or medial form.
    ///
    /// Alef, dal, reh and their kin join only backwards. The table repeats their isolated and
    /// final forms so the lookup stays an index, and this is what says the repeat is not reached
    /// by accident.
    #[test]
    fn a_right_joining_letter_stays_open() {
        // Beh, dal, beh. The dal takes a final form and the beh after it starts a new run, so it
        // is *initial* rather than medial.
        assert_eq!(
            shaped("\u{628}\u{62f}\u{628}"),
            vec![0xFE91, 0xFEAA, 0xFE8F],
            "a right-joining letter joined forwards"
        );
    }

    /// A lone letter is isolated, and non-Arabic is never touched.
    #[test]
    fn the_degenerate_cases_hold() {
        assert_eq!(shaped("\u{628}"), vec![0xFE8F], "a lone beh is isolated");
        assert_eq!(shaped(""), Vec::<u32>::new());
        assert_eq!(
            shaped("Main Street"),
            "Main Street".chars().map(|c| c as u32).collect::<Vec<_>>()
        );
        assert_eq!(
            shaped("\u{5e9}\u{5dc}"),
            vec![0x5E9, 0x5DC],
            "Hebrew is not Arabic"
        );
    }
}
