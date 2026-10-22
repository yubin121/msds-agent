use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::common::{ensure_doc, finish, parse_input, Fail};
use super::ghs::{classify, symbol_name};

pub struct SaveHazardsTool {
    pool: PgPool,
}

impl SaveHazardsTool {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Deserialize)]
struct Input {
    doc_id: String,
    h_codes: Vec<String>,
    #[serde(default)]
    signal_word: Option<String>,
}

impl SaveHazardsTool {
    async fn run(&self, input: Value) -> Result<Value, Fail> {
        let input: Input = parse_input(input)?;
        let doc_id = ensure_doc(&self.pool, &input.doc_id).await?;
        let signal_word = input.signal_word.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let res = classify(&input.h_codes);

        // 행 = (H코드, 그림문자). 그림문자가 없거나 우선순위로 빠진 코드는 ghs_code NULL 한 행.
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM msds_hazards WHERE doc_id = $1").bind(doc_id).execute(&mut *tx).await?;
        let insert = "INSERT INTO msds_hazards (doc_id, h_code, ghs_code, symbol_name, evidence, signal_word) \
                      VALUES ($1, $2, $3, $4, $5, $6)";
        for c in &res.codes {
            let ghs: Vec<Option<&str>> =
                if c.pictograms.is_empty() { vec![None] } else { c.pictograms.iter().map(|p| Some(*p)).collect() };
            for g in ghs {
                sqlx::query(insert)
                    .bind(doc_id)
                    .bind(&c.code)
                    .bind(g)
                    .bind(g.map(symbol_name))
                    .bind(&c.source)
                    .bind(signal_word)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        if res.codes.is_empty() && signal_word.is_some() {
            sqlx::query(insert)
                .bind(doc_id)
                .bind(None::<String>)
                .bind(None::<String>)
                .bind(None::<String>)
                .bind(None::<String>)
                .bind(signal_word)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;

        let pictograms: Vec<Value> = res
            .pictograms
            .iter()
            .map(|(g, codes)| json!({ "ghs_code": g, "symbol_name": symbol_name(g), "h_codes": codes }))
            .collect();
        let suppressed: Vec<Value> =
            res.suppressed.iter().map(|(c, r)| json!({ "h_code": c, "reason": r })).collect();

        Ok(json!({
            "doc_id": doc_id,
            "pictograms": pictograms,
            "h_codes": res.codes.iter().map(|c| &c.code).collect::<Vec<_>>(),
            "no_pictogram": res.no_pictogram,
            "unknown_codes": res.unknown_codes,
            "suppressed_ghs07": suppressed,
            "signal_word": signal_word,
        }))
    }
}

#[async_trait]
impl Tool for SaveHazardsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_save_hazards".into(),
            description: "2번 섹션의 H코드 목록으로 GHS 그림문자를 정해 저장한다. 'H301+H311' 같은 묶음은 나눠서 처리하고, \
                          EUH 코드는 무시한다(unknown_codes). 그림문자 변환과 GHS 우선순위(GHS06·GHS05·H334가 있을 때 \
                          GHS07 생략)는 툴이 처리하므로 H코드만 정확히 보내라. 다시 호출하면 이 문서의 유해성 정보를 \
                          모두 지우고 새로 저장한다."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "h_codes": { "type": "array", "items": { "type": "string" }, "description": "예: [\"H225\", \"H301+H311\"]" },
                    "signal_word": { "type": "string", "description": "신호어 원문 (위험/경고/Danger/Warning)" }
                },
                "required": ["doc_id", "h_codes"]
            }),
            required_permissions: vec![],
            timeout_ms: 10_000,
            idempotent: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        finish("msds_save_hazards", self.run(input).await)
    }
}
