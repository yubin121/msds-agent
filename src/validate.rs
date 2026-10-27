//! 업로드 입력 검증. 규칙은 msds-validate 크레이트 한 곳에 있고 서버와 Cloudflare Worker가 같은 코드를 쓴다.
//! 검사 순서: %PDF- 시그니처 → 크기 20MB 이하 → 공백 제외 텍스트 200자 이상 → 서로 다른 섹션 번호 8개 이상
//! (Worker는 시그니처·크기만, 서버는 네 가지 모두 실행한다)

pub use msds_validate::{validate_file, validate_text, validate_upload};
