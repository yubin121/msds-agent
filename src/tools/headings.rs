//! 섹션 제목 후보 찾기와 섹션 본문 자르기 (순수 함수)
//! 줄 번호는 페이지마다 1부터, `str::lines()` 기준이다. 두 함수가 같은 기준을 써야 한다.

/// 제목 후보 판정은 업로드 검증과 같은 규칙이라 msds-validate에 있다
pub use msds_validate::{heading_number, MIN_DISTINCT_HEADINGS};

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

/// 후보 번호 목록에서 번호가 줄어든 곳의 (인덱스, 앞 번호, 뒤 번호).
/// 1로 돌아가는 것은 다음 제품의 시작이라 정상으로 보고 넣지 않는다.
pub fn backsteps(numbers: &[u32]) -> Vec<(usize, u32, u32)> {
    numbers
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[1] < w[0] && w[1] != 1)
        .map(|(i, w)| (i + 1, w[0], w[1]))
        .collect()
}

/// 이보다 긴 섹션은 다음 제품이나 다음 섹션 제목을 빠뜨렸을 가능성이 높다
pub const LONG_SECTION_CHARS: usize = 20_000;

pub fn is_long_section(chars: usize) -> bool {
    chars > LONG_SECTION_CHARS
}

/// 위치 순서대로 놓인 (product_no, section_no) 목록을 검사한다.
/// product_no는 1부터 시작해 같거나 1씩만 늘어야 하고, (product_no, section_no)는 중복되면 안 된다.
pub fn check_products(entries: &[(i32, i32)]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    let mut prev = 0;
    for (i, &(p, s)) in entries.iter().enumerate() {
        let ok = p == prev + 1 || (i > 0 && p == prev);
        if !ok {
            return Err(if i == 0 {
                format!("첫 섹션의 product_no는 1이어야 합니다: {p}")
            } else {
                format!(
                    "sections[{i}]의 product_no {p}가 잘못되었습니다. 위치 순서대로 {prev} 또는 {}이어야 합니다",
                    prev + 1
                )
            });
        }
        if !seen.insert((p, s)) {
            return Err(format!("제품 {p}의 section_no {s}가 중복되었습니다"));
        }
        prev = p;
    }
    Ok(())
}

/// 제목 줄을 빼면 내용이 없는지
pub fn body_is_empty(section_text: &str) -> bool {
    section_text.lines().skip(1).all(|l| l.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn backsteps_detects_shuffled_order() {
        assert!(backsteps(&[1, 2, 3, 4, 5, 16]).is_empty());
        // 다음 제품(1로 돌아감)과 이어지는 페이지의 반복 제목(같은 번호)은 정상
        assert!(backsteps(&[1, 2, 16, 1, 2, 2, 3, 16, 1]).is_empty());
        // 엑셀로 만든 한글 문서의 기존 추출 순서
        let shuffled = [1, 3, 2, 5, 6, 7, 4, 8, 10, 9, 11, 12, 13, 14, 16, 15];
        assert_eq!(backsteps(&shuffled), vec![(2, 3, 2), (6, 7, 4), (9, 10, 9), (15, 16, 15)]);
        assert!(backsteps(&[]).is_empty());
        assert!(backsteps(&[3]).is_empty());
    }

    #[test]
    fn product_order_rules() {
        // 단일 제품, 3제품, 옛 양식(번호 순서와 상관없이 위치 순서만 본다)
        assert!(check_products(&[(1, 1), (1, 2), (1, 16)]).is_ok());
        assert!(check_products(&[(1, 1), (1, 16), (2, 1), (2, 16), (3, 1)]).is_ok());
        assert!(check_products(&[(1, 1), (1, 3), (1, 2)]).is_ok());
        assert!(check_products(&[]).is_ok());

        // 1부터 시작하지 않음, 건너뜀, 줄어듦
        assert!(check_products(&[(2, 1)]).unwrap_err().contains("1이어야"));
        assert!(check_products(&[(0, 1)]).unwrap_err().contains("1이어야"));
        assert!(check_products(&[(1, 1), (3, 1)]).unwrap_err().contains("1 또는 2"));
        assert!(check_products(&[(1, 1), (2, 1), (1, 2)]).unwrap_err().contains("2 또는 3"));

        // 중복은 같은 제품 안에서만
        assert!(check_products(&[(1, 2), (1, 2)]).unwrap_err().contains("중복"));
        assert!(check_products(&[(1, 2), (2, 2)]).is_ok());
    }

    #[test]
    fn long_section_threshold() {
        assert!(!is_long_section(20_000));
        assert!(is_long_section(20_001));
    }

    #[test]
    fn slice_errors() {
        let pages = vec![(1, "a\nb\nc".to_string())];
        assert!(slice_sections(&pages, &[(1, 3), (1, 2)]).unwrap_err().contains("오름차순"));
        assert!(slice_sections(&pages, &[(1, 2), (1, 2)]).unwrap_err().contains("오름차순"));
        assert!(slice_sections(&pages, &[(2, 1)]).unwrap_err().contains("없습니다"));
    }
}
