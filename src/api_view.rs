//! 조회 API 응답 조립 (순수 함수). DB 행을 받아 상세·legacy 응답 모양으로 묶는다.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use base64::Engine;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::tools::ghs::symbol_name;

/// msds_components 한 행
pub struct ComponentRow {
    pub product_no: i32,
    pub product_name: Option<String>,
    pub component: Option<String>,
    pub cas_no: Option<String>,
    pub percent: Option<String>,
    pub cas_valid: Option<bool>,
}

/// msds_hazards 한 행: (H코드, 그림문자)마다 한 행. 그림문자가 없는 H코드는 ghs_code NULL,
/// no_h_codes는 h_code·ghs_code 모두 NULL에 note만 있는 한 행
pub struct HazardRow {
    pub product_no: i32,
    pub h_code: Option<String>,
    pub ghs_code: Option<String>,
    pub source: Option<String>,
    pub signal_word: Option<String>,
    pub note: Option<String>,
}

/// 제품 목록: msds_sections가 기준이고, 섹션에 없는 product_no는 결과 테이블에서 보충한다.
/// 이름은 섹션 이름 → 성분 행의 이름 순서로 쓴다.
pub fn product_list(
    sections: &[(i32, Option<String>)],
    components: &[ComponentRow],
    hazards: &[HazardRow],
) -> Vec<(i32, Option<String>)> {
    let mut out: BTreeMap<i32, Option<String>> = sections.iter().cloned().collect();
    for c in components {
        let name = out.entry(c.product_no).or_default();
        if name.is_none() {
            *name = c.product_name.clone().filter(|n| !n.trim().is_empty());
        }
    }
    for h in hazards {
        out.entry(h.product_no).or_default();
    }
    out.into_iter().collect()
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Pictogram {
    pub ghs_code: String,
    pub symbol_name: String,
    pub h_codes: Vec<String>,
    /// document(문서에 적힌 코드) / classification(분류에서 변환)
    pub sources: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct HazardSummary {
    pub pictograms: Vec<Pictogram>,
    pub h_codes_without_pictogram: Vec<String>,
    pub signal_word: Option<String>,
    /// no_h_codes 이유
    pub note: Option<String>,
}

/// 한 제품의 msds_hazards 행을 그림문자별로 묶는다
pub fn hazard_summary<'a>(rows: impl IntoIterator<Item = &'a HazardRow>) -> HazardSummary {
    let mut pictograms: BTreeMap<String, (String, BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    let mut with_pictogram = BTreeSet::new();
    let mut without = BTreeSet::new();
    let mut out = HazardSummary::default();
    for r in rows {
        if out.signal_word.is_none() {
            out.signal_word = r.signal_word.clone().filter(|s| !s.trim().is_empty());
        }
        if out.note.is_none() {
            out.note = r.note.clone().filter(|s| !s.trim().is_empty());
        }
        match (&r.h_code, &r.ghs_code) {
            (h, Some(g)) => {
                // 이름은 저장된 symbol_name이 아니라 코드로 표에서 찾는다(이름표를 바꾸기 전에 저장된 행도 같은 이름으로)
                let entry = pictograms.entry(g.clone()).or_insert_with(|| {
                    (symbol_name(g).to_string(), BTreeSet::new(), BTreeSet::new())
                });
                if let Some(h) = h {
                    entry.1.insert(h.clone());
                    with_pictogram.insert(h.clone());
                }
                if let Some(s) = &r.source {
                    entry.2.insert(s.clone());
                }
            }
            (Some(h), None) => {
                without.insert(h.clone());
            }
            (None, None) => {}
        }
    }
    out.pictograms = pictograms
        .into_iter()
        .map(|(ghs_code, (symbol_name, h, s))| Pictogram {
            ghs_code,
            symbol_name,
            h_codes: h.into_iter().collect(),
            sources: s.into_iter().collect(),
        })
        .collect();
    // 한 H코드가 그림문자 행과 그림문자 없는 행에 함께 있을 수는 없지만, 있더라도 그림문자 쪽에만 둔다
    out.h_codes_without_pictogram = without.difference(&with_pictogram).cloned().collect();
    out
}

/// 상세 API의 제품 목록
pub fn detail_products(
    products: &[(i32, Option<String>)],
    components: &[ComponentRow],
    hazards: &[HazardRow],
) -> Vec<Value> {
    products
        .iter()
        .map(|(p, name)| {
            let comps: Vec<Value> = components
                .iter()
                .filter(|c| c.product_no == *p)
                .map(|c| {
                    json!({
                        "component": c.component,
                        "cas_no": c.cas_no,
                        "cas_valid": c.cas_valid,
                        "percent": c.percent,
                    })
                })
                .collect();
            let summary = hazard_summary(hazards.iter().filter(|h| h.product_no == *p));
            json!({
                "product_no": p,
                "product_name": name,
                "components": comps,
                "pictograms": summary.pictograms,
                "h_codes_without_pictogram": summary.h_codes_without_pictogram,
                "signal_word": summary.signal_word,
                "note": summary.note,
            })
        })
        .collect()
}

/// 번역 조각 (product_no, section_no, chunk_no 순서로 정렬된 행) → 섹션별 제목과 이어 붙인 본문
pub fn merge_translations(rows: &[(i32, i32, i32, Option<String>, Option<String>)]) -> Vec<Value> {
    let mut sections: BTreeMap<(i32, i32), (Option<String>, Vec<&str>)> = BTreeMap::new();
    for (p, s, chunk, title, text) in rows {
        let entry = sections.entry((*p, *s)).or_default();
        if *chunk == 0 {
            entry.0 = title.clone();
        }
        entry.1.push(text.as_deref().unwrap_or(""));
    }
    sections
        .into_iter()
        .map(|((p, s), (title, texts))| {
            json!({ "product_no": p, "section_no": s, "title_ko": title, "text_ko": texts.join("\n") })
        })
        .collect()
}

// ---- legacy (기존 서비스와 같은 응답) ----

/// 기존 서비스의 "비공개" 판정 값 (앞뒤 공백 무시, null은 대소문자 무시)
const LEGACY_PRIVATE: [&str; 5] = ["비공개", "영업비밀", "", "-", "null"];

pub const LEGACY_ANALYSIS_FAILED: &str = "MSDS 문서를 입력하세요.";

/// 기존 서비스의 CAS 모양 정규식 (기존 코드와 같게 양끝 \b, 체크섬은 보지 않는다)
static CAS_SHAPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d{2,7}\s*-\s*\d{2}\s*-\s*\d\b").unwrap());

#[derive(Debug, Clone, PartialEq)]
pub struct LegacyComponent {
    pub component: String,
    pub percent: String,
    pub casno: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LegacyProduct {
    pub product_no: i32,
    pub name: String,
    pub components: Vec<LegacyComponent>,
}

/// 그림문자 PNG (미국 OSHA, Public Domain. 화면과 같은 파일을 빌드 때 넣는다, 출처는 frontend/README.md)
const PICTOGRAM_PNGS: [(&str, &[u8]); 9] = [
    ("GHS01", include_bytes!("../frontend/assets/ghs/GHS01.png")),
    ("GHS02", include_bytes!("../frontend/assets/ghs/GHS02.png")),
    ("GHS03", include_bytes!("../frontend/assets/ghs/GHS03.png")),
    ("GHS04", include_bytes!("../frontend/assets/ghs/GHS04.png")),
    ("GHS05", include_bytes!("../frontend/assets/ghs/GHS05.png")),
    ("GHS06", include_bytes!("../frontend/assets/ghs/GHS06.png")),
    ("GHS07", include_bytes!("../frontend/assets/ghs/GHS07.png")),
    ("GHS08", include_bytes!("../frontend/assets/ghs/GHS08.png")),
    ("GHS09", include_bytes!("../frontend/assets/ghs/GHS09.png")),
];

/// legacy image: 그림문자 PNG를 변환 없이 base64로 (모르는 코드는 "")
fn pictogram_base64(ghs: &str) -> String {
    PICTOGRAM_PNGS
        .iter()
        .find(|(g, _)| *g == ghs)
        .map(|(_, png)| base64::engine::general_purpose::STANDARD.encode(png))
        .unwrap_or_default()
}

fn is_private(value: &str) -> bool {
    let v = value.trim();
    LEGACY_PRIVATE.iter().any(|p| v == *p || (*p == "null" && v.eq_ignore_ascii_case(p)))
}

/// 제품명: 없으면 "제품 N", 겹치면 " (2)", " (3)"…을 붙인다
pub fn unique_product_names(products: &[(i32, Option<String>)]) -> Vec<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    products
        .iter()
        .map(|(p, name)| {
            let base = name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("제품 {p}"));
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                base
            } else {
                format!("{base} ({count})")
            }
        })
        .collect()
}

