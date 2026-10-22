//! GHS 유해성 분류(구분) → H코드 변환 (순수 함수)

use std::sync::LazyLock;

use regex::Regex;

/// 입력 스키마 enum에 쓰는 유해성 분류 목록
pub const HAZARD_CLASSES: &[&str] = &[
    "explosive",
    "flammable_gas",
    "aerosol",
    "oxidizing_gas",
    "gas_under_pressure",
    "flammable_liquid",
    "flammable_solid",
    "self_reactive",
    "organic_peroxide",
    "pyrophoric_liquid",
    "pyrophoric_solid",
    "self_heating",
    "water_reactive",
    "oxidizing_liquid",
    "oxidizing_solid",
    "corrosive_to_metals",
    "acute_toxicity_oral",
    "acute_toxicity_dermal",
    "acute_toxicity_inhalation",
    "skin_corrosion_irritation",
    "eye_damage_irritation",
    "respiratory_sensitization",
    "skin_sensitization",
    "germ_cell_mutagenicity",
    "carcinogenicity",
    "reproductive_toxicity",
    "stot_single",
    "stot_repeated",
    "aspiration",
    "aquatic_acute",
    "aquatic_chronic",
    "ozone",
];

/// "구분 1", "Category 1B", "Cat. 2A", "Type B", "1.1", "압축가스", "3 (호흡기 자극)" 등을
/// 표의 키("1", "1B", "2A", "B", "1.1", "compressed", "3R" …)로 정리한다. 알아볼 수 없으면 None.
pub fn normalize_category(hazard_class: &str, raw: &str) -> Option<String> {
    static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\s*(category|cat\.?|구분|division|div\.?|type|유형|형식)\s*").unwrap()
    });
    static CODE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(\d(?:\.\d)?)\s*([A-Ca-c])?(?:$|[^A-Za-z])|^([A-Ga-g])(?:$|[^A-Za-z])").unwrap());

    let lower = raw.trim().to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));

    // 숫자 없이 뜻으로 적는 구분
    if has(&["unstable", "불안정"]) {
        return Some("unstable".into());
    }
    if has(&["refrigerated", "냉동"]) {
        return Some("refrigerated".into());
    }
    if has(&["compressed", "압축"]) {
        return Some("compressed".into());
    }
    if has(&["dissolved", "용해"]) {
        return Some("dissolved".into());
    }
    if has(&["liquefied", "액화"]) {
        return Some("liquefied".into());
    }
    if has(&["lactation", "수유"]) {
        return Some("lactation".into());
    }

    let rest = PREFIX.replace(&lower, "");
    let caps = CODE.captures(rest.trim_start())?;
    let key = match (caps.get(1), caps.get(2), caps.get(3)) {
        (Some(n), letter, _) => format!("{}{}", n.as_str(), letter.map(|l| l.as_str().to_uppercase()).unwrap_or_default()),
        (None, _, Some(t)) => t.as_str().to_uppercase(),
        _ => return None,
    };

    // 특정표적장기 1회 노출 구분 3은 호흡기 자극(H335)과 마취(H336)를 문구로 나눈다
    if hazard_class == "stot_single" && key == "3" {
        if has(&["respiratory", "호흡기"]) {
            return Some("3R".into());
        }
        if has(&["narcotic", "drowsiness", "dizziness", "마취", "졸음"]) {
            return Some("3N".into());
        }
    }
    Some(key)
}

