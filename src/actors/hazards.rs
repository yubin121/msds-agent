//! 그림문자 에이전트: 제품마다 2번 섹션의 H코드·분류로 msds_hazards를 저장한다.

use std::collections::BTreeMap;
use std::sync::Arc;

use pekko_agent_llm::ClaudeClient;
use pekko_agent_tools::ToolRegistry;
use sqlx::PgPool;
use tracing::info;
use uuid::Uuid;

use super::{check_rows_per_product, load_products, product_list, rows_per_product, JobOutcome, NO_SECTIONS_ERROR};
use crate::prompts::{HAZARDS_MAX_TOKENS, HAZARDS_SYSTEM};
use crate::tool_loop::run_tool_loop;
use crate::tools::{GetSectionTool, SaveHazardsTool};

/// 성공 판정: 제품마다 msds_hazards 행이 1개 이상 (H코드가 없어 note만 남긴 행 포함)
pub fn check_hazards(products: &[i32], counts: &BTreeMap<i32, i64>) -> Result<(), String> {
    check_rows_per_product(products, counts, "유해성 정보가")
}

pub async fn run(client: &Arc<ClaudeClient>, pool: &PgPool, doc_id: Uuid) -> Result<JobOutcome, String> {
    let products = load_products(pool, doc_id).await?;
    if products.is_empty() {
        return Ok(JobOutcome::failed(NO_SECTIONS_ERROR));
    }

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GetSectionTool::new(pool.clone())));
    registry.register(Arc::new(SaveHazardsTool::new(pool.clone())));

    let user = format!(
        "doc_id: {doc_id}\n제품 {}개:\n{}\n\n제품마다 유해성 정보와 그림문자를 저장하라.",
        products.len(),
        product_list(&products)
    );
    let result = run_tool_loop(client, &mut registry, HAZARDS_SYSTEM, user, HAZARDS_MAX_TOKENS)
        .await
        .map_err(|e| e.to_string())?;
    info!(%doc_id, "그림문자 에이전트 최종 답:\n{}", result.final_text);

    let counts = rows_per_product(pool, "msds_hazards", doc_id).await?;
    let product_nos: Vec<i32> = products.iter().map(|(p, _)| *p).collect();
    Ok(JobOutcome::after_loop(&result, check_hazards(&product_nos, &counts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hazards_check() {
        // no_h_codes 행도 1행으로 센다
        let counts = BTreeMap::from([(1, 4), (2, 1), (3, 1)]);
        assert!(check_hazards(&[1, 2, 3], &counts).is_ok());
        assert_eq!(check_hazards(&[1, 4], &counts).unwrap_err(), "유해성 정보가 저장되지 않은 제품: 4");
    }
}
