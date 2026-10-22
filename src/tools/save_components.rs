use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::cas::{check_cas, CasCheck};
use super::common::{de_opt_text, ensure_doc, finish, input_err, parse_input, Fail};

pub struct SaveComponentsTool {
    pool: PgPool,
}

impl SaveComponentsTool {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Deserialize)]
struct Input {
    doc_id: String,
    components: Vec<Component>,
}

#[derive(Deserialize)]
struct Component {
    #[serde(default, deserialize_with = "de_opt_text")]
    product_name: Option<String>,
    component: String,
    #[serde(default, deserialize_with = "de_opt_text")]
    cas_no: Option<String>,
    #[serde(default, deserialize_with = "de_opt_text")]
    percent: Option<String>,
}

impl SaveComponentsTool {
    async fn run(&self, input: Value) -> Result<Value, Fail> {
        let input: Input = parse_input(input)?;
        let doc_id = ensure_doc(&self.pool, &input.doc_id).await?;
        if let Some(i) = input.components.iter().position(|c| c.component.trim().is_empty()) {
            return input_err(format!("components[{i}].component가 비어 있습니다"));
        }

        let checks: Vec<CasCheck> = input.components.iter().map(|c| check_cas(c.cas_no.as_deref())).collect();

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM msds_components WHERE doc_id = $1").bind(doc_id).execute(&mut *tx).await?;
        for (c, check) in input.components.iter().zip(&checks) {
            let (cas_no, cas_valid) = check.db_values();
            sqlx::query(
                "INSERT INTO msds_components (doc_id, product_name, component, cas_no, percent, cas_valid) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(doc_id)
            .bind(c.product_name.as_deref().map(str::trim))
            .bind(c.component.trim())
            .bind(cas_no)
            .bind(c.percent.as_deref().map(str::trim)) // 원문 그대로 ("10-30", "<1")
            .bind(cas_valid)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        let rows: Vec<Value> = input
            .components
            .iter()
            .zip(&checks)
            .map(|(c, check)| {
                let (cas_no, cas_valid) = check.db_values();
                json!({ "component": c.component.trim(), "cas_no": cas_no, "cas_valid": cas_valid })
            })
            .collect();
        let invalid: Vec<Value> = input
            .components
            .iter()
            .zip(&checks)
            .enumerate()
            .filter_map(|(i, (c, check))| match check {
                CasCheck::Invalid { cleaned, reason } => Some(json!({
                    "index": i,
                    "component": c.component.trim(),
                    "cas_no": cleaned,
                    "reason": reason,
                })),
                _ => None,
            })
            .collect();

        let mut out = json!({ "doc_id": doc_id, "saved": rows.len(), "rows": rows, "invalid_cas": invalid });
        if !invalid.is_empty() {
            out["notice"] = json!(
                "CAS 번호 형식이나 체크섬이 틀린 행이 있다. 원문을 다시 확인해 고쳐서 다시 저장하라 \
                 (다시 저장하면 이 문서의 성분 목록 전체가 교체된다)."
            );
        }
        Ok(out)
    }
}

#[async_trait]
impl Tool for SaveComponentsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_save_components".into(),
            description: "구성성분 목록을 저장한다. CAS 번호는 공백·대시를 정리한 뒤 형식과 체크섬을 검사한다. \
                          영업비밀·Proprietary는 '비공개'로, 빈 값·'-'·'N/A'는 비워 저장한다. percent는 원문 그대로 \
                          ('10-30', '<1'). 다시 호출하면 이 문서의 성분을 모두 지우고 새로 저장한다."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "components": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "product_name": { "type": "string" },
                                "component": { "type": "string", "description": "성분명" },
                                "cas_no": { "type": "string" },
                                "percent": { "type": "string", "description": "함유량 원문" }
                            },
                            "required": ["component", "cas_no", "percent"]
                        }
                    }
                },
                "required": ["doc_id", "components"]
            }),
            required_permissions: vec![],
            timeout_ms: 10_000,
            idempotent: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        finish("msds_save_components", self.run(input).await)
    }
}
