#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use serde_json::{Value, json};

/// Normalize a SA mobile number the same way the web app does (`0...` -> `+27...`).
pub(crate) fn normalize_msisdn(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if raw.trim_start().starts_with('+') {
        format!("+{digits}")
    } else if let Some(rest) = digits.strip_prefix("27") {
        format!("+27{rest}")
    } else if let Some(rest) = digits.strip_prefix('0') {
        format!("+27{rest}")
    } else if digits.len() == 9 {
        format!("+27{digits}")
    } else {
        digits
    }
}

pub(crate) fn normalize_date_of_birth(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if is_valid_dd_mm_yyyy(trimmed) {
        return Some(trimmed.to_string());
    }
    if let Some((year, month, day)) = trimmed
        .split_once('-')
        .and_then(|(year, rest)| rest.split_once('-').map(|(month, day)| (year, month, day)))
    {
        let candidate = format!("{day}/{month}/{year}");
        if year.len() == 4 && month.len() == 2 && day.len() == 2 && is_valid_dd_mm_yyyy(&candidate)
        {
            return Some(candidate);
        }
    }
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 8 {
        let candidate = format!("{}/{}/{}", &digits[0..2], &digits[2..4], &digits[4..8]);
        if is_valid_dd_mm_yyyy(&candidate) {
            return Some(candidate);
        }

        let candidate = format!("{}/{}/{}", &digits[6..8], &digits[4..6], &digits[0..4]);
        if is_valid_dd_mm_yyyy(&candidate) {
            return Some(candidate);
        }
    }
    None
}

pub(crate) enum IdentityDocument {
    SaId { number: String, birth_date: String },
    Passport { number: String, birth_date: String },
}

impl IdentityDocument {
    pub(crate) fn number(&self) -> &str {
        match self {
            IdentityDocument::SaId { number, .. } | IdentityDocument::Passport { number, .. } => {
                number
            }
        }
    }

    pub(crate) fn payload(&self) -> Value {
        match self {
            IdentityDocument::SaId { number, birth_date } => json!({
                "passportNumber": "",
                "saIdNumber": number,
                "birthDate": birth_date,
            }),
            IdentityDocument::Passport { number, birth_date } => json!({
                "passportNumber": number,
                "birthDate": birth_date,
            }),
        }
    }
}

pub(crate) fn parse_identity_document(raw: &str) -> Option<IdentityDocument> {
    let trimmed = raw.trim();
    if trimmed.to_ascii_lowercase().starts_with("passport") {
        let rest = trimmed
            .find(char::is_whitespace)
            .map(|idx| trimmed[idx..].trim())?;
        let (number, birth_date) = parse_passport_and_birth_date(rest)?;
        return Some(IdentityDocument::Passport { number, birth_date });
    }

    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if is_valid_sa_id(&digits) {
        let birth_date = birth_date_from_sa_id(&digits)?;
        return Some(IdentityDocument::SaId {
            number: digits,
            birth_date,
        });
    }
    None
}

fn parse_passport_and_birth_date(raw: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = raw
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("dob"))
        .collect();
    if parts.len() < 2 {
        return None;
    }
    let birth_date = parts
        .iter()
        .rev()
        .find_map(|part| normalize_date_of_birth(part))?;
    let number = parts
        .iter()
        .copied()
        .find(|part| normalize_date_of_birth(part).is_none())?
        .trim()
        .to_string();
    if number.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some((number, birth_date))
    } else {
        None
    }
}

fn is_valid_sa_id(id: &str) -> bool {
    if id.len() != 13 || !id.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if &id[0..7] == "0000000" || &id[0..7] == "6666666" || &id[6..10] == "0000" {
        return false;
    }
    if birth_date_from_sa_id(id).is_none() {
        return false;
    }

    let mut digits: Vec<u32> = id.chars().filter_map(|c| c.to_digit(10)).collect();
    let check = digits.pop().unwrap_or(10);
    let mut odd_sum = 0;
    let mut even_digits = String::new();
    while !digits.is_empty() {
        odd_sum += digits.remove(0);
        if !digits.is_empty() {
            even_digits.push(char::from_digit(digits.remove(0), 10).unwrap_or('0'));
        }
    }
    let even_sum: u32 = (even_digits.parse::<u32>().unwrap_or(0) * 2)
        .to_string()
        .chars()
        .filter_map(|c| c.to_digit(10))
        .sum();
    ((10 - ((odd_sum + even_sum) % 10)) % 10) == check
}

