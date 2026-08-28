//! The orderings the collator exists for.
//!
//! # Why these and not "nearby strings sort together"
//!
//! Because the whole point is the three pairs codepoint order gets wrong. `a` sorts before `A`
//! where 0x41 < 0x61, and both sort before `ä` where 0xE4 is above either. A comparison that
//! looked reasonable and used codepoint order would pass any test that did not name those pairs,
//! which is what made the earlier approximation pass five of the suite's twelve cases.

#![cfg(feature = "collator")]

use core::cmp::Ordering;

use tessella_style::collator::Collator;

fn strict() -> Collator {
    Collator {
        case_sensitive: true,
        diacritic_sensitive: true,
        locale: None,
    }
}

/// The three levels, one pair each.
#[test]
fn the_levels_separate_the_pairs_codepoint_order_confuses() {
    let collator = strict();

    assert_eq!(
        collator.compare("a", "b"),
        Ordering::Less,
        "primary: letters"
    );
    assert_eq!(
        collator.compare("a", "ä"),
        Ordering::Less,
        "secondary: the accent, where codepoint order agrees by accident"
    );
    assert_eq!(
        collator.compare("a", "A"),
        Ordering::Less,
        "tertiary: case, where codepoint order says the reverse"
    );
    assert_eq!(
        collator.compare("ä", "b"),
        Ordering::Less,
        "and an accented letter still sorts by the letter it is: 0xE4 is above 0x62"
    );
}

/// Dropping a level makes a difference disappear without making others disappear.
#[test]
fn each_switch_drops_exactly_its_own_level() {
    let base = Collator::default();
    assert!(base.equals("a", "A"), "case ignored");
    assert!(base.equals("a", "ä"), "and accents");
    assert_eq!(base.compare("a", "b"), Ordering::Less, "but not letters");

    let case_only = Collator {
        case_sensitive: true,
        diacritic_sensitive: false,
        locale: None,
    };
    assert!(!case_only.equals("a", "A"), "case counts");
    assert!(case_only.equals("a", "ä"), "accents do not");

    let accents_only = Collator {
        case_sensitive: false,
        diacritic_sensitive: true,
        locale: None,
    };
    assert!(accents_only.equals("a", "A"), "case does not count");
    assert!(!accents_only.equals("a", "ä"), "accents do");
}

/// The suite's own comparisons, by hand.
///
/// `collator/base-gt-en`, `case-lteq-en` and `variant-gteq-en` are three of the twelve cases the
/// expression suite runs. Their inputs are exactly the pairs above, and their expected outputs
/// are what these assert — so a change that broke the ordering would fail here with a name rather
/// than in the suite with a case number.
#[test]
fn the_suites_own_cases_come_out_as_it_says() {
    let base = Collator::default();
    assert!(
        base.compare("a", "ä") != Ordering::Greater,
        "base-gt: a > ä is false"
    );
    assert!(
        base.compare("a", "A") != Ordering::Greater,
        "base-gt: a > A is false"
    );
    assert!(
        base.compare("b", "ä") == Ordering::Greater,
        "base-gt: b > ä is true"
    );

    let case_insensitive_accent_sensitive = Collator {
        case_sensitive: false,
        diacritic_sensitive: true,
        locale: None,
    };
    let lteq =
        |a: &str, b: &str| case_insensitive_accent_sensitive.compare(a, b) != Ordering::Greater;
    assert!(!lteq("ä", "a"), "case-lteq: ä <= a is false");
    assert!(lteq("A", "a"), "case-lteq: A <= a is true");
    assert!(lteq("a", "a"), "case-lteq: a <= a is true");
    assert!(lteq("ä", "b"), "case-lteq: ä <= b is true");

    let strict = strict();
    let gteq = |a: &str, b: &str| strict.compare(a, b) != Ordering::Less;
    assert!(!gteq("a", "ä"), "variant-gteq: a >= ä is false");
    assert!(!gteq("a", "A"), "variant-gteq: a >= A is false");
    assert!(gteq("a", "a"), "variant-gteq: a >= a is true");
    assert!(gteq("b", "ä"), "variant-gteq: b >= ä is true");
}

/// Han is not in the table at all, and must still order.
///
/// The ideographs are given weights by construction rather than listed — without that every
/// Chinese label would compare equal to every other, which is the failure a map would show as a
/// label list in input order.
#[test]
fn ideographs_order_by_construction() {
    let collator = strict();
    assert_eq!(
        collator.compare("一", "丁"),
        Ordering::Less,
        "U+4E00 before U+4E01"
    );
    assert_ne!(
        collator.compare("中", "国"),
        Ordering::Equal,
        "two ideographs are not the same string"
    );
    // And they sort after Latin, which is where the implicit base puts them.
    assert_eq!(collator.compare("z", "一"), Ordering::Less);
}

/// A combining mark carries the same secondary as the precomposed letter.
#[test]
fn a_decomposed_letter_compares_as_the_letter() {
    let base = Collator::default();
    let strict = strict();
    // `a` + COMBINING DIAERESIS against the precomposed `ä`.
    assert!(
        base.equals("a\u{0308}", "a"),
        "the accent is ignored at the base level"
    );
    assert_eq!(
        strict.compare("a\u{0308}", "ä"),
        Ordering::Equal,
        "and written either way it is the same letter with the same accent"
    );
}

/// An empty string sorts before anything.
#[test]
fn the_empty_string_is_least() {
    let collator = strict();
    assert_eq!(collator.compare("", "a"), Ordering::Less);
    assert_eq!(collator.compare("", ""), Ordering::Equal);
    assert_eq!(collator.compare("a", ""), Ordering::Greater);
}

/// No locale is resolved, and saying so is what the suite checks.
///
/// `collator/accent-equals-de` asks whether the resolved locale is `de` and *branches on the
/// answer*: comparing `ü` with `ue` where a German tailoring exists, and checking the input
/// directly where none does. An implementation that reported back the locale it was asked for
/// would take the first branch without having the tailoring it promises — and answer `false`
/// where the suite expects `true`. mbgl's default collator returns the empty string for the same
/// reason, its own comment saying it would need ICU to do otherwise.
#[test]
fn no_locale_is_resolved_however_one_is_asked_for() {
    let collator = Collator {
        case_sensitive: true,
        diacritic_sensitive: false,
        locale: Some("de".to_owned()),
    };
    assert_eq!(collator.resolved_locale(), "");
    assert_eq!(Collator::default().resolved_locale(), "");

    // What that answer costs is worth being explicit about. Sent down the else-branch, the suite
    // checks its input directly rather than comparing — which is how it reaches `false` for
    // `ü` against `u`, because this collator, told to ignore accents, calls them equal.
    assert!(
        collator.equals("ü", "u"),
        "told to ignore accents, ü is u — which is why the suite does not ask this collator"
    );
}
