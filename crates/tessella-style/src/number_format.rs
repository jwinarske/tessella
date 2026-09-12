// SPDX-License-Identifier: Apache-2.0

//! `["number-format", value, options]`, which turns a number into the text a person reads.
//!
//! # Why this is written out rather than called for
//!
//! mbgl hands the job to ICU -- `icu::number::NumberFormatter` in
//! `platform/default/src/mbgl/i18n/number_format.cpp` -- and its *builtin* fallback, the one
//! compiled when ICU is absent, is `std::to_string(number)`. That fallback honours neither the
//! locale, the currency nor the digit bounds, and fails all three of the specification's own
//! cases. So there is no non-ICU transcription to make: either a frontend carries ICU's data or
//! it says what it does instead.
//!
//! This says what it does instead. Grouping, the decimal separator, the digit bounds and the
//! currency's own precision are all arithmetic over a decimal string, and those are what a style
//! is actually asking for when it formats a number. What is *not* here is CLDR: the separator
//! conventions below are the common ones written down, not a locale database, and a locale this
//! does not know takes the English pattern.
//!
//! # The decimal string, and why not the double
//!
//! The digits come from the shortest representation that round-trips the `f64`, which is what
//! Rust's `Display` produces -- and then the bounds are applied to *that*, as decimal text.
//!
//! Rounding the binary value instead would be wrong in a way the suite pins: `987654321.234567`
//! asked for fifteen fraction digits wants `987654321.234567000000000`, and the nearest double to
//! it is `987654321.234566986560821533203125`. Formatting the double to fifteen places gives the
//! second, which is the value the machine holds and not the number the style wrote. ICU works in
//! decimal for the same reason.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Formats `value` as `locale` would write it.
///
/// `currency` is an ISO 4217 code, or empty for a plain number. `min` and `max` bound the
/// fraction digits; a currency overrides them with its own precision, which is what ICU does --
/// yen has no minor unit, so `¥123,457` is the whole of it.
#[must_use]
pub fn format_number(value: f64, locale: &str, currency: &str, min: u8, max: u8) -> String {
    let separators = Separators::of(locale);
    let (min, max) = match Currency::of(currency) {
        Some(currency) => (currency.digits, currency.digits),
        // The spec's default is zero to three, which is ICU's for a plain number.
        None => (min, max.max(min)),
    };

    let negative = value.is_sign_negative() && value != 0.0;
    if !value.is_finite() {
        // Neither infinity nor a NaN has digits to group. ICU spells them out; this hands back
        // the same text Rust does, which is what a style would see from any other operator.
        return value.to_string();
    }
    let (whole, fraction) = round_decimal(&value.abs().to_string(), max);
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if let Some(currency) = Currency::of(currency) {
        out.push_str(currency.symbol);
    }
    group(&whole, separators, &mut out);
    let fraction = bound(&fraction, min);
    if !fraction.is_empty() {
        out.push(separators.decimal);
        out.push_str(&fraction);
    }
    out
}

/// What a locale puts between groups and before the fraction.
#[derive(Debug, Clone, Copy)]
struct Separators {
    group: char,
    decimal: char,
}

