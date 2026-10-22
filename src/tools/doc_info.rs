use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// 문서 기본 정보(파일명, 페이지 수, 언어)를 조회하는 테스트용 툴
pub struct DocInfoTool {
    pool: PgPool,
}

impl DocInfoTool {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Tool for DocInfoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_doc_info".into(),
            description: "업로드된 MSDS 문서의 파일명, 페이지 수, 언어(ko/en)를 조회한다.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string", "description": "문서 UUID" }
                },
                "required": ["doc_id"]
            }),
            required_permissions: vec![],
            timeout_ms: 10_000,
            idempotent: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(raw) = input.get("doc_id").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("오류: doc_id(문자열)가 필요합니다"));
        };
        let Ok(doc_id) = Uuid::parse_str(raw.trim()) else {
            return Ok(ToolOutput::error(format!("오류: doc_id가 UUID 형식이 아닙니다: {raw}")));
        };

        let row = sqlx::query("SELECT file_name, page_count, language FROM msds_documents WHERE id = $1")
            .bind(doc_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("DB 조회 실패: {e}")))?;

        let Some(row) = row else {
            return Ok(ToolOutput::error(format!("오류: doc_id {doc_id} 문서가 없습니다")));
        };
        Ok(ToolOutput::success(json!({
            "doc_id": doc_id,
            "file_name": row.get::<Option<String>, _>("file_name"),
            "page_count": row.get::<Option<i32>, _>("page_count"),
            "language": row.get::<Option<String>, _>("language"),
        })))
    }
}
