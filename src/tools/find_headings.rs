use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::common::{ensure_doc, finish, input_err, parse_input, Fail};
use super::headings::{backsteps, heading_number};
use super::text::truncate_chars;

const MAX_RESULTS: usize = 200;
const MAX_LINE_CHARS: usize = 120;

pub struct FindHeadingsTool {
    pool: PgPool,
}

impl FindHeadingsTool {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Deserialize)]
struct Input {
    doc_id: String,
}

impl FindHeadingsTool {
    async fn run(&self, input: Value) -> Result<Value, Fail> {
        let input: Input = parse_input(input)?;
        let doc_id = ensure_doc(&self.pool, &input.doc_id).await?;

        let pages: Vec<(i32, Option<String>)> =
            sqlx::query_as("SELECT page_no, text FROM msds_pages WHERE doc_id = $1 ORDER BY page_no")
                .bind(doc_id)
                .fetch_all(&self.pool)
                .await?;
        if pages.is_empty() {
            return input_err("이 문서에는 추출된 페이지 텍스트가 없습니다");
        }

        // (page_no, line_no, number, line)
        let mut found = Vec::new();
        for (page_no, text) in &pages {
            for (i, line) in text.as_deref().unwrap_or("").lines().enumerate() {
                if let Some(n) = heading_number(line) {
                    found.push((*page_no, i + 1, n, line));
                }
            }
        }

        let numbers: Vec<u32> = found.iter().map(|f| f.2).collect();
        let steps: Vec<Value> = backsteps(&numbers)
            .into_iter()
            .map(|(i, from, to)| json!({ "page_no": found[i].0, "line_no": found[i].1, "from": from, "to": to }))
            .collect();
        let order_ok = steps.is_empty();
        let headings: Vec<Value> = found
            .iter()
            .take(MAX_RESULTS)
            .map(|(page_no, line_no, n, line)| {
                json!({
                    "page_no": page_no,
                    "line_no": line_no,
                    "number": n,
                    "text": truncate_chars(line.trim(), MAX_LINE_CHARS).0,
                })
            })
            .collect();

        Ok(json!({
            "doc_id": doc_id,
            "headings": headings,
            "total_found": found.len(),
            "truncated": found.len() > MAX_RESULTS,
            "order_ok": order_ok,
            "backsteps": steps,
            "note": if order_ok {
                "패턴으로 찾은 후보다. 표 번호나 목록 번호가 섞여 있을 수 있으니 진짜 섹션 제목만 골라라."
            } else {
                "섹션 순서가 뒤섞인 문서다. 저장하지 말고 보고하라"
            },
        }))
    }
}

#[async_trait]
impl Tool for FindHeadingsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_find_headings".into(),
            description: "문서의 각 페이지를 줄 단위로 훑어 MSDS 섹션 제목 후보(SECTION 3, 3. Composition, 3. 구성성분 등)를 \
                          찾는다. 결과의 page_no·line_no를 msds_save_sections에 그대로 쓴다. 최대 200개."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "doc_id": { "type": "string", "description": "문서 UUID" } },
                "required": ["doc_id"]
            }),
            required_permissions: vec![],
            timeout_ms: 10_000,
            idempotent: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        finish("msds_find_headings", self.run(input).await)
    }
}
