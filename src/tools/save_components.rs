use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::cas::{check_cas_field, CasField};
use super::common::{check_product_no, de_opt_text, ensure_doc, finish, first_product, input_err, parse_input, Fail};

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
    #[serde(default = "first_product")]
    product_no: i32,
    /// 행에 product_name이 없으면 이 값을 쓴다
    #[serde(default, deserialize_with = "de_opt_text")]
    product_name: Option<String>,
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

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

impl SaveComponentsTool {
    async fn run(&self, input: Value) -> Result<Value, Fail> {
        let input: Input = parse_input(input)?;
        let doc_id = ensure_doc(&self.pool, &input.doc_id).await?;
        check_product_no(input.product_no)?;
        if let Some(i) = input.components.iter().position(|c| c.component.trim().is_empty()) {
            return input_err(format!("components[{i}].component가 비어 있습니다"));
        }

        let checks: Vec<CasField> = input.components.iter().map(|c| check_cas_field(c.cas_no.as_deref())).collect();

        // 제품명은 msds_sections가 기준이다. 거기에 이름이 없을 때만 입력 값을 쓴다
        let (section_name,): (Option<String>,) = sqlx::query_as(
            "SELECT max(product_name) FROM msds_sections WHERE doc_id = $1 AND product_no = $2",
        )
        .bind(doc_id)
        .bind(input.product_no)
        .fetch_one(&self.pool)
        .await?;
        let section_name = non_empty(section_name.as_deref()).map(str::to_string);
        let default_name = section_name.as_deref().or(non_empty(input.product_name.as_deref()));

        let mut tx = self.pool.begin().await?;
        // 이 제품의 행만 지우고 다시 넣는다
        sqlx::query("DELETE FROM msds_components WHERE doc_id = $1 AND product_no = $2")
            .bind(doc_id)
            .bind(input.product_no)
            .execute(&mut *tx)
            .await?;
        for (c, check) in input.components.iter().zip(&checks) {
            let name = match section_name {
                Some(_) => default_name,
                None => non_empty(c.product_name.as_deref()).or(default_name),
            };
            sqlx::query(
                "INSERT INTO msds_components (doc_id, product_no, product_name, component, cas_no, percent, cas_valid) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(doc_id)
            .bind(input.product_no)
            .bind(name)
            .bind(c.component.trim())
            .bind(&check.cas_no)
            .bind(c.percent.as_deref().map(str::trim)) // 원문 그대로 ("10-30", "<1")
            .bind(check.cas_valid)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        let rows: Vec<Value> = input
            .components
            .iter()
            .zip(&checks)
            .map(|(c, check)| json!({ "component": c.component.trim(), "cas_no": check.cas_no, "cas_valid": check.cas_valid }))
            .collect();
        // 한 칸에 CAS가 여러 개면 틀린 번호만 적는다
        let invalid: Vec<Value> = input
            .components
            .iter()
            .zip(&checks)
            .enumerate()
            .flat_map(|(i, (c, check))| {
                check.invalid.iter().map(move |(cleaned, reason)| {
                    json!({ "index": i, "component": c.component.trim(), "cas_no": cleaned, "reason": reason })
                })
            })
            .collect();

        let mut out = json!({
            "doc_id": doc_id,
            "product_no": input.product_no,
            "product_name": default_name,
            "saved": rows.len(),
            "rows": rows,
            "invalid_cas": invalid,
        });
        if !invalid.is_empty() {
            out["notice"] = json!(
                "CAS 번호 형식이나 체크섬이 틀린 행이 있다. 원문을 다시 읽어 잘못 읽은 것이면 고쳐서 다시 저장하라 \
                 (다시 저장하면 이 제품의 성분 목록 전체가 교체된다). 원문 자체가 틀린 것이면 고치지 말고 그대로 둔다."
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
            description: "구성성분 목록을 제품 단위로 저장한다. CAS 번호는 공백·대시를 정리한 뒤 형식과 체크섬을 검사한다. \
                          원문 한 칸에 CAS가 여러 개면 행을 나누지 말고 쉼표로 구분해 한 행에 넣는다 \
                          ('64742-54-7, 64742-65-0'). 영업비밀·Proprietary는 '비공개'로, 빈 값·'-'·'N/A'·'없음'은 \
                          비워 저장한다. percent는 원문 그대로('10-30', '<1'). 다중 제품 문서는 제품마다 product_no(기본 1)를 \
                          적어 따로 호출한다. 제품명은 추출 단계에서 저장한 이름을 쓴다(product_name은 그 이름이 없을 때만 쓰인다). \
                          다시 호출하면 그 제품의 성분만 지우고 새로 저장한다."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "product_no": { "type": "integer", "minimum": 1, "default": 1, "description": "다중 제품 문서의 제품 번호" },
                    "product_name": { "type": "string" },
                    "components": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "product_name": { "type": "string" },
                                "component": { "type": "string", "description": "성분명" },
                                "cas_no": { "type": "string", "description": "CAS 원문. 여러 개면 쉼표로 구분" },
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
