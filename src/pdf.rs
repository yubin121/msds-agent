/// PDF 바이트에서 페이지별 텍스트를 뽑는다.
/// pdf_extract는 동기 함수이고 일부 PDF에서 panic을 내므로
/// spawn_blocking 안에서 실행하고 panic은 Err로 바꾼다.
pub async fn extract_pages(bytes: Vec<u8>) -> Result<Vec<String>, String> {
    let joined = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pdf_extract::extract_text_from_mem_by_pages(&bytes)
        }))
    })
    .await;

    match joined {
        Ok(Ok(Ok(pages))) => Ok(pages),
        Ok(Ok(Err(e))) => Err(format!("PDF 텍스트 추출 실패: {e}")),
        Ok(Err(panic)) => Err(format!("PDF 텍스트 추출 중 panic: {}", panic_message(&panic))),
        Err(e) => Err(format!("추출 작업 실행 실패: {e}")),
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "알 수 없는 panic".to_string()
    }
}

/// 한글 글자가 전체 글자(공백·숫자·기호 제외)의 20% 이상이면 "ko", 아니면 "en".
pub fn detect_language(text: &str) -> &'static str {
    let mut letters = 0usize;
    let mut hangul = 0usize;
    for c in text.chars().filter(|c| c.is_alphabetic()) {
        letters += 1;
        if is_hangul(c) {
            hangul += 1;
        }
    }
    if letters > 0 && hangul * 5 >= letters {
        "ko"
    } else {
        "en"
    }
}

fn is_hangul(c: char) -> bool {
    matches!(c,
        '\u{AC00}'..='\u{D7A3}'   // 완성형 음절
        | '\u{1100}'..='\u{11FF}' // 자모
        | '\u{3130}'..='\u{318F}' // 호환 자모
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_korean_and_english() {
        assert_eq!(detect_language("물질안전보건자료 MSDS"), "ko");
        assert_eq!(detect_language("Safety Data Sheet 1. Identification"), "en");
        assert_eq!(detect_language(""), "en");
    }

    #[tokio::test]
    async fn invalid_pdf_returns_error() {
        assert!(extract_pages(b"not a pdf".to_vec()).await.is_err());
    }
}