pub fn legacy_products(products: &[(i32, Option<String>)], components: &[ComponentRow]) -> Vec<LegacyProduct> {
    products
        .iter()
        .zip(unique_product_names(products))
        .map(|((p, _), name)| LegacyProduct {
            product_no: *p,
            name,
            components: components
                .iter()
                .filter(|c| c.product_no == *p)
                .map(|c| LegacyComponent {
                    component: c.component.clone().unwrap_or_default(),
                    percent: c.percent.clone().unwrap_or_default(),
                    casno: c.cas_no.clone().unwrap_or_default(),
                })
                .collect(),
        })
        .collect()
}

/// 기존 서비스의 결과 종류 판정:
/// 201(기본, 혼합물·여러 제품) → 성분 있는 제품 0개면 401
/// → CAS 모양이 하나도 없거나 모든 성분의 casno·percent·component가 비공개 값이면 203 → 제품 1개·성분 1개면 200
pub fn legacy_status(products: &[LegacyProduct]) -> u16 {
    let with_components: Vec<&LegacyProduct> = products.iter().filter(|p| !p.components.is_empty()).collect();
    if with_components.is_empty() {
        return 401;
    }
    let all: Vec<&LegacyComponent> = with_components.iter().flat_map(|p| &p.components).collect();
    let no_cas_shape = !all.iter().any(|c| CAS_SHAPE.is_match(&c.casno));
    let all_private = all.iter().all(|c| is_private(&c.casno) && is_private(&c.percent) && is_private(&c.component));
    if no_cas_shape || all_private {
        return 203;
    }
    if products.len() == 1 && all.len() == 1 {
        return 200;
    }
    201
}

