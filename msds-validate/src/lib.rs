//! 업로드 입력 검증 (순수 함수, DB·IO 없음). 서버(네이티브)와 Cloudflare Worker(WASM)가 같은 코드를 쓴다.
//!
//! 분석 실패 후 재시도하는 대신 입력 단계에서 걸러 낸다(10/26 소장님 피드백).
//! 검사 순서: %PDF- 시그니처 → 크기 20MB 이하 → 공백 제외 텍스트 200자 이상 → 서로 다른 섹션 번호 8개 이상
//! - Worker는 validate_file(시그니처·크기)만 실행한다. 텍스트 검사는 PDF 텍스트 추출이 필요해 서버에서만 실행한다

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;
/// 업로드 요청 본문 한도: 파일 한도에 multipart 헤더 여유를 더한다
/// (20MB를 조금 넘는 파일은 413이 아니라 validate_file의 크기 오류로 거르게)
pub const MAX_BODY_BYTES: usize = 21 * 1024 * 1024;
/// 공백을 뺀 글자 수 합이 이보다 적으면 텍스트가 없는 PDF로 본다
pub const MIN_TEXT_CHARS: usize = 200;
pub const NO_TEXT_ERROR: &str = "텍스트가 없는 PDF입니다(스캔본일 수 있음)";
/// 제목 후보의 서로 다른 번호가 이보다 적으면 MSDS가 아닌 것으로 본다 (업로드 검증·추출 사전 검사 공용)
pub const MIN_DISTINCT_HEADINGS: usize = 8;

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

/// 공백을 뺀 글자 수 합
pub fn non_space_chars(pages: &[String]) -> usize {
    pages.iter().map(|p| p.chars().filter(|c| !c.is_whitespace()).count()).sum()
}

#[derive(Debug, Clone, PartialEq)]
pub enum UploadError {
    NotPdf,
    TooLarge { bytes: usize },
    NoText,
    NotMsds { distinct: usize },
}

impl UploadError {
    /// 사용자에게 보여 주고 msds_documents.error에 저장하는 메시지
    pub fn message(&self) -> String {
        match self {
            UploadError::NotPdf => "PDF 파일이 아닙니다".into(),
            UploadError::TooLarge { bytes } => {
                format!("파일이 20MB를 넘습니다({:.1}MB)", *bytes as f64 / (1024.0 * 1024.0))
            }
            UploadError::NoText => NO_TEXT_ERROR.into(),
            UploadError::NotMsds { .. } => "MSDS 문서가 아닌 것으로 보입니다".into(),
        }
    }
}

/// 텍스트 추출 전에 할 수 있는 검사: 시그니처, 크기 (Worker와 서버가 실행)
pub fn validate_file(bytes: &[u8]) -> Result<(), UploadError> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(UploadError::NotPdf);
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(UploadError::TooLarge { bytes: bytes.len() });
    }
    Ok(())
}

/// 본문을 읽기 전에 Content-Length로 하는 크기 검사. 헤더가 없거나 숫자가 아니면 본문 검사(validate_file)로 넘긴다
pub fn validate_content_length(header: Option<&str>) -> Result<(), UploadError> {
    match header.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(bytes) if bytes > MAX_BODY_BYTES => Err(UploadError::TooLarge { bytes }),
        _ => Ok(()),
    }
}

/// 추출한 페이지 텍스트 검사: 글자 수, MSDS 여부 (서버만 실행)
pub fn validate_text(pages: &[String]) -> Result<(), UploadError> {
    if non_space_chars(pages) < MIN_TEXT_CHARS {
        return Err(UploadError::NoText);
    }
    let distinct: BTreeSet<u32> = pages.iter().flat_map(|p| p.lines().filter_map(heading_number)).collect();
    if distinct.len() < MIN_DISTINCT_HEADINGS {
        return Err(UploadError::NotMsds { distinct: distinct.len() });
    }
    Ok(())
}

