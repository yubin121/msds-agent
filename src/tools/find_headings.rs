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
/// 1번 섹션 후보마다 함께 돌려주는 아래 줄 수 (제품명 찾기용)
const FOLLOWING_LINES: usize = 10;

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

        // 모든 줄을 (page_no, line_no, line)로 펼친다. 1번 섹션 아래 줄은 페이지를 넘어 이어 붙인다
        let lines: Vec<(i32, usize, &str)> = pages
            .iter()
            .flat_map(|(p, text)| text.as_deref().unwrap_or("").lines().enumerate().map(move |(i, l)| (*p, i + 1, l)))
            .collect();
        // (lines 인덱스, page_no, line_no, number, line)
        let found: Vec<(usize, i32, usize, u32, &str)> = lines
            .iter()
            .enumerate()
            .filter_map(|(idx, &(p, l, line))| heading_number(line).map(|n| (idx, p, l, n, line)))
            .collect();

        let numbers: Vec<u32> = found.iter().map(|f| f.3).collect();
        let steps: Vec<Value> = backsteps(&numbers)
            .into_iter()
            .map(|(i, from, to)| json!({ "page_no": found[i].1, "line_no": found[i].2, "from": from, "to": to }))
            .collect();
        let order_ok = steps.is_empty();
        let headings: Vec<Value> = found
            .iter()
            .take(MAX_RESULTS)
            .map(|&(idx, page_no, line_no, n, line)| {
                let mut h = json!({
                    "page_no": page_no,
                    "line_no": line_no,
                    "number": n,
                    "text": truncate_chars(line.trim(), MAX_LINE_CHARS).0,
                });
                // 1번 섹션 후보는 제품명을 찾을 수 있게 아래 줄을 함께 준다
                if n == 1 {
                    h["following_lines"] = json!(following_lines(&lines, idx));
                }
                h
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

/// lines[idx] 아래의 빈 줄이 아닌 줄을 FOLLOWING_LINES개까지, 줄마다 MAX_LINE_CHARS자로 자른다.
fn following_lines(lines: &[(i32, usize, &str)], idx: usize) -> Vec<String> {
    lines[idx + 1..]
        .iter()
        .map(|(_, _, l)| l.trim())
        .filter(|l| !l.is_empty())
        .take(FOLLOWING_LINES)
        .map(|l| truncate_chars(l, MAX_LINE_CHARS).0.to_string())
        .collect()
}

#[async_trait]
impl Tool for FindHeadingsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_find_headings".into(),
            description: "문서의 각 페이지를 줄 단위로 훑어 MSDS 섹션 제목 후보(SECTION 3, 3. Composition, 3. 구성성분 등)를 \
                          찾는다. 결과의 page_no·line_no를 msds_save_sections에 그대로 쓴다. 최대 200개. \
                          1번 섹션 후보에는 제품명을 찾을 수 있게 아래 10줄(following_lines)을 함께 준다."
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn following_lines_skip_blank_and_cross_pages() {
        let long = "가".repeat(130);
        let lines = vec![
            (1, 1, "SECTION 1"),
            (1, 2, ""),
            (1, 3, "  Product name : 터치크린  "),
            (2, 1, long.as_str()),
        ];
        let out = following_lines(&lines, 0);
        assert_eq!(out, vec!["Product name : 터치크린".to_string(), "가".repeat(120)]);
        assert!(following_lines(&lines, 3).is_empty());

        let many: Vec<(i32, usize, &str)> = (1..=20).map(|i| (1, i, "x")).collect();
        assert_eq!(following_lines(&many, 0).len(), FOLLOWING_LINES);
    }
}