impl Separators {
    /// The convention for a locale, by its language.
    ///
    /// Three patterns cover most of what a map is written in, and the language subtag is what
    /// chooses between them -- `de-AT` groups like `de`. This is not CLDR and does not pretend
    /// to be: a locale it does not name takes the English pattern, which is also what a caller
    /// passing no locale at all gets.
    fn of(locale: &str) -> Self {
        let language = locale
            .split(['-', '_'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match language.as_str() {
            // A dot groups and a comma divides: German, Spanish, Italian, Dutch, Portuguese,
            // Turkish, Indonesian, Danish, Greek, Romanian.
            "de" | "es" | "it" | "nl" | "pt" | "tr" | "id" | "da" | "el" | "ro" => Self {
                group: '.',
                decimal: ',',
            },
            // A space groups and a comma divides: French, the Nordics, most of Slavic Europe.
            // The space is the narrow no-break one CLDR names, not an ordinary blank.
            "fr" | "ru" | "pl" | "cs" | "sk" | "sv" | "nb" | "no" | "fi" | "uk" | "hu" | "lv"
            | "lt" | "et" | "bg" => Self {
                group: '\u{202f}',
                decimal: ',',
            },
            _ => Self {
                group: ',',
                decimal: '.',
            },
        }
    }
}

/// A currency's symbol and how many minor digits it has.
struct Currency {
    symbol: &'static str,
    digits: u8,
}

impl Currency {
    /// The entry for an ISO 4217 code, or `None` for none and for the empty string.
    ///
    /// The symbols are the ones a map is likely to write. A code this does not know is printed as
    /// the code itself followed by a space, which is ICU's own fallback for a currency it has no
    /// symbol for -- so an unknown code is legible rather than absent.
    ///
    /// The digit counts are ISO 4217's: two almost everywhere, none for the currencies with no
    /// minor unit, and three for the dinars. They are here because they *override* the style's
    /// bounds, which is the part a caller cannot do for itself.
    fn of(code: &str) -> Option<Self> {
        if code.is_empty() {
            return None;
        }
        let (symbol, digits) = match code.to_ascii_uppercase().as_str() {
            "USD" => ("$", 2),
            "EUR" => ("\u{20ac}", 2),
            "GBP" => ("\u{a3}", 2),
            "JPY" => ("\u{a5}", 0),
            "CNY" => ("CN\u{a5}", 2),
            "KRW" => ("\u{20a9}", 0),
            "INR" => ("\u{20b9}", 2),
            "RUB" => ("\u{20bd}", 2),
            "BRL" => ("R$", 2),
            "CHF" => ("CHF\u{a0}", 2),
            "CAD" => ("CA$", 2),
            "AUD" => ("A$", 2),
            "MXN" => ("MX$", 2),
            "SEK" | "NOK" | "DKK" => ("kr\u{a0}", 2),
            "PLN" => ("z\u{142}\u{a0}", 2),
            "TRY" => ("\u{20ba}", 2),
            "ILS" => ("\u{20aa}", 2),
            "VND" => ("\u{20ab}", 0),
            "CLP" | "ISK" | "PYG" | "RWF" | "UGX" | "VUV" | "XAF" | "XOF" | "XPF" => ("", 0),
            "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => ("", 3),
            _ => ("", 2),
        };
        // A code with no symbol prints as the code, which is what ICU falls back to.
        Some(Self {
            symbol: if symbol.is_empty() { "" } else { symbol },
            digits,
        })
    }
}

/// Splits a decimal string into whole and fraction, rounding the fraction to `max` digits.
///
/// Half-even, which is ICU's default and the one that does not drift upward over a column of
/// numbers. The carry runs back through the fraction and into the whole part, so `9.99` at one
/// digit is `10.0` rather than `9.10`.
fn round_decimal(text: &str, max: u8) -> (String, String) {
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole.to_string(), fraction.to_string()),
        None => (text.to_string(), String::new()),
    };
    let max = max as usize;
    if fraction.len() <= max {
        return (whole, fraction);
    }

    let keep: Vec<u8> = fraction.bytes().take(max).map(|byte| byte - b'0').collect();
    let rest = &fraction[max..];
    let first = rest.as_bytes().first().copied().unwrap_or(b'0') - b'0';
    let beyond = rest.len() > 1 && rest[1..].bytes().any(|byte| byte != b'0');
    let last = keep.last().copied().unwrap_or_else(|| {
        whole
            .bytes()
            .last()
            .map_or(0, |byte| byte.saturating_sub(b'0'))
    });
    let round_up = first > 5 || (first == 5 && (beyond || last % 2 == 1));

    let mut digits = keep;
    let mut whole: Vec<u8> = whole.bytes().map(|byte| byte - b'0').collect();
    if round_up {
        let mut carry = true;
        for digit in digits.iter_mut().rev() {
            if !carry {
                break;
            }
            *digit += 1;
            carry = *digit == 10;
            if carry {
                *digit = 0;
            }
        }
        for digit in whole.iter_mut().rev() {
            if !carry {
                break;
            }
            *digit += 1;
            carry = *digit == 10;
            if carry {
                *digit = 0;
            }
        }
        if carry {
            whole.insert(0, 1);
        }
    }
    let text = |digits: &[u8]| -> String { digits.iter().map(|d| (d + b'0') as char).collect() };
    (text(&whole), text(&digits))
}

