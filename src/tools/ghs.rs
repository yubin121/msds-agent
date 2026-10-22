//! H코드 → GHS 그림문자 변환 (순수 함수)

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

pub const PICTOGRAMS: &[(&str, &str)] = &[
    ("GHS01", "폭발성(폭탄)"),
    ("GHS02", "인화성(불꽃)"),
    ("GHS03", "산화성(원 위의 불꽃)"),
    ("GHS04", "고압가스(가스 실린더)"),
    ("GHS05", "부식성"),
    ("GHS06", "급성독성(해골과 X자 뼈)"),
    ("GHS07", "경고(느낌표)"),
    ("GHS08", "건강유해성"),
    ("GHS09", "수생환경유해성"),
];

/// 그림문자가 없는 H코드
const NO_PICTOGRAM: &[u16] = &[227, 229, 230, 231, 303, 305, 313, 316, 320, 333, 362, 401, 402, 412, 413, 433];

pub fn symbol_name(ghs: &str) -> &'static str {
    PICTOGRAMS.iter().find(|(c, _)| *c == ghs).map(|(_, n)| *n).unwrap_or("")
}

/// H코드 번호(예: 301)에 붙는 그림문자. 정렬된 순서로 돌려준다.
pub fn pictograms_for(h: u16) -> Vec<&'static str> {
    let mut out = Vec::new();
    if matches!(h, 200..=205 | 240 | 241) { out.push("GHS01"); }
    if matches!(h, 206..=208 | 220..=226 | 228 | 232 | 241 | 242 | 250..=252 | 260 | 261) { out.push("GHS02"); }
    if matches!(h, 270..=272) { out.push("GHS03"); }
    if matches!(h, 280 | 281) { out.push("GHS04"); }
    if matches!(h, 290 | 314 | 318) { out.push("GHS05"); }
    if matches!(h, 300 | 301 | 310 | 311 | 330 | 331) { out.push("GHS06"); }
    if matches!(h, 302 | 312 | 315 | 317 | 319 | 332 | 335 | 336 | 420) { out.push("GHS07"); }
    if matches!(h, 304 | 334 | 340 | 341 | 350 | 351 | 360 | 361 | 370..=373) { out.push("GHS08"); }
    if matches!(h, 400 | 410 | 411) { out.push("GHS09"); }
    out
}

static H_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[Hh](\d{3})([A-Za-z]{0,3})$").unwrap());

/// 한 H코드의 최종 판정
#[derive(Debug, Clone, PartialEq)]
pub struct CodeResult {
    pub code: String,
    /// 입력에서 이 코드가 나온 원문 (예: "H301+H311")
    pub source: String,
    /// 우선순위 적용 후 남은 그림문자
    pub pictograms: Vec<&'static str>,
}

#[derive(Debug, Default, PartialEq)]
pub struct GhsResult {
    /// 인식한 H코드 (중복 제거, 입력 순서)
    pub codes: Vec<CodeResult>,
    /// 그림문자 → 근거 H코드
    pub pictograms: BTreeMap<&'static str, Vec<String>>,
    pub no_pictogram: Vec<String>,
    pub unknown_codes: Vec<String>,
    /// (H코드, 이유): 우선순위 때문에 GHS07을 뺀 코드
    pub suppressed: Vec<(String, String)>,
}

