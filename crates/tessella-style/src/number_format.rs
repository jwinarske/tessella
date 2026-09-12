// SPDX-License-Identifier: Apache-2.0

//! `["number-format", value, options]`, which turns a number into the text a person reads.
//!
//! # Why this is CLDR's data and not a table written here
//!
//! mbgl hands the job to ICU -- `icu::number::NumberFormatter` in
//! `platform/default/src/mbgl/i18n/number_format.cpp` -- and its *builtin* fallback, compiled when
//! ICU is absent, is `std::to_string(number)`: no locale, no currency, no digit bounds, and it
//! fails all three of the specification's own cases. So there is no non-ICU transcription to make.
//!
//! A hand-written formatter stood here first and got four things wrong that only data can get
//! right. Grouping *shape*: South Asian locales group 2-2-3, so `hi-IN` writes `1,23,45,678` where
//! grouping by threes writes `12,345,678` -- a different number to a reader. Numbering systems:
//! `ar-EG` writes its digits and separators as `١٢٬٣٤٥٬٦٧٨٫٩`. Currency placement: most of Europe
//! writes `1.234,50 €` where English writes `€1,234.50`. And the tail of CLDR behind all three,
//! which is hundreds of locales against the two dozen a table can name.
//!
//! `icu4x` is ICU's data without ICU: pure Rust, `no_std`, and the cross lanes stay green -- which
//! is exactly why it is this and not the C library mbgl links. The baked data costs 125 KiB in the
//! stripped, link-time-optimized `libtessella_ffi.so`, decimal and currency together. Currency is
//! `icu_experimental`, which is pre-1.0 and will move; the decimal half is not.
//!
//! # The digits come from the decimal string, not the double
//!
//! `987654321.234567` asked for fifteen fraction digits wants `987654321.234567000000000`, and the
//! nearest double to it is `987654321.234566986560821533203125`. Rounding the binary value gives
//! the second -- what the machine holds rather than what the style wrote.
//!
//! So the number reaches `fixed_decimal` as the shortest text that round-trips the `f64`, which is
//! what `Display` produces, and every bound after that is decimal arithmetic. ICU works this way
//! for the same reason.

use alloc::string::{String, ToString};

use fixed_decimal::{Decimal, SignedRoundingMode, UnsignedRoundingMode};
use icu_decimal::DecimalFormatter;
use icu_locale_core::Locale;

/// Formats `value` as `locale` would write it.
///
/// `currency` is an ISO 4217 code, or empty for a plain number. `min` and `max` bound the fraction
/// digits; a currency brings its own precision instead, which is CLDR's and not the style's -- yen
/// has no minor unit, so `¥123,457` is the whole of it.
///
/// A locale that does not parse, or one CLDR has no data for, falls back to the root locale rather
/// than failing: a number formatted in the wrong convention is legible, and an expression that
/// errors takes the label with it.
#[must_use]
pub fn format_number(value: f64, locale: &str, currency: &str, min: u8, max: u8) -> String {
    // Neither an infinity nor a NaN has digits to group, and `Decimal` will not parse one.
    let Ok(decimal) = value.to_string().parse::<Decimal>() else {
        return value.to_string();
    };
    let locale: Locale = locale.parse().unwrap_or(Locale::UNKNOWN);

    if !currency.is_empty()
        && let Some(text) = currency_text(&decimal, &locale, currency)
    {
        return text;
    }

    let mut decimal = decimal;
    #[allow(clippy::cast_possible_wrap)]
    {
        decimal.round_with_mode(
            -(max.min(20) as i16),
            SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfEven),
        );
        // Rounding leaves the digits it rounded to, so `9.99` at one digit is `10.0` where ICU
        // says `10`: a maximum bounds what is *kept*, not what is shown. The minimum then pads
        // back out, which is what makes a price end in two zeros.
        decimal.trim_end();
        decimal.pad_end(-(min.min(20) as i16));
    }
    match DecimalFormatter::try_new(
        (&locale).into(),
        icu_decimal::options::DecimalFormatterOptions::default(),
    ) {
        Ok(formatter) => formatter.format(&decimal).to_string(),
        // No data for the locale is not a reason to lose the label.
        Err(_) => decimal.to_string(),
    }
}

