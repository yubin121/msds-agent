//! CAS 번호 정리·검증 (순수 함수)

#[derive(Debug, Clone, PartialEq)]
pub enum CasCheck {
    /// 빈 값, "-", "N/A", "없음" 등 → cas_no, cas_valid 모두 NULL
    Missing,
    /// 영업비밀 등 → cas_no "비공개", cas_valid NULL
    Confidential,
    /// 형식·체크섬 통과
    Valid(String),
    /// 정리한 값과 실패 이유. cas_valid = false로 저장한다
    Invalid { cleaned: String, reason: String },
}

const CONFIDENTIAL: &[&str] = &["proprietary", "trade secret", "confidential", "비공개", "영업비밀"];
/// CAS가 없다는 뜻의 값. 소문자로 바꾸고 공백을 뺀 뒤 비교한다 ("해당 없음" = "해당없음")
const MISSING: &[&str] = &[
    "", "-", "n/a", "없음", "해당없음", "자료없음", "none", "notavailable", "notapplicable", "n.a.", "na",
];
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
    if MISSING.contains(&cleaned.to_lowercase().as_str()) {
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

/// 한 칸에 적힌 CAS(쉼표·세미콜론으로 여러 개 가능)를 검사한 결과
#[derive(Debug, Clone, PartialEq)]
pub struct CasField {
    /// "64742-54-7, 64742-65-0"처럼 정리한 값을 ", "로 이은 것
    pub cas_no: Option<String>,
    /// 번호가 하나라도 틀리면 false, 모두 맞으면 true, 검사할 번호가 없으면(빈 값·비공개) NULL
    pub cas_valid: Option<bool>,
    /// 틀린 번호만 (정리한 값, 이유)
    pub invalid: Vec<(String, String)>,
}

pub fn check_cas_field(raw: Option<&str>) -> CasField {
    // "Trade secret, see note"처럼 비공개 문구 안의 쉼표로 나누지 않는다
    if check_cas(raw) == CasCheck::Confidential {
        return CasField { cas_no: Some("비공개".into()), cas_valid: None, invalid: vec![] };
    }
    let checks: Vec<CasCheck> = raw
        .unwrap_or("")
        .split([',', ';'])
        .map(|part| check_cas(Some(part)))
        .filter(|c| *c != CasCheck::Missing)
        .collect();

    let mut values = Vec::new();
    let mut invalid = Vec::new();
    let mut any_valid = false;
    for c in &checks {
        match c {
            CasCheck::Missing => {}
            CasCheck::Confidential => values.push("비공개".to_string()),
            CasCheck::Valid(v) => {
                any_valid = true;
                values.push(v.clone());
            }
            CasCheck::Invalid { cleaned, reason } => {
                invalid.push((cleaned.clone(), reason.clone()));
                values.push(cleaned.clone());
            }
        }
    }
    values.dedup();
    let cas_valid = if !invalid.is_empty() {
        Some(false)
    } else if any_valid {
        Some(true)
    } else {
        None
    };
    CasField { cas_no: (!values.is_empty()).then(|| values.join(", ")), cas_valid, invalid }
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
    }

    #[test]
    fn missing_words() {
        for c in [
            "없음", "해당없음", "해당 없음", " 자료 없음 ", "None", "NONE", "Not available", "not  applicable",
            "N.A.", "n.a.", "NA", "na",
        ] {
            assert_eq!(check_cas(Some(c)), CasCheck::Missing, "{c}");
            assert_eq!(check_cas_field(Some(c)), CasField { cas_no: None, cas_valid: None, invalid: vec![] }, "{c}");
        }
        // 엑셀이 날짜로 바꾼 값은 빈 값이 아니라 오류다
        assert!(matches!(check_cas(Some("1964-02-08")), CasCheck::Invalid { .. }));
    }

    #[test]
    fn several_cas_in_one_cell() {
        let f = check_cas_field(Some("64742-54-7, 64742-65-0"));
        assert_eq!(f.cas_no.as_deref(), Some("64742-54-7, 64742-65-0"));
        assert_eq!(f.cas_valid, Some(true));
        assert!(f.invalid.is_empty());

        // 세미콜론, 공백 없음, 빈 조각
        let f = check_cas_field(Some("7732-18-5;64-17-5;"));
        assert_eq!(f.cas_no.as_deref(), Some("7732-18-5, 64-17-5"));
        assert_eq!(f.cas_valid, Some(true));

        // 하나라도 틀리면 false, invalid에는 틀린 것만
        let f = check_cas_field(Some("7732-18-5, 7732-18-4"));
        assert_eq!(f.cas_no.as_deref(), Some("7732-18-5, 7732-18-4"));
        assert_eq!(f.cas_valid, Some(false));
        assert_eq!(f.invalid.len(), 1);
        assert_eq!(f.invalid[0].0, "7732-18-4");
        assert!(f.invalid[0].1.contains("체크섬"));
    }

    #[test]
    fn single_cas_field_matches_check_cas() {
        assert_eq!(
            check_cas_field(Some("7732-18-5")),
            CasField { cas_no: Some("7732-18-5".into()), cas_valid: Some(true), invalid: vec![] }
        );
        let f = check_cas_field(Some("1964-02-08"));
        assert_eq!((f.cas_no.as_deref(), f.cas_valid), (Some("1964-02-08"), Some(false)));
        assert_eq!(
            check_cas_field(Some("Proprietary")),
            CasField { cas_no: Some("비공개".into()), cas_valid: None, invalid: vec![] }
        );
        assert_eq!(
            check_cas_field(Some("Trade secret, see section 16")),
            CasField { cas_no: Some("비공개".into()), cas_valid: None, invalid: vec![] }
        );
        assert_eq!(check_cas_field(None), CasField { cas_no: None, cas_valid: None, invalid: vec![] });
    }
}
