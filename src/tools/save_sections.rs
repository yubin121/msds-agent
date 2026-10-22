use std::collections::HashSet;

use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::common::{check_section_no, ensure_doc, finish, input_err, parse_input, Fail};
use super::headings::{body_is_empty, slice_sections};

pub struct SaveSectionsTool {
    pool: PgPool,
}

impl SaveSectionsTool {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Deserialize)]
struct Input {
    doc_id: String,
    sections: Vec<SectionStart>,
}

#[derive(Deserialize)]
struct SectionStart {
    section_no: i32,
    title: String,
    page_no: i32,
    line_no: i32,
}

impl SaveSectionsTool {
    async fn run(&self, input: Value) -> Result<Value, Fail> {
        let input: Input = parse_input(input)?;
        let doc_id = ensure_doc(&self.pool, &input.doc_id).await?;
        if input.sections.is_empty() {
            return input_err("sections가 비어 있습니다");
        }

        let mut seen = HashSet::new();
        for s in &input.sections {
            check_section_no(s.section_no)?;
            if !seen.insert(s.section_no) {
                return input_err(format!("section_no {}가 중복되었습니다", s.section_no));
            }
        }

        let pages: Vec<(i32, Option<String>)> =
            sqlx::query_as("SELECT page_no, text FROM msds_pages WHERE doc_id = $1 ORDER BY page_no")
                .bind(doc_id)
                .fetch_all(&self.pool)
                .await?;
        let pages: Vec<(i32, String)> = pages.into_iter().map(|(p, t)| (p, t.unwrap_or_default())).collect();
        let starts: Vec<(i32, i32)> = input.sections.iter().map(|s| (s.page_no, s.line_no)).collect();
        let texts = slice_sections(&pages, &starts).map_err(Fail::Input)?;

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM msds_sections WHERE doc_id = $1").bind(doc_id).execute(&mut *tx).await?;
        for (s, text) in input.sections.iter().zip(&texts) {
            sqlx::query(
                "INSERT INTO msds_sections (doc_id, section_no, title, text, page_no, line_no) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(doc_id)
            .bind(s.section_no)
            .bind(s.title.trim())
            .bind(text)
            .bind(s.page_no)
            .bind(s.line_no)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        let saved: Vec<Value> = input
            .sections
            .iter()
            .zip(&texts)
            .map(|(s, t)| json!({ "section_no": s.section_no, "title": s.title.trim(), "chars": t.chars().count() }))
            .collect();
        let missing: Vec<i32> = (1..=16).filter(|n| !seen.contains(n)).collect();
        let empty_warnings: Vec<String> = input
            .sections
            .iter()
            .zip(&texts)
            .filter(|(_, t)| body_is_empty(t))
            .map(|(s, _)| format!("{}번 섹션은 제목 줄 외에 내용이 없습니다. 위치가 맞는지 확인하라", s.section_no))
            .collect();

        Ok(json!({
            "doc_id": doc_id,
            "saved": saved,
            "missing_sections": missing,
            "warnings": empty_warnings,
        }))
    }
}

#[async_trait]
impl Tool for SaveSectionsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_save_sections".into(),
            description: "섹션 제목의 위치(page_no, line_no)를 받아 각 위치부터 다음 제목 전까지의 본문을 페이지 텍스트에서 \
                          잘라 저장한다. 본문은 보내지 않는다. 위치는 문서 순서(오름차순)로, section_no(1~16)는 중복 없이. \
                          다시 호출하면 이 문서의 섹션을 모두 지우고 새로 저장한다."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "sections": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "section_no": { "type": "integer", "minimum": 1, "maximum": 16 },
                                "title": { "type": "string" },
                                "page_no": { "type": "integer" },
                                "line_no": { "type": "integer" }
                            },
                            "required": ["section_no", "title", "page_no", "line_no"]
                        }
                    }
                },
                "required": ["doc_id", "sections"]
            }),
            required_permissions: vec![],
            timeout_ms: 10_000,
            idempotent: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        finish("msds_save_sections", self.run(input).await)
    }
}