/// hazard_symbol: 제품마다 ghs_code 중복 없이 (msds_hazards는 GHS07 생략 규칙을 적용한 최종 결과다).
/// image는 그림문자 PNG의 base64
pub fn legacy_hazard_symbols(products: &[LegacyProduct], hazards: &[HazardRow]) -> Vec<Value> {
    let mut out = Vec::new();
    for p in products {
        let codes: BTreeSet<&str> =
            hazards.iter().filter(|h| h.product_no == p.product_no).filter_map(|h| h.ghs_code.as_deref()).collect();
        for g in codes {
            out.push(json!({
                "image": pictogram_base64(g),
                "ghs_code": g,
                "symbol_name": symbol_name(g),
                "product_name": p.name,
            }));
        }
    }
    out
}

/// done·partial 문서의 legacy 응답
pub fn legacy_body(products: &[LegacyProduct], hazards: &[HazardRow]) -> Value {
    let mut product = Map::new();
    for p in products {
        let comps: Vec<Value> = p
            .components
            .iter()
            .map(|c| json!({ "component": c.component, "percent": c.percent, "casno": c.casno }))
            .collect();
        product.insert(p.name.clone(), Value::Array(comps));
    }
    let symbols = legacy_hazard_symbols(products, hazards);
    json!({
        "status": legacy_status(products),
        "product": product,
        // 기존 서비스는 그림문자가 없으면 빈 배열이 아니라 빈 문자열을 보냈다
        "hazard_symbol": if symbols.is_empty() { Value::String(String::new()) } else { Value::Array(symbols) },
        // 번역 PDF는 만들지 않는다(번역 텍스트는 상세 API에 있다)
        "translation_mono": "",
        "translation_dual": "",
    })
}