fn birth_date_from_sa_id(id: &str) -> Option<String> {
    birth_date_from_sa_id_with_cutoff(id, current_year_cutoff())
}

fn birth_date_from_sa_id_with_cutoff(id: &str, cutoff: u32) -> Option<String> {
    if id.len() < 6 {
        return None;
    }
    let yy = id[0..2].parse::<u32>().ok()?;
    let month = id[2..4].parse::<u32>().ok()?;
    let day = id[4..6].parse::<u32>().ok()?;
    let year = if yy > cutoff { 1900 + yy } else { 2000 + yy };
    if valid_date_parts(day, month, year) {
        Some(format!("{year:04}/{month:02}/{day:02}"))
    } else {
        None
    }
}

#[cfg(target_arch = "wasm32")]
fn current_year_cutoff() -> u32 {
    (js_sys::Date::new_0().get_full_year() as u32) % 100
}

#[cfg(not(target_arch = "wasm32"))]
fn current_year_cutoff() -> u32 {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64 / 86_400)
        .unwrap_or(0);
    (civil_year_from_days(days) as u32) % 100
}

#[cfg(not(target_arch = "wasm32"))]
fn civil_year_from_days(days_since_epoch: i64) -> i32 {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = (yoe + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    year
}

fn is_valid_dd_mm_yyyy(s: &str) -> bool {
    let bytes = s.as_bytes();
    if !(bytes.len() == 10
        && bytes[2] == b'/'
        && bytes[5] == b'/'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, b)| matches!(idx, 2 | 5) || b.is_ascii_digit()))
    {
        return false;
    }

    let day = s[0..2].parse::<u32>().ok();
    let month = s[3..5].parse::<u32>().ok();
    let year = s[6..10].parse::<u32>().ok();
    match (day, month, year) {
        (Some(day), Some(month), Some(year)) => valid_date_parts(day, month, year),
        _ => false,
    }
}

fn valid_date_parts(day: u32, month: u32, year: u32) -> bool {
    if !(1..=12).contains(&month) || year < 1900 || day == 0 {
        return false;
    }
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };
    day <= max_day
}

fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_sa_mobile_numbers() {
        assert_eq!(normalize_msisdn("0712345678"), "+27712345678");
        assert_eq!(normalize_msisdn("27 71 234 5678"), "+27712345678");
        assert_eq!(normalize_msisdn("+27 (71) 234 5678"), "+27712345678");
        assert_eq!(normalize_msisdn("712345678"), "+27712345678");
    }

    #[test]
    fn normalizes_supported_birth_date_formats() {
        assert_eq!(
            normalize_date_of_birth("21/05/1990"),
            Some("21/05/1990".into())
        );
        assert_eq!(
            normalize_date_of_birth("1990-05-21"),
            Some("21/05/1990".into())
        );
        assert_eq!(
            normalize_date_of_birth("21051990"),
            Some("21/05/1990".into())
        );
        assert_eq!(
            normalize_date_of_birth("19900521"),
            Some("21/05/1990".into())
        );
        assert_eq!(normalize_date_of_birth("31/02/1990"), None);
    }

    #[test]
    fn derives_birth_date_from_sa_id() {
        assert_eq!(
            birth_date_from_sa_id_with_cutoff("9001015009086", 26),
            Some("1990/01/01".into())
        );
        assert!(is_valid_sa_id("9001015009086"));
        assert!(!is_valid_sa_id("9001015009087"));
    }

    #[test]
    fn parses_identity_documents() {
        let sa_id = parse_identity_document("9001015009086").unwrap();
        assert_eq!(sa_id.number(), "9001015009086");
        assert_eq!(
            sa_id.payload(),
            json!({
                "passportNumber": "",
                "saIdNumber": "9001015009086",
                "birthDate": "1990/01/01",
            })
        );

        let passport = parse_identity_document("passport AB123456 21/05/1990").unwrap();
        assert_eq!(passport.number(), "AB123456");
        assert_eq!(
            passport.payload(),
            json!({
                "passportNumber": "AB123456",
                "birthDate": "21/05/1990",
            })
        );
    }
}
