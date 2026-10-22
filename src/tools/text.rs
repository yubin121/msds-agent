//! 문자열 유틸. 자르기는 모두 문자 경계에서 한다.

/// 앞에서 최대 `max_chars`글자만 남긴다. 잘렸으면 true.
pub fn truncate_chars(s: &str, max_chars: usize) -> (&str, bool) {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => (&s[..i], true),
        None => (s, false),
    }
}

/// `offset`글자부터 최대 `max_chars`글자. offset이 글자 수 이상이면 빈 문자열.
pub fn char_window(s: &str, offset: usize, max_chars: usize) -> &str {
    let start = match s.char_indices().nth(offset) {
        Some((i, _)) => i,
        None => return "",
    };
    truncate_chars(&s[start..], max_chars).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_korean_on_char_boundary() {
        assert_eq!(truncate_chars("물질안전보건자료", 4), ("물질안전", true));
        assert_eq!(truncate_chars("물질안전", 4), ("물질안전", false));
        assert_eq!(truncate_chars("가®나", 2), ("가®", true));
        assert_eq!(truncate_chars("", 3), ("", false));
    }

    #[test]
    fn window_korean() {
        let s = "가나다라마바사";
        assert_eq!(char_window(s, 2, 3), "다라마");
        assert_eq!(char_window(s, 5, 10), "바사");
        assert_eq!(char_window(s, 7, 10), "");
        assert_eq!(char_window(s, 100, 10), "");
    }
}