pub fn classify(inputs: &[String]) -> GhsResult {
    let mut res = GhsResult::default();
    let mut parsed: Vec<(String, u16, String)> = Vec::new(); // (code, number, source)

    for raw in inputs {
        let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        for part in compact.split(['+', ',', ';', '/']).filter(|p| !p.is_empty()) {
            if part.to_ascii_uppercase().starts_with("EUH") {
                push_unique(&mut res.unknown_codes, part.to_string());
                continue;
            }
            let Some(caps) = H_CODE.captures(part) else {
                push_unique(&mut res.unknown_codes, part.to_string());
                continue;
            };
            let num: u16 = caps[1].parse().unwrap();
            let code = format!("H{}{}", &caps[1], &caps[2]);
            let known = !pictograms_for(num).is_empty() || NO_PICTOGRAM.contains(&num);
            if !known {
                push_unique(&mut res.unknown_codes, code);
            } else if !parsed.iter().any(|(c, _, _)| *c == code) {
                parsed.push((code, num, raw.trim().to_string()));
            }
        }
    }

    let all: Vec<&str> = parsed.iter().flat_map(|(_, n, _)| pictograms_for(*n)).collect();
    let has06 = all.contains(&"GHS06");
    let has05 = all.contains(&"GHS05");
    let has334 = parsed.iter().any(|(_, n, _)| *n == 334);

    for (code, num, source) in parsed {
        let mut pics = pictograms_for(num);
        if pics.is_empty() {
            res.no_pictogram.push(code.clone());
        }
        if pics.contains(&"GHS07") {
            let reason = if has06 && matches!(num, 302 | 312 | 332) {
                Some("GHS06이 있어 급성독성에 따른 GHS07 생략")
            } else if has05 && matches!(num, 315 | 319) {
                Some("GHS05가 있어 피부·눈 자극에 따른 GHS07 생략")
            } else if has334 && matches!(num, 315 | 317 | 319) {
                Some("GHS08(H334)이 있어 피부 과민·자극에 따른 GHS07 생략")
            } else {
                None
            };
            if let Some(r) = reason {
                pics.retain(|p| *p != "GHS07");
                res.suppressed.push((code.clone(), r.to_string()));
            }
        }
        for p in &pics {
            res.pictograms.entry(p).or_default().push(code.clone());
        }
        res.codes.push(CodeResult { code, source, pictograms: pics });
    }
    res
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pics(inputs: &[&str]) -> Vec<&'static str> {
        let v: Vec<String> = inputs.iter().map(|s| s.to_string()).collect();
        classify(&v).pictograms.keys().copied().collect()
    }

    #[test]
    fn basic_mapping() {
        assert_eq!(pictograms_for(225), vec!["GHS02"]);
        assert_eq!(pictograms_for(241), vec!["GHS01", "GHS02"]);
        assert_eq!(pictograms_for(372), vec!["GHS08"]);
        assert!(pictograms_for(412).is_empty());
        assert_eq!(pics(&["H225", "H319", "H336"]), vec!["GHS02", "GHS07"]);
    }

    #[test]
    fn combined_codes_split_and_euh_ignored() {
        let r = classify(&["H301+H311".into(), "h331".into(), "EUH066".into(), "H999".into(), "H301".into()]);
        let codes: Vec<&str> = r.codes.iter().map(|c| c.code.as_str()).collect();
        assert_eq!(codes, vec!["H301", "H311", "H331"]);
        assert_eq!(r.codes[0].source, "H301+H311");
        assert_eq!(r.unknown_codes, vec!["EUH066", "H999"]);
        assert_eq!(r.pictograms.keys().copied().collect::<Vec<_>>(), vec!["GHS06"]);
    }

    #[test]
    fn suffix_codes() {
        let r = classify(&["H360Fd".into(), "H350i".into()]);
        assert_eq!(r.codes[0].code, "H360Fd");
        assert_eq!(r.pictograms["GHS08"], vec!["H360Fd", "H350i"]);
    }

    #[test]
    fn no_pictogram_codes() {
        let r = classify(&["H227".into(), "H412".into(), "H229".into()]);
        assert!(r.pictograms.is_empty());
        assert_eq!(r.no_pictogram, vec!["H227", "H412", "H229"]);
    }

    #[test]
    fn ghs06_suppresses_acute_tox_ghs07_only() {
        // H302(급성독성)의 GHS07은 빠지고 H335(호흡기 자극)의 GHS07은 남는다
        assert_eq!(pics(&["H301", "H302"]), vec!["GHS06"]);
        let r = classify(&["H301".into(), "H302".into(), "H335".into()]);
        assert_eq!(r.pictograms["GHS07"], vec!["H335"]);
        assert_eq!(r.suppressed.len(), 1);
        assert_eq!(r.suppressed[0].0, "H302");
    }

    #[test]
    fn ghs05_suppresses_irritation_ghs07() {
        assert_eq!(pics(&["H314", "H315", "H319"]), vec!["GHS05"]);
        assert_eq!(pics(&["H318", "H319", "H336"]), vec!["GHS05", "GHS07"]);
        // GHS05가 있어도 H317(피부 과민)의 GHS07은 남는다
        assert_eq!(pics(&["H314", "H317"]), vec!["GHS05", "GHS07"]);
    }

    #[test]
    fn h334_suppresses_skin_ghs07() {
        assert_eq!(pics(&["H334", "H317", "H315", "H319"]), vec!["GHS08"]);
        // H334가 아닌 GHS08(H350)은 GHS07을 빼지 않는다
        assert_eq!(pics(&["H350", "H317"]), vec!["GHS07", "GHS08"]);
        assert_eq!(pics(&["H334", "H332"]), vec!["GHS07", "GHS08"]);
    }

    #[test]
    fn without_ghs06_acute_tox_keeps_ghs07() {
        assert_eq!(pics(&["H302", "H312"]), vec!["GHS07"]);
    }
}