/// 전체 검사 (파일 바이트와 추출한 페이지 텍스트)
pub fn validate_upload(bytes: &[u8], pages: &[String]) -> Result<(), UploadError> {
    validate_file(bytes)?;
    validate_text(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msds_pages() -> Vec<String> {
        let body = "물질안전보건자료 본문 내용입니다. ".repeat(20);
        let page: String = (1..=16).map(|n| format!("{n}. 섹션 제목\n{body}\n")).collect();
        vec![page]
    }

    #[test]
    fn accepts_msds_pdf() {
        assert_eq!(validate_upload(b"%PDF-1.7 ...", &msds_pages()), Ok(()));
    }

    #[test]
    fn signature_first() {
        // 시그니처가 틀리면 크기·텍스트보다 먼저 걸린다
        assert_eq!(validate_upload(b"PK\x03\x04", &[]), Err(UploadError::NotPdf));
        assert_eq!(validate_upload(b"", &msds_pages()), Err(UploadError::NotPdf));
        assert_eq!(validate_upload(b" %PDF-1.4", &msds_pages()), Err(UploadError::NotPdf));
        assert_eq!(UploadError::NotPdf.message(), "PDF 파일이 아닙니다");
    }

    #[test]
    fn size_limit() {
        let mut big = b"%PDF-1.7".to_vec();
        big.resize(MAX_UPLOAD_BYTES, b' ');
        assert_eq!(validate_file(&big), Ok(()));
        big.push(b' ');
        assert_eq!(validate_file(&big), Err(UploadError::TooLarge { bytes: MAX_UPLOAD_BYTES + 1 }));
        // 크기가 텍스트보다 먼저
        assert!(matches!(validate_upload(&big, &[]), Err(UploadError::TooLarge { .. })));
        assert!(UploadError::TooLarge { bytes: big.len() }.message().contains("20MB"));
    }

    #[test]
    fn no_text() {
        assert_eq!(validate_upload(b"%PDF-1.4", &[]), Err(UploadError::NoText));
        // 공백은 세지 않는다: 199자 + 공백
        let pages = vec![format!("{}   \n\n", "가".repeat(199))];
        assert_eq!(validate_text(&pages), Err(UploadError::NoText));
        assert_eq!(UploadError::NoText.message(), "텍스트가 없는 PDF입니다(스캔본일 수 있음)");
    }

    #[test]
    fn not_msds() {
        // 텍스트는 충분하지만 섹션 번호가 7가지뿐 (보고서의 번호 목록)
        let text: String = (0..10).flat_map(|_| 1..=7).map(|n| format!("{n}. 항목 설명이 길게 이어집니다\n")).collect();
        assert_eq!(validate_text(&[text]), Err(UploadError::NotMsds { distinct: 7 }));
        assert_eq!(UploadError::NotMsds { distinct: 7 }.message(), "MSDS 문서가 아닌 것으로 보입니다");
    }

    #[test]
    fn text_before_msds_check() {
        // 글자 수가 모자라면 섹션 번호와 상관없이 NoText
        let pages = vec![(1..=16).map(|n| format!("{n}. A\n")).collect::<String>()];
        assert_eq!(validate_text(&pages), Err(UploadError::NoText));
    }

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
    fn content_length_limit() {
        assert_eq!(validate_content_length(None), Ok(()));
        assert_eq!(validate_content_length(Some("abc")), Ok(()));
        assert_eq!(validate_content_length(Some(&MAX_BODY_BYTES.to_string())), Ok(()));
        let over = MAX_BODY_BYTES + 1;
        assert_eq!(validate_content_length(Some(&format!(" {over} "))), Err(UploadError::TooLarge { bytes: over }));
        assert_eq!(
            UploadError::TooLarge { bytes: 60 * 1024 * 1024 }.message(),
            "파일이 20MB를 넘습니다(60.0MB)"
        );
    }
}
