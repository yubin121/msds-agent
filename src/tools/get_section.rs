use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::common::{check_section_no, ensure_doc, finish, input_err, parse_input, Fail};
use super::text::char_window;

const MAX_CHARS: usize = 15_000;

pub struct GetSectionTool {
    pool: PgPool,
}

impl GetSectionTool {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Deserialize)]
struct Input {
    doc_id: String,
    section_no: i32,
    #[serde(default)]
    offset: usize,
}

impl GetSectionTool {
    async fn run(&self, input: Value) -> Result<Value, Fail> {
        let input: Input = parse_input(input)?;
        let doc_id = ensure_doc(&self.pool, &input.doc_id).await?;
        check_section_no(input.section_no)?;

        let row: Option<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT title, text FROM msds_sections WHERE doc_id = $1 AND section_no = $2")
                .bind(doc_id)
                .bind(input.section_no)
                .fetch_optional(&self.pool)
                .await?;
        let Some((title, text)) = row else {
            let saved: Vec<(i32,)> =
                sqlx::query_as("SELECT section_no FROM msds_sections WHERE doc_id = $1 ORDER BY section_no")
                    .bind(doc_id)
                    .fetch_all(&self.pool)
                    .await?;
            let saved: Vec<i32> = saved.into_iter().map(|(n,)| n).collect();
            return input_err(format!("{}번 섹션이 저장되어 있지 않습니다. 저장된 섹션: {saved:?}", input.section_no));
        };

        let text = text.unwrap_or_default();
        let total = text.chars().count();
        if input.offset > total {
            return input_err(format!("offset {}이 전체 글자 수 {total}보다 큽니다", input.offset));
        }
        let chunk = char_window(&text, input.offset, MAX_CHARS);
        let end = input.offset + chunk.chars().count();

        Ok(json!({
            "section_no": input.section_no,
            "title": title,
            "text": chunk,
            "offset": input.offset,
            "total_chars": total,
            "next_offset": if end < total { Some(end) } else { None },
        }))
    }
}

#[async_trait]
impl Tool for GetSectionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_get_section".into(),
            description: "저장된 섹션 본문을 offset(글자 단위, 기본 0)부터 최대 15000자 돌려준다. \
                          next_offset이 null이 아니면 그 값으로 다시 호출해 나머지를 읽는다."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "section_no": { "type": "integer", "minimum": 1, "maximum": 16 },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 }
                },
                "required": ["doc_id", "section_no"]
            }),
            required_permissions: vec![],
            timeout_ms: 10_000,
            idempotent: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        finish("msds_get_section", self.run(input).await)
    }
}