/// (분류, 정리된 구분) → H코드. 표에 없으면 None.
pub fn h_code_for(hazard_class: &str, key: &str) -> Option<&'static str> {
    let k = key;
    Some(match hazard_class {
        "explosive" => match k {
            "unstable" => "H200",
            "1.1" => "H201",
            "1.2" => "H202",
            "1.3" => "H203",
            "1.4" => "H204",
            "1.5" => "H205",
            _ => return None,
        },
        "flammable_gas" => match k {
            "1" | "1A" | "1B" => "H220",
            "2" => "H221",
            _ => return None,
        },
        "aerosol" => match k {
            "1" => "H222",
            "2" => "H223",
            "3" => "H229",
            _ => return None,
        },
        "oxidizing_gas" => match k {
            "1" => "H270",
            _ => return None,
        },
        "gas_under_pressure" => match k {
            "compressed" | "liquefied" | "dissolved" => "H280",
            "refrigerated" => "H281",
            _ => return None,
        },
        "flammable_liquid" => match k {
            "1" => "H224",
            "2" => "H225",
            "3" => "H226",
            "4" => "H227",
            _ => return None,
        },
        "flammable_solid" => match k {
            "1" | "2" => "H228",
            _ => return None,
        },
        "self_reactive" | "organic_peroxide" => match k {
            "A" => "H240",
            "B" => "H241",
            "C" | "D" | "E" | "F" => "H242",
            _ => return None,
        },
        "pyrophoric_liquid" | "pyrophoric_solid" => match k {
            "1" => "H250",
            _ => return None,
        },
        "self_heating" => match k {
            "1" => "H251",
            "2" => "H252",
            _ => return None,
        },
        "water_reactive" => match k {
            "1" => "H260",
            "2" | "3" => "H261",
            _ => return None,
        },
        "oxidizing_liquid" | "oxidizing_solid" => match k {
            "1" => "H271",
            "2" | "3" => "H272",
            _ => return None,
        },
        "corrosive_to_metals" => match k {
            "1" => "H290",
            _ => return None,
        },
        "acute_toxicity_oral" => match k {
            "1" | "2" => "H300",
            "3" => "H301",
            "4" => "H302",
            "5" => "H303",
            _ => return None,
        },
        "acute_toxicity_dermal" => match k {
            "1" | "2" => "H310",
            "3" => "H311",
            "4" => "H312",
            "5" => "H313",
            _ => return None,
        },
        "acute_toxicity_inhalation" => match k {
            "1" | "2" => "H330",
            "3" => "H331",
            "4" => "H332",
            "5" => "H333",
            _ => return None,
        },
        "skin_corrosion_irritation" => match k {
            "1" | "1A" | "1B" | "1C" => "H314",
            "2" => "H315",
            "3" => "H316",
            _ => return None,
        },
        "eye_damage_irritation" => match k {
            "1" => "H318",
            "2" | "2A" => "H319",
            "2B" => "H320",
            _ => return None,
        },
        "respiratory_sensitization" => match k {
            "1" | "1A" | "1B" => "H334",
            _ => return None,
        },
        "skin_sensitization" => match k {
            "1" | "1A" | "1B" => "H317",
            _ => return None,
        },
        "germ_cell_mutagenicity" => match k {
            "1" | "1A" | "1B" => "H340",
            "2" => "H341",
            _ => return None,
        },
        "carcinogenicity" => match k {
            "1" | "1A" | "1B" => "H350",
            "2" => "H351",
            _ => return None,
        },
        "reproductive_toxicity" => match k {
            "1" | "1A" | "1B" => "H360",
            "2" => "H361",
            "lactation" => "H362",
            _ => return None,
        },
        "stot_single" => match k {
            "1" => "H370",
            "2" => "H371",
            "3R" => "H335",
            "3N" => "H336",
            _ => return None,
        },
        "stot_repeated" => match k {
            "1" => "H372",
            "2" => "H373",
            _ => return None,
        },
        "aspiration" => match k {
            "1" => "H304",
            "2" => "H305",
            _ => return None,
        },
        "aquatic_acute" => match k {
            "1" => "H400",
            "2" => "H401",
            "3" => "H402",
            _ => return None,
        },
        "aquatic_chronic" => match k {
            "1" => "H410",
            "2" => "H411",
            "3" => "H412",
            "4" => "H413",
            _ => return None,
        },
        "ozone" => match k {
            "1" => "H420",
            _ => return None,
        },
        _ => return None,
    })
}

