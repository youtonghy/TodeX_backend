//! National ID numbers. A number with a strong check or a distinctive
//! shape counts on its own: Chinese, Hong Kong and Taiwan IDs, US Social
//! Security numbers written `123-45-6789`, UK National Insurance numbers,
//! Singapore NRIC/FIN, Korean resident registration numbers, Spanish
//! DNI/NIE and formatted Brazilian CPFs. One that random digits pass too
//! often only counts after a word naming it: Australian tax file and
//! Medicare numbers, Canadian SINs, Aadhaar, Indian PAN, Japanese My
//! Number, undashed SSNs and CPFs, and passport numbers.

use std::sync::OnceLock;

use regex::bytes::Regex;

use super::super::{Category, Finding};
use super::{digit_values, find_numbers, luhn, NumberRule};

const SSN_WORDS: &[&str] = &["ssn", "social security", "社会安全号", "社会保障号"];
const HKID_WORDS: &[&str] = &["hkid", "hong kong id", "香港身份证", "香港身份證", "身份證"];
const CPF_WORDS: &[&str] = &["cpf"];
const PASSPORT_WORDS: &[&str] = &[
    "passport",
    "passeport",
    "reisepass",
    "pasaporte",
    "passaporte",
    "护照",
    "護照",
    "パスポート",
    "旅券",
    "여권",
];

const RULES: &[NumberRule] = &[
    NumberRule {
        label: "cn_id",
        pattern: r"[1-9][0-9]{5}(?:18|19|20)[0-9]{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12][0-9]|3[01])[0-9]{3}[0-9Xx]",
        valid: valid_cn_id,
        context: &[],
    },
    NumberRule {
        label: "hk_id",
        pattern: r"[A-Z]{1,2}[0-9]{6}\([0-9A]\)",
        valid: valid_hk_id,
        context: &[],
    },
    NumberRule {
        label: "hk_id",
        pattern: r"[A-Z]{1,2}[0-9]{6}[0-9A]",
        valid: valid_hk_id,
        context: HKID_WORDS,
    },
    NumberRule {
        label: "tw_id",
        pattern: r"[A-Z][1289][0-9]{8}",
        valid: valid_tw_id,
        context: &[],
    },
    NumberRule {
        label: "us_ssn",
        pattern: r"[0-9]{3}-[0-9]{2}-[0-9]{4}",
        valid: valid_ssn,
        context: &[],
    },
    NumberRule {
        label: "us_ssn",
        pattern: r"[0-9]{3} [0-9]{2} [0-9]{4}|[0-9]{9}",
        valid: valid_ssn,
        context: SSN_WORDS,
    },
    NumberRule {
        label: "uk_nino",
        pattern: r"[A-CEGHJ-PR-TW-Z][A-CEGHJ-NPR-TW-Z] ?[0-9]{2} ?[0-9]{2} ?[0-9]{2} ?[A-D]",
        valid: valid_nino,
        context: &[],
    },
    NumberRule {
        label: "au_tfn",
        pattern: r"[0-9]{3}[ -]?[0-9]{3}[ -]?[0-9]{2,3}",
        valid: valid_tfn,
        context: &["tfn", "tax file"],
    },
    NumberRule {
        label: "au_medicare",
        pattern: r"[2-6][0-9]{3}[ -]?[0-9]{5}[ -]?[0-9](?:[ -]?[1-9])?",
        valid: valid_medicare,
        context: &["medicare"],
    },
    NumberRule {
        label: "ca_sin",
        pattern: r"[1-79][0-9]{2}[ -]?[0-9]{3}[ -]?[0-9]{3}",
        valid: valid_sin,
        context: &["sin", "social insurance", "assurance sociale", "nas"],
    },
    NumberRule {
        label: "in_aadhaar",
        pattern: r"[2-9][0-9]{3}[ -]?[0-9]{4}[ -]?[0-9]{4}",
        valid: valid_aadhaar,
        context: &["aadhaar", "aadhar", "uidai", "आधार"],
    },
    NumberRule {
        label: "in_pan",
        pattern: r"[A-Z]{3}[ABCFGHJLPT][A-Z][0-9]{4}[A-Z]",
        valid: |_| true,
        context: &["pan", "permanent account"],
    },
    NumberRule {
        label: "sg_nric",
        pattern: r"[STFG][0-9]{7}[A-Z]",
        valid: valid_nric,
        context: &[],
    },
    NumberRule {
        label: "kr_rrn",
        pattern: r"[0-9]{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12][0-9]|3[01])-[1-8][0-9]{6}",
        valid: valid_rrn,
        context: &[],
    },
    NumberRule {
        label: "jp_my_number",
        pattern: r"[0-9]{4}[ -]?[0-9]{4}[ -]?[0-9]{4}",
        valid: valid_my_number,
        context: &["my number", "mynumber", "マイナンバー", "個人番号"],
    },
    NumberRule {
        label: "es_dni",
        pattern: r"[0-9]{8}-?[A-Z]|[XYZ]-?[0-9]{7}-?[A-Z]",
        valid: valid_dni,
        context: &[],
    },
    NumberRule {
        label: "br_cpf",
        pattern: r"[0-9]{3}\.[0-9]{3}\.[0-9]{3}-[0-9]{2}",
        valid: valid_cpf,
        context: &[],
    },
    NumberRule {
        label: "br_cpf",
        pattern: r"[0-9]{11}",
        valid: valid_cpf,
        context: CPF_WORDS,
    },
    NumberRule {
        label: "passport",
        pattern: r"[A-Z]{1,2}[0-9]{6,9}|[0-9]{8,9}",
        valid: |_| true,
        context: PASSPORT_WORDS,
    },
];

