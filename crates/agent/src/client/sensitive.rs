use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

/// A single sensitive-data match inside clipboard text. `masked` keeps only
/// the first and last two characters of the matched secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SensitiveFinding {
    pub kind: String,
    pub masked: String,
}

/// Scan `text` for sensitive data: credit cards (Luhn-verified), SA ID
/// numbers, US SSNs, JWTs, API keys, PEM private keys and password
/// assignments.
pub fn scan(text: &str) -> Vec<SensitiveFinding> {
    let mut findings = Vec::new();

    for m in CREDIT_CARD_RE.find_iter(text) {
        let digits: String = m.as_str().chars().filter(|c| c.is_ascii_digit()).collect();
        if !(13..=19).contains(&digits.len()) || !luhn_valid(&digits) {
            continue;
        }
        // A 13-digit Luhn-valid number with a valid YYMMDD date prefix is
        // more likely an SA ID number than a card; classify it as such.
        if digits.len() == 13 && sa_id_date_valid(&digits) {
            findings.push(finding("sa_id_number", &digits));
        } else {
            findings.push(finding("credit_card", &digits));
        }
    }

    for m in SSN_RE.find_iter(text) {
        findings.push(finding("ssn", m.as_str()));
    }

    for m in JWT_RE.find_iter(text) {
        findings.push(finding("jwt", m.as_str()));
    }

    for m in API_KEY_RE.find_iter(text) {
        findings.push(finding("api_key", m.as_str()));
    }

    for m in PRIVATE_KEY_RE.find_iter(text) {
        findings.push(finding("private_key", m.as_str()));
    }

    for caps in PASSWORD_RE.captures_iter(text) {
        if let Some(value) = caps.get(1) {
            findings.push(finding("password", value.as_str()));
        }
    }

    findings
}

fn finding(kind: &str, secret: &str) -> SensitiveFinding {
    SensitiveFinding {
        kind: kind.to_string(),
        masked: mask(secret),
    }
}

/// Keep only the first and last two characters; everything between becomes
/// `*`. Very short secrets (<= 4 chars) are fully masked.
fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..2].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{}{}{}", head, "*".repeat(chars.len() - 4), tail)
}

/// Standard Luhn checksum over a string of ASCII digits.
fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    // Double every second digit starting from the rightmost-but-one.
    for (i, c) in digits.chars().rev().enumerate() {
        let Some(mut d) = c.to_digit(10) else {
            return false;
        };
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    !digits.is_empty() && sum % 10 == 0
}

/// SA ID numbers start with a YYMMDD birth date.
fn sa_id_date_valid(digits: &str) -> bool {
    let month: u32 = digits[2..4].parse().unwrap_or(0);
    let day: u32 = digits[4..6].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

// 13-19 digits, allowing single spaces or dashes between digits.
static CREDIT_CARD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:\d[ -]?){13,19}\b").unwrap());
static SSN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap());
static JWT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b").unwrap());
static API_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:sk-[A-Za-z0-9]{16,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|xoxb-[0-9A-Za-z-]{10,}|xoxp-[0-9A-Za-z-]{10,}|AIza[0-9A-Za-z_-]{20,})\b",
    )
    .unwrap()
});
static PRIVATE_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |ENCRYPTED )?PRIVATE KEY(?: BLOCK)?-----")
        .unwrap()
});
static PASSWORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:password|passwd|pwd)\s*[:=]\s*(\S+)").unwrap());

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<String> {
        scan(text).into_iter().map(|f| f.kind).collect()
    }

    #[test]
    fn detects_credit_card_with_luhn() {
        // 4111 1111 1111 1111 passes the Luhn check.
        let found = scan("pay with card 4111 1111 1111 1111 please");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "credit_card");
        assert_eq!(found[0].masked, mask("4111111111111111"));
        assert_eq!(found[0].masked.chars().filter(|c| *c == '*').count(), 12);
    }

    #[test]
    fn luhn_rejects_random_digit_strings() {
        assert!(kinds("card 4111 1111 1111 1112").is_empty());
        assert!(kinds("number 1234567890123").is_empty());
        assert!(!luhn_valid("1234567812345671"));
        assert!(luhn_valid("4111111111111111"));
    }

    #[test]
    fn detects_sa_id_number() {
        // 8001015009087: valid 1 Jan 1980 date and Luhn checksum.
        let found = scan("id 8001015009087");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "sa_id_number");
    }

    #[test]
    fn rejects_invalid_sa_id_number() {
        // Bad Luhn checksum.
        assert!(kinds("id 8001015009088").is_empty());
        // Bad month (13).
        assert!(kinds("id 8013015009084").is_empty());
    }

    #[test]
    fn detects_ssn() {
        assert_eq!(kinds("ssn: 123-45-6789"), vec!["ssn"]);
        assert!(kinds("call 123456789").is_empty());
    }

    #[test]
    fn detects_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert_eq!(kinds(jwt), vec!["jwt"]);
        assert!(kinds("eyJnot-a-jwt").is_empty());
    }

    #[test]
    fn detects_api_keys() {
        assert_eq!(kinds("key sk-abcdefghijklmnop"), vec!["api_key"]);
        assert_eq!(kinds("ghp_abcdefghijklmnopqrst"), vec!["api_key"]);
        assert_eq!(kinds("AKIAIOSFODNN7EXAMPLE"), vec!["api_key"]);
        assert!(kinds("sk-short").is_empty());
        assert!(kinds("notakey").is_empty());
    }

    #[test]
    fn detects_private_key_headers() {
        assert_eq!(
            kinds("-----BEGIN OPENSSH PRIVATE KEY-----"),
            vec!["private_key"]
        );
        assert_eq!(
            kinds("-----BEGIN RSA PRIVATE KEY-----"),
            vec!["private_key"]
        );
        assert!(kinds("-----BEGIN CERTIFICATE-----").is_empty());
    }

    #[test]
    fn detects_password_assignments() {
        let found = scan("password=Hunter2Secret");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "password");
        assert_eq!(found[0].masked, mask("Hunter2Secret"));
        assert_eq!(kinds("Password: s3cr3t-value"), vec!["password"]);
        assert!(kinds("no password field here").is_empty());
    }

    #[test]
    fn mask_keeps_first_and_last_two_chars() {
        assert_eq!(mask("4111111111111111"), format!("41{}11", "*".repeat(12)));
        assert_eq!(mask("abcdef"), "ab**ef");
        assert_eq!(mask("abcd"), "****");
        assert_eq!(mask("ab"), "**");
    }
}
