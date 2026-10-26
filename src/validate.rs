//! 업로드 입력 검증 (순수 함수)
//!
//! 분석 실패 후 재시도하는 대신 입력 단계에서 걸러 낸다(10/26 소장님 피드백).
//! 규칙이 단순해서 다른 런타임(Cloudflare Worker 등)에서도 같은 순서·기준으로 옮길 수 있다.
//! 검사 순서: %PDF- 시그니처 → 크기 20MB 이하 → 공백 제외 텍스트 200자 이상 → 서로 다른 섹션 번호 8개 이상

use std::collections::BTreeSet;

use crate::pdf::{non_space_chars, MIN_TEXT_CHARS, NO_TEXT_ERROR};
use crate::tools::headings::{heading_number, MIN_DISTINCT_HEADINGS};

pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;

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

/// 텍스트 추출 전에 할 수 있는 검사: 시그니처, 크기
pub fn validate_file(bytes: &[u8]) -> Result<(), UploadError> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(UploadError::NotPdf);
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(UploadError::TooLarge { bytes: bytes.len() });
    }
    Ok(())
}

/// 추출한 페이지 텍스트 검사: 글자 수, MSDS 여부
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
}