pub(super) fn find(bytes: &[u8], findings: &mut Vec<Finding>) {
    static COMPILED: OnceLock<Vec<Regex>> = OnceLock::new();
    find_numbers(bytes, RULES, &COMPILED, Category::IdCard, findings);
}

/// 18-digit PRC resident ID: a real birth date and the ISO 7064 check.
fn valid_cn_id(id: &[u8]) -> bool {
    const WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CHECK: &[u8; 11] = b"10X98765432";
    let digits = digit_values(&id[..17]);
    let number = |range: std::ops::Range<usize>| {
        digits[range]
            .iter()
            .fold(0, |value, digit| value * 10 + digit)
    };
    if !real_date(number(6..10), number(10..12), number(12..14)) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .zip(WEIGHTS)
        .map(|(digit, weight)| digit * weight)
        .sum();
    CHECK[(sum % 11) as usize] == id[17].to_ascii_uppercase()
}

/// Hong Kong identity card: one or two letters, six digits and a MOD 11
/// check digit (`A` for 10), a missing first letter counting as 36.
fn valid_hk_id(raw: &[u8]) -> bool {
    let chars: Vec<u8> = raw
        .iter()
        .copied()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .collect();
    let (body, check) = chars.split_at(chars.len() - 1);
    let mut values: Vec<u32> = body
        .iter()
        .map(|byte| match byte {
            b'A'..=b'Z' => u32::from(byte - b'A') + 10,
            _ => u32::from(byte - b'0'),
        })
        .collect();
    if values.len() == 7 {
        values.insert(0, 36);
    }
    let sum: u32 = values
        .iter()
        .zip((2..=9).rev())
        .map(|(value, weight)| value * weight)
        .sum();
    let expected = (11 - sum % 11) % 11;
    let check = match check[0] {
        b'A' => 10,
        digit => u32::from(digit - b'0'),
    };
    check == expected
}

/// Taiwan national ID or resident certificate: the letter's area code and
/// the nine digits, weighted 1, 9, 8 … 1, 1, sum to a multiple of 10.
fn valid_tw_id(raw: &[u8]) -> bool {
    const AREA: &[u8; 26] = &[
        10, 11, 12, 13, 14, 15, 16, 17, 34, 18, 19, 20, 21, 22, 35, 23, 24, 25, 26, 27, 28, 29, 32,
        30, 31, 33,
    ];
    let area = u32::from(AREA[usize::from(raw[0] - b'A')]);
    let digits = digit_values(&raw[1..]);
    let body: u32 = digits[..8]
        .iter()
        .zip((1..=8).rev())
        .map(|(digit, weight)| digit * weight)
        .sum();
    (area / 10 + area % 10 * 9 + body + digits[8]).is_multiple_of(10)
}

/// US Social Security number: no area 000, 666 or 900–999, no group 00 and
/// no serial 0000.
fn valid_ssn(raw: &[u8]) -> bool {
    let digits = digit_values(raw);
    let area = digits[0] * 100 + digits[1] * 10 + digits[2];
    area != 0
        && area != 666
        && area < 900
        && digits[3..5].iter().any(|digit| *digit != 0)
        && digits[5..].iter().any(|digit| *digit != 0)
}

/// UK National Insurance number: prefixes HMRC never issues are out.
fn valid_nino(raw: &[u8]) -> bool {
    !matches!(
        &raw[..2],
        b"BG" | b"GB" | b"KN" | b"NK" | b"NT" | b"TN" | b"ZZ"
    )
}

/// Australian tax file number: 8 or 9 digits whose weighted sum is a
/// multiple of 11.
fn valid_tfn(raw: &[u8]) -> bool {
    let digits = digit_values(raw);
    let weights: &[u32] = match digits.len() {
        9 => &[1, 4, 3, 7, 5, 8, 6, 9, 10],
        8 => &[10, 7, 8, 4, 6, 3, 5, 1],
        _ => return false,
    };
    let sum: u32 = digits
        .iter()
        .zip(weights)
        .map(|(digit, weight)| digit * weight)
        .sum();
    sum.is_multiple_of(11)
}