/// The currency form, or `None` when CLDR has no symbol for the code.
///
/// The formatter applies the currency's own precision -- `JPY` rounds to whole yen and `EUR` to
/// cents -- so the style's digit bounds do not reach here. That is ICU's rule and the
/// specification's case pins it.
fn currency_text(decimal: &Decimal, locale: &Locale, currency: &str) -> Option<String> {
    use icu_experimental::dimension::currency::CurrencyType;
    use icu_experimental::dimension::currency::formatter::CurrencyFormatter;

    let code: CurrencyType = currency.parse().ok()?;
    let formatter =
        CurrencyFormatter::try_new_symbol(locale.into(), code, Default::default()).ok()?;
    Some(formatter.format_fixed_decimal(decimal).to_string())
}

#[cfg(test)]
mod tests {
    use super::format_number;

    /// The specification's own three cases, which are what ICU produces for `en-US`.
    #[test]
    fn the_suites_cases() {
        assert_eq!(format_number(123_456.789, "en-US", "", 0, 3), "123,456.789");
        // Fifteen fraction digits of a number that has six: the rest are zeros, which is what
        // makes this decimal arithmetic rather than binary.
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

    /// What a hand-written formatter could not do, and why this crate is here.
    #[test]
    fn cldr_decides_the_shape_not_only_the_separators() {
        // South Asian grouping is 2-2-3, not by threes.
        assert_eq!(
            format_number(12_345_678.0, "hi-IN", "", 0, 3),
            "1,23,45,678"
        );
        // A numbering system is part of the locale: digits *and* separators.
        assert_eq!(format_number(1_234.5, "ar-EG", "", 0, 3), "١٬٢٣٤٫٥");
        // And a currency goes where the locale puts it, which is not always in front.
        assert_eq!(
            format_number(1_234.5, "de-DE", "EUR", 0, 3),
            "1.234,50\u{a0}€"
        );
        assert_eq!(format_number(1_234.5, "en-US", "EUR", 0, 3), "€1,234.50");
    }

    /// Half-even, which is ICU's default: a five with nothing after it goes to the even digit.
    #[test]
    fn a_tie_rounds_to_even() {
        assert_eq!(format_number(0.125, "en-US", "", 0, 2), "0.12");
        assert_eq!(format_number(0.135, "en-US", "", 0, 2), "0.14");
        assert_eq!(format_number(0.1251, "en-US", "", 0, 2), "0.13");
    }

    /// The carry runs out of the fraction and through the whole part.
    #[test]
    fn rounding_carries_into_the_integer() {
        assert_eq!(format_number(9.99, "en-US", "", 0, 1), "10");
        assert_eq!(format_number(999.999, "en-US", "", 0, 2), "1,000");
        assert_eq!(format_number(9.99, "en-US", "", 2, 2), "9.99");
    }

    /// A locale with no data falls back to the root rather than losing the label.
    #[test]
    fn an_unknown_locale_still_formats() {
        assert_eq!(format_number(1_234.5, "xx-YY", "", 0, 3), "1,234.5");
        assert_eq!(format_number(1_234.5, "not a tag", "", 0, 3), "1,234.5");
    }

    /// So does a currency CLDR has no symbol for: the code stands in for it.
    #[test]
    fn an_unknown_currency_still_formats() {
        let text = format_number(1_234.5, "en-US", "ZZZ", 0, 3);
        assert!(text.contains("1,234.5"), "{text}");
    }

    /// Neither an infinity nor a NaN has digits to group.
    #[test]
    fn the_values_with_no_digits_pass_through() {
        assert_eq!(format_number(f64::INFINITY, "en-US", "", 0, 3), "inf");
        assert_eq!(format_number(f64::NAN, "en-US", "", 0, 3), "NaN");
    }
}
