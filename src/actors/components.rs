//! 성분 분석 에이전트: 제품마다 3번 섹션에서 성분을 뽑아 msds_components에 저장한다.

use std::collections::BTreeMap;
use std::sync::Arc;

use pekko_agent_llm::ClaudeClient;
use pekko_agent_tools::ToolRegistry;
use sqlx::PgPool;
use tracing::info;
use uuid::Uuid;

use super::JobOutcome;
use crate::prompts::{COMPONENTS_MAX_TOKENS, COMPONENTS_SYSTEM};
use crate::tool_loop::run_tool_loop;
use crate::tools::{GetSectionTool, SaveComponentsTool};

pub const NO_SECTIONS_ERROR: &str = "추출 결과 없음";

/// 성공 판정: 제품마다 성분 행이 1개 이상. counts는 product_no별 행 수.
pub fn check_components(products: &[i32], counts: &BTreeMap<i32, i64>) -> Result<(), String> {
    let missing: Vec<String> = products
        .iter()
        .filter(|p| counts.get(p).copied().unwrap_or(0) == 0)
        .map(i32::to_string)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("성분이 저장되지 않은 제품: {}", missing.join(", ")))
    }
}

pub async fn run(client: &Arc<ClaudeClient>, pool: &PgPool, doc_id: Uuid) -> Result<JobOutcome, String> {
    let products: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT product_no, max(product_name) FROM msds_sections WHERE doc_id = $1 \
         GROUP BY product_no ORDER BY product_no",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("섹션 조회 실패: {e}"))?;
    if products.is_empty() {
        return Ok(JobOutcome::failed(NO_SECTIONS_ERROR));
    }

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GetSectionTool::new(pool.clone())));
    registry.register(Arc::new(SaveComponentsTool::new(pool.clone())));

    let list: Vec<String> = products
        .iter()
        .map(|(no, name)| format!("- product_no {no}: {}", name.as_deref().unwrap_or("(제품명 없음)")))
        .collect();
    let user = format!(
        "doc_id: {doc_id}\n제품 {}개:\n{}\n\n제품마다 구성성분을 저장하라.",
        products.len(),
        list.join("\n")
    );
    let result = run_tool_loop(client, &mut registry, COMPONENTS_SYSTEM, user, COMPONENTS_MAX_TOKENS)
        .await
        .map_err(|e| e.to_string())?;
    info!(%doc_id, "성분 에이전트 최종 답:\n{}", result.final_text);

    let counts: Vec<(i32, i64)> = sqlx::query_as(
        "SELECT product_no, count(*) FROM msds_components WHERE doc_id = $1 GROUP BY product_no",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("성분 조회 실패: {e}"))?;
    let counts: BTreeMap<i32, i64> = counts.into_iter().collect();
    let product_nos: Vec<i32> = products.iter().map(|(p, _)| *p).collect();
    Ok(JobOutcome::after_loop(&result, check_components(&product_nos, &counts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_check() {
        let counts = BTreeMap::from([(1, 3), (3, 1)]);
        assert!(check_components(&[1, 3], &counts).is_ok());
        assert_eq!(check_components(&[1, 2, 3], &counts).unwrap_err(), "성분이 저장되지 않은 제품: 2");
        assert_eq!(check_components(&[1], &BTreeMap::new()).unwrap_err(), "성분이 저장되지 않은 제품: 1");
    }
}