/// Australian Medicare card: the ninth digit checks the first eight, then
/// the issue number and, optionally, the individual reference number.
fn valid_medicare(raw: &[u8]) -> bool {
    const WEIGHTS: [u32; 8] = [1, 3, 7, 9, 1, 3, 7, 9];
    let digits = digit_values(raw);
    let sum: u32 = digits
        .iter()
        .zip(WEIGHTS)
        .map(|(digit, weight)| digit * weight)
        .sum();
    sum % 10 == digits[8]
}

/// Canadian Social Insurance number: Luhn.
fn valid_sin(raw: &[u8]) -> bool {
    let digits: Vec<u8> = raw.iter().copied().filter(u8::is_ascii_digit).collect();
    luhn(&digits)
}

/// Aadhaar: Verhoeff.
fn valid_aadhaar(raw: &[u8]) -> bool {
    const MULTIPLY: [[u8; 10]; 10] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
        [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
        [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
        [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
        [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
        [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
        [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
        [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
        [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    ];
    const PERMUTE: [[u8; 10]; 8] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
        [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
        [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
        [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
        [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
        [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
        [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
    ];
    let check = digit_values(raw)
        .iter()
        .rev()
        .enumerate()
        .fold(0u8, |check, (index, digit)| {
            MULTIPLY[usize::from(check)][usize::from(PERMUTE[index % 8][*digit as usize])]
        });
    check == 0
}

/// Singapore NRIC (S, T) and FIN (F, G): the letter checks the digits.
fn valid_nric(raw: &[u8]) -> bool {
    const WEIGHTS: [u32; 7] = [2, 7, 6, 5, 4, 3, 2];
    let digits = digit_values(&raw[1..8]);
    let offset = if matches!(raw[0], b'T' | b'G') { 4 } else { 0 };
    let sum: u32 = digits
        .iter()
        .zip(WEIGHTS)
        .map(|(digit, weight)| digit * weight)
        .sum();
    let letters: &[u8; 11] = if matches!(raw[0], b'S' | b'T') {
        b"JZIHGFEDCBA"
    } else {
        b"XWUTRQPNMLK"
    };
    letters[((sum + offset) % 11) as usize] == raw[8]
}

/// Korean resident registration number: a real birth date (the seventh
/// digit gives the century); numbers issued since 2020 carry no check digit.
fn valid_rrn(raw: &[u8]) -> bool {
    let digits = digit_values(raw);
    let century = match digits[6] {
        1 | 2 | 5 | 6 => 1900,
        _ => 2000,
    };
    real_date(
        century + digits[0] * 10 + digits[1],
        digits[2] * 10 + digits[3],
        digits[4] * 10 + digits[5],
    )
}

/// Japanese My Number: the twelfth digit checks the first eleven.
fn valid_my_number(raw: &[u8]) -> bool {
    let digits = digit_values(raw);
    let sum: u32 = digits[..11]
        .iter()
        .rev()
        .zip(1u32..)
        .map(|(digit, n)| digit * if n <= 6 { n + 1 } else { n - 5 })
        .sum();
    let remainder = sum % 11;
    let check = if remainder <= 1 { 0 } else { 11 - remainder };
    check == digits[11]
}

/// Spanish DNI and NIE: the letter is the number MOD 23, NIE's X, Y, Z
/// standing for 0, 1, 2.
fn valid_dni(raw: &[u8]) -> bool {
    const LETTERS: &[u8; 23] = b"TRWAGMYFPDXBNJZSQVHLCKE";
    let lead = match raw[0] {
        b'X' => 0,
        b'Y' => 1,
        b'Z' => 2,
        _ => 0,
    };
    let number = digit_values(raw)
        .iter()
        .fold(lead, |value, digit| value * 10 + digit);
    LETTERS[(number % 23) as usize] == raw[raw.len() - 1]
}

/// Brazilian CPF: two MOD 11 check digits, and not one digit repeated.
fn valid_cpf(raw: &[u8]) -> bool {
    let digits = digit_values(raw);
    if digits.iter().all(|digit| *digit == digits[0]) {
        return false;
    }
    let check = |count: usize| {
        let sum: u32 = digits[..count]
            .iter()
            .zip((2..=count as u32 + 1).rev())
            .map(|(digit, weight)| digit * weight)
            .sum();
        sum * 10 % 11 % 10
    };
    check(9) == digits[9] && check(10) == digits[10]
}

fn real_date(year: u32, month: u32, day: u32) -> bool {
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return false,
    };
    (1..=days).contains(&day)
}
