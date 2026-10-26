//! 추출 에이전트: 섹션 제목 위치를 골라 msds_sections에 저장한다.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pekko_agent_llm::ClaudeClient;
use pekko_agent_tools::ToolRegistry;
use sqlx::PgPool;
use tracing::info;
use uuid::Uuid;

use super::JobOutcome;
use crate::prompts::{EXTRACT_MAX_TOKENS, EXTRACT_SYSTEM};
use crate::tool_loop::run_tool_loop;
use crate::tools::headings::{backsteps, heading_number};
use crate::tools::{FindHeadingsTool, SaveSectionsTool};

/// 제목 후보의 서로 다른 번호가 이보다 적으면 MSDS가 아닌 것으로 본다
pub const MIN_DISTINCT_HEADINGS: usize = 8;
pub const NOT_MSDS_ERROR: &str = "MSDS 문서가 아닌 것으로 보입니다(섹션 제목 부족)";
pub const SHUFFLED_ERROR: &str = "섹션 순서가 뒤섞인 문서입니다";

/// LLM 호출 전 코드 검사. 후보 번호(위치 순서)를 받는다.
pub fn precheck(numbers: &[u32]) -> Result<(), String> {
    let distinct: BTreeSet<u32> = numbers.iter().copied().collect();
    if distinct.len() < MIN_DISTINCT_HEADINGS {
        return Err(NOT_MSDS_ERROR.into());
    }
    if !backsteps(numbers).is_empty() {
        return Err(SHUFFLED_ERROR.into());
    }
    Ok(())
}

/// 성공 판정: 저장된 제품마다 2번과 3번 섹션이 있어야 한다. rows는 (product_no, section_no).
pub fn check_sections(rows: &[(i32, i32)]) -> Result<(), String> {
    if rows.is_empty() {
        return Err("저장된 섹션이 없습니다".into());
    }
    let mut products: BTreeMap<i32, BTreeSet<i32>> = BTreeMap::new();
    for &(p, s) in rows {
        products.entry(p).or_default().insert(s);
    }
    let problems: Vec<String> = products
        .iter()
        .filter_map(|(p, secs)| {
            let missing: Vec<String> =
                [2, 3].iter().filter(|n| !secs.contains(n)).map(|n| format!("{n}번")).collect();
            (!missing.is_empty()).then(|| format!("제품 {p}에 {} 섹션이 없습니다", missing.join("·")))
        })
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join(", "))
    }
}

pub async fn run(client: &Arc<ClaudeClient>, pool: &PgPool, doc_id: Uuid) -> Result<JobOutcome, String> {
    let doc: Option<(Option<String>, Option<i32>, Option<String>)> =
        sqlx::query_as("SELECT file_name, page_count, language FROM msds_documents WHERE id = $1")
            .bind(doc_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("문서 조회 실패: {e}"))?;
    let Some((file_name, page_count, language)) = doc else {
        return Ok(JobOutcome::failed(format!("doc_id {doc_id} 문서가 없습니다")));
    };

    // 사전 검사: find_headings와 같은 기준으로 후보 번호를 모은다
    let pages: Vec<(Option<String>,)> =
        sqlx::query_as("SELECT text FROM msds_pages WHERE doc_id = $1 ORDER BY page_no")
            .bind(doc_id)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("페이지 조회 실패: {e}"))?;
    let numbers: Vec<u32> = pages
        .iter()
        .flat_map(|(t,)| t.as_deref().unwrap_or("").lines().filter_map(heading_number).collect::<Vec<_>>())
        .collect();
    if let Err(e) = precheck(&numbers) {
        info!(%doc_id, candidates = numbers.len(), "추출 사전 검사 실패: {e}");
        return Ok(JobOutcome::failed(e));
    }

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FindHeadingsTool::new(pool.clone())));
    registry.register(Arc::new(SaveSectionsTool::new(pool.clone())));

    let user = format!(
        "doc_id: {doc_id}\n파일명: {}\n페이지 수: {}\n언어: {}\n\n이 문서의 섹션을 찾아 저장하라.",
        file_name.unwrap_or_default(),
        page_count.unwrap_or(0),
        language.unwrap_or_default(),
    );
    let result = run_tool_loop(client, &mut registry, EXTRACT_SYSTEM, user, EXTRACT_MAX_TOKENS)
        .await
        .map_err(|e| e.to_string())?;
    info!(%doc_id, "추출 에이전트 최종 답:\n{}", result.final_text);

    let rows: Vec<(i32, i32)> =
        sqlx::query_as("SELECT product_no, section_no FROM msds_sections WHERE doc_id = $1")
            .bind(doc_id)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("섹션 조회 실패: {e}"))?;
    Ok(JobOutcome::after_loop(&result, check_sections(&rows)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precheck_accepts_msds() {
        assert!(precheck(&(1..=16).collect::<Vec<_>>()).is_ok());
        // 3제품, 반복 제목 포함
        let three: Vec<u32> = (0..3).flat_map(|_| [1, 2, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]).collect();
        assert!(precheck(&three).is_ok());
        // 옛 양식처럼 일부 섹션만 있어도 서로 다른 번호가 8개면 통과
        assert!(precheck(&[1, 2, 3, 4, 5, 6, 7, 8]).is_ok());
    }

    #[test]
    fn precheck_rejects_non_msds() {
        // 논문·보고서: 번호 목록이 조금 있을 뿐
        assert_eq!(precheck(&[]).unwrap_err(), NOT_MSDS_ERROR);
        assert_eq!(precheck(&[1, 2, 3, 1, 2, 3, 4]).unwrap_err(), NOT_MSDS_ERROR);
        // 같은 번호가 아무리 많아도 서로 다른 번호가 7개면 부족
        let many: Vec<u32> = (0..20).flat_map(|_| 1..=7).collect();
        assert_eq!(precheck(&many).unwrap_err(), NOT_MSDS_ERROR);
    }

    #[test]
    fn precheck_rejects_shuffled() {
        // NO.6 터치크린 기존 추출 순서
        let shuffled = [1, 3, 2, 5, 6, 7, 4, 8, 10, 9, 11, 12, 13, 14, 16, 15];
        assert_eq!(precheck(&shuffled).unwrap_err(), SHUFFLED_ERROR);
    }

    #[test]
    fn section_check() {
        assert!(check_sections(&[(1, 1), (1, 2), (1, 3)]).is_ok());
        assert!(check_sections(&[(1, 2), (1, 3), (2, 3), (2, 2)]).is_ok());
        assert_eq!(check_sections(&[]).unwrap_err(), "저장된 섹션이 없습니다");
        assert_eq!(check_sections(&[(1, 1), (1, 2)]).unwrap_err(), "제품 1에 3번 섹션이 없습니다");
        assert_eq!(
            check_sections(&[(1, 2), (1, 3), (2, 1)]).unwrap_err(),
            "제품 2에 2번·3번 섹션이 없습니다"
        );
    }
}