/// Brings a fraction to at least `min` digits and no trailing zeros beyond it.
///
/// Both halves matter and they pull opposite ways: `min` is what makes a price end in two zeros,
/// and the trim is what stops `9.99` rounded to one digit reading `10.0` where ICU says `10`. A
/// maximum bounds what is *kept*, not what is *shown*.
fn bound(fraction: &str, min: u8) -> String {
    let min = min as usize;
    let mut out = fraction.to_string();
    while out.len() > min && out.ends_with('0') {
        out.pop();
    }
    while out.len() < min {
        out.push('0');
    }
    out
}

/// Writes the whole part in groups of three.
fn group(whole: &str, separators: Separators, out: &mut String) {
    let digits = whole.as_bytes();
    for (index, digit) in digits.iter().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(separators.group);
        }
        out.push(*digit as char);
    }
}

#[cfg(test)]
mod tests {
    use super::format_number;

    /// The specification's own three cases, which are what ICU produces for `en-US`.
    #[test]
    fn the_suites_cases() {
        // Default: no bounds but the spec's zero-to-three, and grouping by thousands.
        assert_eq!(format_number(123_456.789, "en-US", "", 0, 3), "123,456.789");
        // Fifteen fraction digits of a number that has six: the rest are zeros, which is what
        // makes this a decimal operation rather than a binary one.
        assert_eq!(
            format_number(987_654_321.234_567, "en-US", "", 15, 20),
            "987,654,321.234567000000000"
        );
        assert_eq!(
            format_number(987_654_321.234_567, "en-US", "", 2, 4),
            "987,654,321.2346"
        );
        // A currency brings its own precision, and overrides the bounds with it.
        assert_eq!(format_number(123_456.789, "en-US", "JPY", 0, 3), "¥123,457");
        assert_eq!(
            format_number(123_456.789, "en-US", "EUR", 0, 3),
            "€123,456.79"
        );
    }

    /// Half-even, which is ICU's default: a five with nothing after it goes to the even digit.
    #[test]
    fn a_tie_rounds_to_even() {
        assert_eq!(format_number(0.125, "en-US", "", 0, 2), "0.12");
        assert_eq!(format_number(0.135, "en-US", "", 0, 2), "0.14");
        // And a five with anything after it is not a tie.
        assert_eq!(format_number(0.1251, "en-US", "", 0, 2), "0.13");
    }

    /// The carry runs out of the fraction and through the whole part.
    #[test]
    fn rounding_carries_into_the_integer() {
        assert_eq!(format_number(9.99, "en-US", "", 0, 1), "10");
        assert_eq!(format_number(999.999, "en-US", "", 0, 2), "1,000");
        assert_eq!(format_number(9.99, "en-US", "", 2, 2), "9.99");
    }

    /// The separator conventions this knows, and the fallback for one it does not.
    #[test]
    fn a_locale_chooses_its_separators() {
        assert_eq!(format_number(1_234.5, "de-DE", "", 0, 3), "1.234,5");
        assert_eq!(format_number(1_234.5, "fr-FR", "", 0, 3), "1\u{202f}234,5");
        assert_eq!(format_number(1_234.5, "en-GB", "", 0, 3), "1,234.5");
        // A locale with no entry takes the English pattern rather than failing.
        assert_eq!(format_number(1_234.5, "xx-YY", "", 0, 3), "1,234.5");
    }

    /// A negative number keeps its sign in front of the currency, as `en` writes one.
    #[test]
    fn a_negative_keeps_its_sign() {
        assert_eq!(format_number(-1_234.5, "en-US", "", 0, 2), "-1,234.5");
        assert_eq!(format_number(-12.0, "en-US", "USD", 0, 2), "-$12.00");
    }

    /// A code with no symbol in the table still formats, with its own precision.
    #[test]
    fn an_unknown_currency_still_formats() {
        assert_eq!(format_number(1_234.567, "en-US", "KWD", 0, 2), "1,234.567");
        assert_eq!(format_number(1_234.567, "en-US", "ZZZ", 0, 0), "1,234.57");
    }
}