/// 분류 하나를 H코드로. 실패하면 모델이 고칠 수 있는 이유를 돌려준다.
pub fn convert(hazard_class: &str, category: &str) -> Result<&'static str, String> {
    if !HAZARD_CLASSES.contains(&hazard_class) {
        return Err(format!("알 수 없는 hazard_class: {hazard_class}"));
    }
    let Some(key) = normalize_category(hazard_class, category) else {
        return Err(format!("구분을 알아볼 수 없습니다: {category}"));
    };
    h_code_for(hazard_class, &key).ok_or_else(|| {
        if hazard_class == "stot_single" && key == "3" {
            "stot_single 구분 3은 '3 (호흡기 자극)' 또는 '3 (마취)'처럼 영향을 함께 적어라".to_string()
        } else {
            format!("{hazard_class}에 구분 {key}가 없습니다")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_spellings() {
        assert_eq!(normalize_category("x", "구분 1").as_deref(), Some("1"));
        assert_eq!(normalize_category("x", "구분1A").as_deref(), Some("1A"));
        assert_eq!(normalize_category("x", "Category 1B").as_deref(), Some("1B"));
        assert_eq!(normalize_category("x", "Cat. 2A").as_deref(), Some("2A"));
        assert_eq!(normalize_category("x", "cat 2a").as_deref(), Some("2A"));
        assert_eq!(normalize_category("x", "1B").as_deref(), Some("1B"));
        assert_eq!(normalize_category("x", "Type B").as_deref(), Some("B"));
        assert_eq!(normalize_category("x", "Division 1.4").as_deref(), Some("1.4"));
        assert_eq!(normalize_category("x", "Category 3 (aquatic)").as_deref(), Some("3"));
        assert_eq!(normalize_category("x", "압축가스").as_deref(), Some("compressed"));
        assert_eq!(normalize_category("x", "Refrigerated liquefied gas").as_deref(), Some("refrigerated"));
        assert_eq!(normalize_category("x", "Unstable explosive").as_deref(), Some("unstable"));
        assert_eq!(normalize_category("x", "수유독성").as_deref(), Some("lactation"));
        assert_eq!(normalize_category("stot_single", "구분 3(호흡기 자극)").as_deref(), Some("3R"));
        assert_eq!(normalize_category("stot_single", "Category 3 (narcotic effects)").as_deref(), Some("3N"));
        assert_eq!(normalize_category("x", "Not classified"), None);
        assert_eq!(normalize_category("x", ""), None);
    }

    #[test]
    fn table_conversions() {
        let cases = [
            ("explosive", "1.1", "H201"),
            ("flammable_gas", "1A", "H220"),
            ("aerosol", "3", "H229"),
            ("gas_under_pressure", "liquefied gas", "H280"),
            ("gas_under_pressure", "냉동액화가스", "H281"),
            ("flammable_liquid", "구분 2", "H225"),
            ("organic_peroxide", "Type D", "H242"),
            ("water_reactive", "3", "H261"),
            ("oxidizing_solid", "2", "H272"),
            ("acute_toxicity_oral", "Category 4", "H302"),
            ("acute_toxicity_inhalation", "2", "H330"),
            ("skin_corrosion_irritation", "1B", "H314"),
            ("skin_corrosion_irritation", "2", "H315"),
            ("eye_damage_irritation", "1", "H318"),
            ("eye_damage_irritation", "2A", "H319"),
            ("eye_damage_irritation", "2B", "H320"),
            ("skin_sensitization", "1", "H317"),
            ("reproductive_toxicity", "Lactation", "H362"),
            ("stot_single", "3 (respiratory tract irritation)", "H335"),
            ("stot_single", "구분 3(마취작용)", "H336"),
            ("stot_repeated", "1", "H372"),
            ("aspiration", "1", "H304"),
            ("aquatic_acute", "3", "H402"),
            ("aquatic_chronic", "Category 3", "H412"),
            ("ozone", "1", "H420"),
        ];
        for (class, cat, want) in cases {
            assert_eq!(convert(class, cat), Ok(want), "{class} {cat}");
        }
    }

    #[test]
    fn unknown_combinations() {
        assert!(convert("skin_corrosion_irritation", "4").unwrap_err().contains("없습니다"));
        assert!(convert("eye_damage_irritation", "1A").is_err());
        assert!(convert("self_reactive", "G").is_err());
        assert!(convert("stot_single", "3").unwrap_err().contains("호흡기 자극"));
        assert!(convert("toxic", "1").unwrap_err().contains("hazard_class"));
        assert!(convert("carcinogenicity", "Not classified").unwrap_err().contains("알아볼 수 없습니다"));
    }

    #[test]
    fn every_class_has_a_row() {
        for c in HAZARD_CLASSES {
            let any = ["1", "1.1", "A", "compressed", "unstable"].iter().any(|k| h_code_for(c, k).is_some());
            assert!(any, "{c}");
        }
    }
}
