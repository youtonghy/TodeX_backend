//! Phone numbers (an aggressive category, off by default): any number in
//! international `+<country code>` form, the national formats of mainland
//! China, the US and Canada, Australia, the UK, France, Japan, Korea and
//! Taiwan, and any other 7–15 digit number right after a phone word.

use std::sync::OnceLock;

use regex::bytes::Regex;

use super::super::{Category, Finding};
use super::{digit_values, find_numbers, NumberRule};

const PHONE_WORDS: &[&str] = &[
    "phone",
    "telephone",
    "tel",
    "mobile",
    "cell",
    "fax",
    "whatsapp",
    "téléphone",
    "telefon",
    "teléfono",
    "telefone",
    "电话",
    "手机",
    "联系方式",
    "電話",
    "手機",
    "携帯",
    "전화",
    "휴대폰",
];

const RULES: &[NumberRule] = &[
    // Mainland China mobile, optionally +86, grouped 3-4-4.
    NumberRule {
        label: "phone",
        pattern: r"(?:\+86[ -]?)?1[3-9][0-9](?:[0-9]{8}|[ -][0-9]{4}[ -][0-9]{4})",
        valid: same_separators,
        context: &[],
    },
    // `+61 4 1234 5678`, `+1 (415) 555-2671`, `+44 20 7946 0958`.
    NumberRule {
        label: "phone",
        pattern: r"\+[1-9][0-9]{0,3}[ .-]?(?:\([0-9]{1,4}\)[ .-]?)?[0-9]{1,4}(?:[ .-]?[0-9]{1,4}){1,5}",
        valid: international,
        context: &[],
    },
    // US and Canada: `(415) 555-2671`, `415-555-2671`, `1-800-555-0199`.
    NumberRule {
        label: "phone",
        pattern: r"(?:1[ .-]?)?(?:\([2-9][0-9]{2}\)[ .-]?|[2-9][0-9]{2}[.-])[2-9][0-9]{2}[.-][0-9]{4}",
        valid: north_american,
        context: &[],
    },
    // Australian (04) and Taiwanese (09) mobiles, Australian landlines
    // and 1300/1800 numbers.
    NumberRule {
        label: "phone",
        pattern: r"0[49][0-9]{2}[ -]?[0-9]{3}[ -]?[0-9]{3}|\(0[2378]\) ?[0-9]{4}[ -]?[0-9]{4}|0[2378][ -][0-9]{4}[ -][0-9]{4}|1[38]00[ -][0-9]{3}[ -][0-9]{3}",
        valid: same_separators,
        context: &[],
    },
    // UK mobiles and spaced geographic numbers.
    NumberRule {
        label: "phone",
        pattern: r"07[0-9]{3} ?[0-9]{6}|07[0-9]{3} [0-9]{3} [0-9]{3}|\(?0[12][0-9]{1,3}\)? [0-9]{3,4} [0-9]{3,4}",
        valid: ten_or_eleven_digits,
        context: &[],
    },
    // France: `06 12 34 56 78`.
    NumberRule {
        label: "phone",
        pattern: r"0[1-9](?:[ .][0-9]{2}){4}",
        valid: same_separators,
        context: &[],
    },
    // Dashed numbers with a trunk 0: Japan `090-1234-5678`, `03-1234-5678`,
    // Korea `010-1234-5678`, Taiwan `02-2345-6789`.
    NumberRule {
        label: "phone",
        pattern: r"0[1-9][0-9]{0,3}-[0-9]{1,4}-[0-9]{4}",
        valid: ten_or_eleven_digits,
        context: &[],
    },
    NumberRule {
        label: "phone",
        pattern: r"\+?(?:\([0-9]{1,5}\)[ .-]?)?[0-9]{1,15}(?:[ .-][0-9]{1,15}){0,5}",
        valid: seven_to_fifteen_digits,
        context: PHONE_WORDS,
    },
];

pub(super) fn find(bytes: &[u8], findings: &mut Vec<Finding>) {
    static COMPILED: OnceLock<Vec<Regex>> = OnceLock::new();
    find_numbers(bytes, RULES, &COMPILED, Category::Phone, findings);
}

/// Digit groups split by one kind of separator (or none), after any
/// country code.
fn same_separators(raw: &[u8]) -> bool {
    let national = raw.strip_prefix(b"+86").unwrap_or(raw);
    let mut separators = national
        .iter()
        .skip_while(|byte| !byte.is_ascii_digit())
        .filter(|byte| !byte.is_ascii_digit() && !matches!(byte, b'(' | b')'));
    let first = separators.next();
    separators.all(|byte| Some(byte) == first)
}

/// E.164 allows at most 15 digits; fewer than 8 is a short code or noise.
fn international(raw: &[u8]) -> bool {
    (8..=15).contains(&digit_values(raw).len())
}

/// No N11 service code (411, 911) as the area code.
fn north_american(raw: &[u8]) -> bool {
    let digits = digit_values(raw);
    let area = &digits[digits.len() - 10..digits.len() - 7];
    !(area[1] == 1 && area[2] == 1)
}

fn ten_or_eleven_digits(raw: &[u8]) -> bool {
    (10..=11).contains(&digit_values(raw).len())
}

fn seven_to_fifteen_digits(raw: &[u8]) -> bool {
    (7..=15).contains(&digit_values(raw).len())
}
