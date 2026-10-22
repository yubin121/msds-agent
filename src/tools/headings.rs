//! 섹션 제목 후보 찾기와 섹션 본문 자르기 (순수 함수)
//! 줄 번호는 페이지마다 1부터, `str::lines()` 기준이다. 두 함수가 같은 기준을 써야 한다.

use std::sync::LazyLock;

use regex::Regex;

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)^\s*section\s*(\d{1,2})\b",       // SECTION 3, Section 3:
        r"^\s*제\s*(\d{1,2})\s*[장항]",           // 제3장
        r"^\s*(\d{1,2})\s*[.):]\s*[^\d\s.]",     // 3. Composition, 3) 구성 (3.5 같은 소수는 제외)
        r"^\s*(\d{1,2})\s+[가-힣]",              // 3 구성성분의 명칭
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

/// 줄이 섹션 제목처럼 보이면 섹션 번호(1~16)를 돌려준다.
pub fn heading_number(line: &str) -> Option<u32> {
    PATTERNS.iter().find_map(|re| {
        let n: u32 = re.captures(line)?[1].parse().ok()?;
        (1..=16).contains(&n).then_some(n)
    })
}

/// (page_no, line_no) 위치 목록으로 문서를 잘라 섹션 본문을 만든다.
/// 각 섹션은 자기 위치(제목 줄 포함)부터 다음 위치 전 줄까지, 마지막은 문서 끝까지다.
/// 위치가 없거나 오름차순이 아니면 Err.
pub fn slice_sections(pages: &[(i32, String)], starts: &[(i32, i32)]) -> Result<Vec<String>, String> {
    let lines: Vec<(i32, i32, &str)> = pages
        .iter()
        .flat_map(|(p, text)| text.lines().enumerate().map(move |(i, l)| (*p, i as i32 + 1, l)))
        .collect();

    let mut idx = Vec::with_capacity(starts.len());
    for &(p, l) in starts {
        let i = lines
            .iter()
            .position(|&(lp, ll, _)| lp == p && ll == l)
            .ok_or_else(|| format!("{p}페이지 {l}번째 줄이 없습니다"))?;
        if let Some(&prev) = idx.last() {
            if i <= prev {
                return Err(format!("위치가 오름차순이 아닙니다: {p}페이지 {l}번째 줄"));
            }
        }
        idx.push(i);
    }

    Ok(idx
        .iter()
        .enumerate()
        .map(|(k, &from)| {
            let to = idx.get(k + 1).copied().unwrap_or(lines.len());
            lines[from..to].iter().map(|(_, _, l)| *l).collect::<Vec<_>>().join("\n")
        })
        .collect())
}

/// 제목 줄을 빼면 내용이 없는지
pub fn body_is_empty(section_text: &str) -> bool {
    section_text.lines().skip(1).all(|l| l.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_patterns() {
        assert_eq!(heading_number("SECTION 3"), Some(3));
        assert_eq!(heading_number("Section 3: Composition"), Some(3));
        assert_eq!(heading_number("  section 16 Other information"), Some(16));
        assert_eq!(heading_number("3. Composition/information on ingredients"), Some(3));
        assert_eq!(heading_number("3. 구성성분의 명칭 및 함유량"), Some(3));
        assert_eq!(heading_number("11) 독성에 관한 정보"), Some(11));
        assert_eq!(heading_number("1 화학제품과 회사에 관한 정보"), Some(1));
        assert_eq!(heading_number("제2장 유해성·위험성"), Some(2));
    }

    #[test]
    fn non_headings() {
        assert_eq!(heading_number("3.5 mg/kg"), None);
        assert_eq!(heading_number("17. Extra"), None);
        assert_eq!(heading_number("0. Zero"), None);
        assert_eq!(heading_number("Sectional view"), None);
        assert_eq!(heading_number("Composition"), None);
        assert_eq!(heading_number("2024. 01. 05"), None);
    }

    #[test]
    fn slices_across_pages() {
        let pages = vec![
            (1, "표지\n1. 제품명\n물\n2. 유해성".to_string()),
            (2, "계속되는 2번\n3. 구성성분\n물 100%".to_string()),
        ];
        let out = slice_sections(&pages, &[(1, 2), (1, 4), (2, 2)]).unwrap();
        assert_eq!(out, vec!["1. 제품명\n물", "2. 유해성\n계속되는 2번", "3. 구성성분\n물 100%"]);
        assert!(!body_is_empty(&out[0]));
        assert!(body_is_empty("4. 응급조치\n  \n"));
    }

    #[test]
    fn slice_errors() {
        let pages = vec![(1, "a\nb\nc".to_string())];
        assert!(slice_sections(&pages, &[(1, 3), (1, 2)]).unwrap_err().contains("오름차순"));
        assert!(slice_sections(&pages, &[(1, 2), (1, 2)]).unwrap_err().contains("오름차순"));
        assert!(slice_sections(&pages, &[(2, 1)]).unwrap_err().contains("없습니다"));
    }
}