/// failed 문서의 legacy error: 업로드 검증 실패면 그 메시지, 분석 실패면 기존 서비스 문구.
/// 분석을 한 번이라도 시작했으면(총괄 실행 기록이 있거나 재시작으로 중단) 분석 실패로 본다.
pub fn legacy_failed_error(error: Option<&str>, analysis_started: bool) -> String {
    let error = error.unwrap_or("").trim();
    if analysis_started || error == crate::db::RESTART_ERROR || error.is_empty() {
        LEGACY_ANALYSIS_FAILED.to_string()
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(product_no: i32, component: &str, cas: Option<&str>, percent: &str) -> ComponentRow {
        ComponentRow {
            product_no,
            product_name: None,
            component: Some(component.into()),
            cas_no: cas.map(str::to_string),
            percent: Some(percent.into()),
            cas_valid: None,
        }
    }

    fn haz(product_no: i32, h: Option<&str>, g: Option<&str>, source: &str) -> HazardRow {
        HazardRow {
            product_no,
            h_code: h.map(str::to_string),
            ghs_code: g.map(str::to_string),
            source: h.map(|_| source.to_string()),
            signal_word: Some("위험".into()),
            note: None,
        }
    }

    fn legacy(name: &str, comps: &[(&str, &str, &str)]) -> LegacyProduct {
        LegacyProduct {
            product_no: 1,
            name: name.into(),
            components: comps
                .iter()
                .map(|(c, p, n)| LegacyComponent { component: c.to_string(), percent: p.to_string(), casno: n.to_string() })
                .collect(),
        }
    }

    #[test]
    fn hazards_grouped_by_pictogram() {
        let rows = [
            haz(1, Some("H225"), Some("GHS02"), "document"),
            haz(1, Some("H304"), Some("GHS08"), "document"),
            haz(1, Some("H373"), Some("GHS08"), "classification"),
            haz(1, Some("H227"), None, "document"),
            haz(2, Some("H315"), Some("GHS07"), "document"),
        ];
        let s = hazard_summary(rows.iter().filter(|r| r.product_no == 1));
        assert_eq!(s.pictograms.len(), 2);
        assert_eq!(s.pictograms[1].ghs_code, "GHS08");
        assert_eq!(s.pictograms[1].symbol_name, "건강 유해성");
        assert_eq!(s.pictograms[1].h_codes, vec!["H304", "H373"]);
        assert_eq!(s.pictograms[1].sources, vec!["classification", "document"]);
        assert_eq!(s.h_codes_without_pictogram, vec!["H227"]);
        assert_eq!(s.signal_word.as_deref(), Some("위험"));
    }

    #[test]
    fn no_h_codes_row_gives_note_only() {
        let row = HazardRow {
            product_no: 1,
            h_code: None,
            ghs_code: None,
            source: None,
            signal_word: None,
            note: Some("GHS 이전 양식".into()),
        };
        let s = hazard_summary([&row]);
        assert!(s.pictograms.is_empty() && s.h_codes_without_pictogram.is_empty());
        assert_eq!(s.signal_word, None);
        assert_eq!(s.note.as_deref(), Some("GHS 이전 양식"));
    }

    #[test]
    fn products_from_sections_then_results() {
        let sections = [(1, Some("A".to_string())), (2, None)];
        let mut c = comp(2, "x", None, "");
        c.product_name = Some("B".into());
        let comps = [c, comp(3, "y", None, "")];
        let p = product_list(&sections, &comps, &[haz(4, None, None, "")]);
        assert_eq!(p, vec![(1, Some("A".into())), (2, Some("B".into())), (3, None), (4, None)]);
    }

    #[test]
    fn translations_merged_in_chunk_order() {
        let rows = [
            (1, 2, 0, Some("2. 유해성·위험성".to_string()), Some("가".to_string())),
            (1, 2, 1, None, Some("나".to_string())),
            (1, 3, 0, Some("3. 구성성분".to_string()), Some("다".to_string())),
        ];
        let v = merge_translations(&rows);
        assert_eq!(v[0]["title_ko"], "2. 유해성·위험성");
        assert_eq!(v[0]["text_ko"], "가\n나");
        assert_eq!(v[1]["section_no"], 3);
    }

    #[test]
    fn product_names_unique() {
        let products = [(1, Some("Sample Coating".to_string())), (2, None), (3, Some(" Sample Coating ".to_string())), (4, Some("".into()))];
        assert_eq!(unique_product_names(&products), vec!["Sample Coating", "제품 2", "Sample Coating (2)", "제품 4"]);
    }

    #[test]
    fn legacy_status_rules() {
        // 성분 있는 제품 0개
        assert_eq!(legacy_status(&[]), 401);
        assert_eq!(legacy_status(&[legacy("A", &[])]), 401);
        // 단일물질
        assert_eq!(legacy_status(&[legacy("A", &[("Ethanol", "100", "64-17-5")])]), 200);
        // 체크섬은 보지 않는다(틀린 번호도 모양만 맞으면 됨), 공백 섞인 모양도 인정
        assert_eq!(legacy_status(&[legacy("A", &[("x", "100", "64 - 17 - 6")])]), 200);
        // 혼합물
        assert_eq!(legacy_status(&[legacy("A", &[("a", "60", "64-17-5"), ("b", "40", "67-56-1")])]), 201);
        // 여러 제품이면 성분이 1개씩이어도 201
        let mut p2 = legacy("B", &[("b", "100", "67-56-1")]);
        p2.product_no = 2;
        assert_eq!(legacy_status(&[legacy("A", &[("a", "100", "64-17-5")]), p2]), 201);
        // CAS 모양이 하나도 없음
        assert_eq!(legacy_status(&[legacy("A", &[("a", "60", "비공개"), ("b", "40", "")])]), 203);
        // 끝의 \b 때문에 마지막 칸이 두 자리 이상이면 CAS 모양이 아니다(엑셀이 날짜로 바꾼 값 포함)
        assert_eq!(legacy_status(&[legacy("A", &[("a", "60", "64742-54-71")])]), 203);
        assert_eq!(legacy_status(&[legacy("A", &[("a", "60", "1964-02-08")])]), 203);
        assert_eq!(legacy_status(&[legacy("A", &[("a", "60", "64742-54-7")])]), 200);
        // 여러 개를 한 칸에 담은 값도 각각 경계가 있어 모양으로 센다
        assert_eq!(legacy_status(&[legacy("A", &[("a", "60", "64742-54-7, 64742-65-0")])]), 200);
        // 모든 성분의 세 값이 비공개
        assert_eq!(legacy_status(&[legacy("A", &[("영업비밀", "-", "NULL")])]), 203);
        // 하나라도 CAS 모양이 있으면 203이 아니다
        assert_eq!(legacy_status(&[legacy("A", &[("a", "60", "비공개"), ("b", "40", "7732-18-5")])]), 201);
    }

    #[test]
    fn legacy_body_shape() {
        let products = vec![legacy("Sample Lubricant", &[("Mineral oil", "60-100", "64742-54-7, 64742-65-0")])];
        let hazards = [
            haz(1, Some("H315"), Some("GHS07"), "document"),
            haz(1, Some("H319"), Some("GHS07"), "document"),
            haz(1, Some("H227"), None, "document"),
        ];
        let v = legacy_body(&products, &hazards);
        assert_eq!(v["status"], 200);
        assert_eq!(v["product"]["Sample Lubricant"][0]["casno"], "64742-54-7, 64742-65-0");
        assert!(v["product"]["Sample Lubricant"][0].get("cas_valid").is_none());
        assert_eq!(v["hazard_symbol"].as_array().unwrap().len(), 1);
        assert_eq!(v["hazard_symbol"][0]["symbol_name"], "감탄부호");
        assert_eq!(v["hazard_symbol"][0]["product_name"], "Sample Lubricant");
        // image는 GHS07.png 파일 그대로의 base64
        let image = base64::engine::general_purpose::STANDARD
            .decode(v["hazard_symbol"][0]["image"].as_str().unwrap())
            .unwrap();
        assert_eq!(image, include_bytes!("../frontend/assets/ghs/GHS07.png"));
        assert_eq!(v["translation_mono"], "");

        // 그림문자가 없으면 빈 문자열
        let v = legacy_body(&products, &[]);
        assert_eq!(v["hazard_symbol"], "");
    }

    #[test]
    fn pictogram_pngs_are_real_pngs() {
        for (g, png) in PICTOGRAM_PNGS {
            assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "{g}");
        }
        assert_eq!(pictogram_base64("GHS10"), "");
    }

    #[test]
    fn legacy_failed_messages() {
        assert_eq!(legacy_failed_error(Some("PDF 파일이 아닙니다"), false), "PDF 파일이 아닙니다");
        assert_eq!(legacy_failed_error(Some("추출: 섹션 순서가 뒤섞인 문서입니다"), true), LEGACY_ANALYSIS_FAILED);
        assert_eq!(legacy_failed_error(Some(crate::db::RESTART_ERROR), false), LEGACY_ANALYSIS_FAILED);
        assert_eq!(legacy_failed_error(None, false), LEGACY_ANALYSIS_FAILED);
    }
}
