use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::common::{check_product_no, check_section_no, ensure_doc, finish, first_product, input_err, parse_input, Fail};

pub struct SaveTranslationTool {
    pool: PgPool,
}

impl SaveTranslationTool {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Deserialize)]
struct Input {
    doc_id: String,
    #[serde(default = "first_product")]
    product_no: i32,
    section_no: i32,
    #[serde(default)]
    chunk_no: i32,
    #[serde(default)]
    title_ko: Option<String>,
    text_ko: String,
}

impl SaveTranslationTool {
    async fn run(&self, input: Value) -> Result<Value, Fail> {
        let input: Input = parse_input(input)?;
        let doc_id = ensure_doc(&self.pool, &input.doc_id).await?;
        check_product_no(input.product_no)?;
        check_section_no(input.section_no)?;
        if input.chunk_no < 0 {
            return input_err(format!("chunk_no는 0 이상이어야 합니다: {}", input.chunk_no));
        }
        if input.text_ko.trim().is_empty() {
            return input_err("text_ko가 비어 있습니다");
        }
        let title_ko = input.title_ko.as_deref().map(str::trim).filter(|s| !s.is_empty());

        let chunk = TranslationChunk {
            doc_id,
            product_no: input.product_no,
            section_no: input.section_no,
            chunk_no: input.chunk_no,
            title_ko,
            text_ko: &input.text_ko,
        };
        let saved_chunks = save_chunk(&self.pool, &chunk).await?;

        Ok(json!({
            "doc_id": doc_id,
            "product_no": input.product_no,
            "section_no": input.section_no,
            "chunk_no": input.chunk_no,
            "chars": input.text_ko.chars().count(),
            "saved_chunks": saved_chunks,
        }))
    }
}

/// 번역 조각 하나
pub struct TranslationChunk<'a> {
    pub doc_id: Uuid,
    pub product_no: i32,
    pub section_no: i32,
    pub chunk_no: i32,
    pub title_ko: Option<&'a str>,
    pub text_ko: &'a str,
}

/// (doc_id, product_no, section_no, chunk_no) 단위로 교체 저장하고, 그 섹션에 저장된 chunk_no 목록을 돌려준다.
/// 툴과 번역 에이전트가 같이 쓴다. 입력 검사는 호출하는 쪽에서 한다.
pub async fn save_chunk(pool: &PgPool, c: &TranslationChunk<'_>) -> Result<Vec<i32>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM msds_translations \
         WHERE doc_id = $1 AND product_no = $2 AND section_no = $3 AND chunk_no = $4",
    )
    .bind(c.doc_id)
    .bind(c.product_no)
    .bind(c.section_no)
    .bind(c.chunk_no)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO msds_translations (doc_id, product_no, section_no, chunk_no, title_ko, text_ko) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(c.doc_id)
    .bind(c.product_no)
    .bind(c.section_no)
    .bind(c.chunk_no)
    .bind(c.title_ko)
    .bind(c.text_ko)
    .execute(&mut *tx)
    .await?;
    let chunks: Vec<(i32,)> = sqlx::query_as(
        "SELECT chunk_no FROM msds_translations \
         WHERE doc_id = $1 AND product_no = $2 AND section_no = $3 ORDER BY chunk_no",
    )
    .bind(c.doc_id)
    .bind(c.product_no)
    .bind(c.section_no)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(chunks.into_iter().map(|(n,)| n).collect())
}

#[async_trait]
impl Tool for SaveTranslationTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_save_translation".into(),
            description: "섹션 번역문을 저장한다. 긴 섹션은 chunk_no(0부터)로 나눠 여러 번 저장하면 chunk_no 순서로 이어 붙여 \
                          보여준다. 같은 (product_no, section_no, chunk_no)로 다시 저장하면 그 조각만 교체된다. \
                          다중 제품 문서는 product_no(기본 1)를 적는다."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "product_no": { "type": "integer", "minimum": 1, "default": 1 },
                    "section_no": { "type": "integer", "minimum": 1, "maximum": 16 },
                    "chunk_no": { "type": "integer", "minimum": 0, "default": 0 },
                    "title_ko": { "type": "string", "description": "섹션 제목 번역" },
                    "text_ko": { "type": "string", "description": "본문 번역" }
                },
                "required": ["doc_id", "section_no", "title_ko", "text_ko"]
            }),
            required_permissions: vec![],
            timeout_ms: 10_000,
            idempotent: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        finish("msds_save_translation", self.run(input).await)
    }
}
