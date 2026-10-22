//! CAS 번호 정리·검증 (순수 함수)

#[derive(Debug, Clone, PartialEq)]
pub enum CasCheck {
    /// 빈 값, "-", "N/A" → cas_no, cas_valid 모두 NULL
    Missing,
    /// 영업비밀 등 → cas_no "비공개", cas_valid NULL
    Confidential,
    /// 형식·체크섬 통과
    Valid(String),
    /// 정리한 값과 실패 이유. cas_valid = false로 저장한다
    Invalid { cleaned: String, reason: String },
}

impl CasCheck {
    /// DB에 저장할 (cas_no, cas_valid)
    pub fn db_values(&self) -> (Option<String>, Option<bool>) {
        match self {
            CasCheck::Missing => (None, None),
            CasCheck::Confidential => (Some("비공개".into()), None),
            CasCheck::Valid(c) => (Some(c.clone()), Some(true)),
            CasCheck::Invalid { cleaned, .. } => (Some(cleaned.clone()), Some(false)),
        }
    }
}

const CONFIDENTIAL: &[&str] = &["proprietary", "trade secret", "confidential", "비공개", "영업비밀"];
const DASHES: &[char] = &['\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}', '\u{FE63}', '\u{FF0D}'];

/// 공백 제거, 여러 대시를 '-'로 통일
pub fn clean_cas(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| if DASHES.contains(&c) { '-' } else { c })
        .collect()
}

pub fn check_cas(raw: Option<&str>) -> CasCheck {
    let raw = raw.unwrap_or("").trim();
    let lower = raw.to_lowercase();
    if CONFIDENTIAL.iter().any(|k| lower.contains(k)) {
        return CasCheck::Confidential;
    }
    let cleaned = clean_cas(raw);
    if cleaned.is_empty() || cleaned == "-" || cleaned.eq_ignore_ascii_case("n/a") {
        return CasCheck::Missing;
    }

    let parts: Vec<&str> = cleaned.split('-').collect();
    let format_ok = parts.len() == 3
        && (2..=7).contains(&parts[0].len())
        && parts[1].len() == 2
        && parts[2].len() == 1
        && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()));
    if !format_ok {
        return CasCheck::Invalid {
            reason: "형식 오류 (숫자2~7자리-숫자2자리-숫자1자리 이어야 함)".into(),
            cleaned,
        };
    }

    let body: Vec<u32> = format!("{}{}", parts[0], parts[1]).bytes().map(|b| (b - b'0') as u32).collect();
    let check = (parts[2].as_bytes()[0] - b'0') as u32;
    let sum: u32 = body.iter().rev().enumerate().map(|(i, d)| (i as u32 + 1) * d).sum();
    if sum % 10 != check {
        return CasCheck::Invalid {
            reason: format!("체크섬 오류 (계산값 {}, 마지막 자리 {check})", sum % 10),
            cleaned,
        };
    }
    CasCheck::Valid(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_cas_numbers() {
        for c in ["7732-18-5", "64-17-5", "67-64-1"] {
            assert_eq!(check_cas(Some(c)), CasCheck::Valid(c.into()), "{c}");
        }
    }

    #[test]
    fn bad_checksum() {
        assert!(matches!(check_cas(Some("7732-18-4")), CasCheck::Invalid { reason, .. } if reason.contains("체크섬")));
    }

    #[test]
    fn cleaned_then_valid() {
        // U+2010 하이픈과 공백이 섞인 값
        assert_eq!(check_cas(Some("7732 \u{2010}18\u{2010}5")), CasCheck::Valid("7732-18-5".into()));
        assert_eq!(check_cas(Some(" 64\u{2013}17\u{2014}5 ")), CasCheck::Valid("64-17-5".into()));
    }

    #[test]
    fn bad_format() {
        for c in ["7732185", "7-18-5", "77321-8-5", "12345678-18-5", "abcd-18-5"] {
            assert!(matches!(check_cas(Some(c)), CasCheck::Invalid { reason, .. } if reason.contains("형식")), "{c}");
        }
    }

    #[test]
    fn missing_and_confidential() {
        for c in [None, Some(""), Some("  "), Some("-"), Some("\u{2013}"), Some("N/A"), Some("n/a")] {
            assert_eq!(check_cas(c), CasCheck::Missing, "{c:?}");
        }
        for c in ["Proprietary", "TRADE SECRET", "Confidential business information", "비공개", "영업비밀"] {
            assert_eq!(check_cas(Some(c)), CasCheck::Confidential, "{c}");
        }
        assert_eq!(CasCheck::Confidential.db_values(), (Some("비공개".into()), None));
        assert_eq!(CasCheck::Missing.db_values(), (None, None));
    }
}
