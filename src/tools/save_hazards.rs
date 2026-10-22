use std::collections::HashMap;

use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::classification::{convert, HAZARD_CLASSES};
use super::common::{check_product_no, ensure_doc, finish, first_product, input_err, parse_input, Fail};
use super::ghs::{classify, symbol_name};

pub const SOURCE_DOCUMENT: &str = "document";
pub const SOURCE_CLASSIFICATION: &str = "classification";

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
    #[serde(default = "first_product")]
    product_no: i32,
    #[serde(default)]
    h_codes: Vec<String>,
    #[serde(default)]
    classifications: Vec<Classification>,
    #[serde(default)]
    signal_word: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
struct Classification {
    hazard_class: String,
    category: String,
}

/// "H360Fd" → 360
fn base_number(code: &str) -> Option<u16> {
    code.get(1..4)?.parse().ok()
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

impl SaveHazardsTool {
    async fn run(&self, input: Value) -> Result<Value, Fail> {
        let input: Input = parse_input(input)?;
        let doc_id = ensure_doc(&self.pool, &input.doc_id).await?;
        check_product_no(input.product_no)?;
        let signal_word = non_empty(input.signal_word.as_deref());
        let note = non_empty(input.note.as_deref());

        if input.h_codes.is_empty() && input.classifications.is_empty() && note.is_none() {
            return input_err("H코드가 문서에 없으면 추측하지 말고 note에 이유를 적어라");
        }

        // 문서에 적힌 H코드
        let doc = classify(&input.h_codes);
        let doc_bases: Vec<u16> = doc.codes.iter().filter_map(|c| base_number(&c.code)).collect();

        // 분류 → H코드. 문서 코드와 번호가 같으면(H360 ↔ H360Fd) 문서 코드를 쓴다
        let mut unknown_classifications = Vec::new();
        let mut from_class: HashMap<u16, String> = HashMap::new(); // 번호 → 근거 ("skin_sensitization 1")
        let mut extra_codes = Vec::new();
        let mut warnings = Vec::new();
        for c in &input.classifications {
            match convert(c.hazard_class.trim(), &c.category) {
                Ok(code) => {
                    let n = base_number(code).unwrap_or_default();
                    let evidence = format!("{} {}", c.hazard_class.trim(), c.category.trim());
                    if from_class.contains_key(&n) {
                        continue;
                    }
                    from_class.insert(n, evidence.clone());
                    if !doc_bases.contains(&n) {
                        extra_codes.push(code.to_string());
                        // 문서에 H코드 목록이 있는데 분류에서 나온 코드가 빠져 있으면 경고 (목록이 없으면 당연히 없다)
                        if !doc.codes.is_empty() {
                            warnings.push(format!(
                                "{code}({evidence})는 분류에서 나왔지만 문서의 H코드 목록에 없다. 문서를 다시 확인하라"
                            ));
                        }
                    }
                }
                Err(reason) => unknown_classifications.push(json!({
                    "hazard_class": c.hazard_class,
                    "category": c.category,
                    "reason": reason,
                })),
            }
        }

        let mut merged = input.h_codes.clone();
        merged.extend(extra_codes);
        let res = classify(&merged);

        // 코드별 출처와 근거
        let doc_codes: Vec<&str> = doc.codes.iter().map(|c| c.code.as_str()).collect();
        let rows: Vec<(&super::ghs::CodeResult, Vec<&str>, String)> = res
            .codes
            .iter()
            .map(|c| {
                let n = base_number(&c.code).unwrap_or_default();
                let mut sources = Vec::new();
                if doc_codes.contains(&c.code.as_str()) {
                    sources.push(SOURCE_DOCUMENT);
                }
                if from_class.contains_key(&n) {
                    sources.push(SOURCE_CLASSIFICATION);
                }
                // 합친 입력에서 나온 코드라 출처가 늘 하나 이상이지만, 없으면 문서로 본다
                if sources.is_empty() {
                    sources.push(SOURCE_DOCUMENT);
                }
                let evidence = if sources[0] == SOURCE_DOCUMENT {
                    c.source.clone()
                } else {
                    from_class.get(&n).cloned().unwrap_or_default()
                };
                (c, sources, evidence)
            })
            .collect();

        // 행 = (H코드, 그림문자). 그림문자가 없거나 우선순위로 빠진 코드는 ghs_code NULL 한 행.
        // source는 문서에 있으면 document, 분류에서만 나왔으면 classification.
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM msds_hazards WHERE doc_id = $1 AND product_no = $2")
            .bind(doc_id)
            .bind(input.product_no)
            .execute(&mut *tx)
            .await?;
        let insert = "INSERT INTO msds_hazards \
                      (doc_id, product_no, h_code, ghs_code, symbol_name, evidence, signal_word, note, source) \
                      VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";
        for (c, sources, evidence) in &rows {
            let ghs: Vec<Option<&str>> =
                if c.pictograms.is_empty() { vec![None] } else { c.pictograms.iter().map(|p| Some(*p)).collect() };
            for g in ghs {
                sqlx::query(insert)
                    .bind(doc_id)
                    .bind(input.product_no)
                    .bind(&c.code)
                    .bind(g)
                    .bind(g.map(symbol_name))
                    .bind(evidence)
                    .bind(signal_word)
                    .bind(note)
                    .bind(sources[0])
                    .execute(&mut *tx)
                    .await?;
            }
        }
        if rows.is_empty() && (note.is_some() || signal_word.is_some()) {
            // H코드 없음: note(이유)와 신호어만 담은 한 행
            sqlx::query(insert)
                .bind(doc_id)
                .bind(input.product_no)
                .bind(None::<String>)
                .bind(None::<String>)
                .bind(None::<String>)
                .bind(None::<String>)
                .bind(signal_word)
                .bind(note)
                .bind(None::<String>)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;

        let pictograms: Vec<Value> = res
            .pictograms
            .iter()
            .map(|(g, codes)| json!({ "ghs_code": g, "symbol_name": symbol_name(g), "h_codes": codes }))
            .collect();
        let codes: Vec<Value> = rows
            .iter()
            .map(|(c, sources, evidence)| {
                json!({ "h_code": c.code, "sources": sources, "evidence": evidence, "pictograms": c.pictograms })
            })
            .collect();
        let suppressed: Vec<Value> =
            res.suppressed.iter().map(|(c, r)| json!({ "h_code": c, "reason": r })).collect();

        let mut out = json!({
            "doc_id": doc_id,
            "product_no": input.product_no,
            "status": if rows.is_empty() { "no_h_codes" } else { "ok" },
            "pictograms": pictograms,
            "h_codes": codes,
            "no_pictogram": res.no_pictogram,
            "unknown_codes": res.unknown_codes,
            "unknown_classifications": unknown_classifications,
            "suppressed_ghs07": suppressed,
            "warnings": warnings,
            "signal_word": signal_word,
            "note": note,
        });
        if !unknown_classifications.is_empty() {
            out["notice"] = json!(
                "표에 없는 분류·구분이 있다. 문서의 표기를 다시 확인해 hazard_class와 category를 고쳐서 다시 저장하라."
            );
        }
        Ok(out)
    }
}

#[async_trait]
impl Tool for SaveHazardsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_save_hazards".into(),
            description: "유해성 섹션의 H코드와 분류로 GHS 그림문자를 정해 제품 단위로 저장한다.\n\
                          - 문서에 적힌 H코드나 분류만 보내라. 증상 설명, HMIS·NFPA 숫자로 분류를 추측하지 마라.\n\
                          - h_codes: 문서의 H코드. 'H301+H311' 같은 묶음은 나눠 처리하고 EUH 코드는 무시한다(unknown_codes).\n\
                          - classifications: 문서의 분류표(예: SKIN CORROSION 1B). 툴이 H코드로 바꾸고, h_codes와 합친다. \
                          표에 없는 조합은 unknown_classifications로 돌려준다.\n\
                          - H코드가 없으면 문장으로 분류를 추측하지 마라. h_codes와 classifications가 둘 다 비면 \
                          note에 이유(예: 'GHS 이전 양식, HMIS만 있음')를 반드시 적는다.\n\
                          - 그림문자 변환과 GHS07 우선순위(KOSHA W-14-2022)는 툴이 처리한다.\n\
                          - 다중 제품 문서는 product_no(기본 1)로 제품마다 따로 호출한다. 다시 호출하면 그 제품의 \
                          유해성 정보만 지우고 새로 저장한다."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "product_no": { "type": "integer", "minimum": 1, "default": 1 },
                    "h_codes": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "문서에 적힌 H코드. 예: [\"H225\", \"H301+H311\"]"
                    },
                    "classifications": {
                        "type": "array",
                        "description": "문서에 적힌 유해성 분류와 구분",
                        "items": {
                            "type": "object",
                            "properties": {
                                "hazard_class": { "type": "string", "enum": HAZARD_CLASSES },
                                "category": { "type": "string", "description": "예: \"1B\", \"구분 2\", \"Category 3 (respiratory tract irritation)\"" }
                            },
                            "required": ["hazard_class", "category"]
                        }
                    },
                    "signal_word": { "type": "string", "description": "신호어 원문 (위험/경고/Danger/Warning)" },
                    "note": { "type": "string", "description": "H코드·분류가 문서에 없는 이유. 둘 다 비면 필수" }
                },
                "required": ["doc_id"]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_number_of_codes() {
        assert_eq!(base_number("H360Fd"), Some(360));
        assert_eq!(base_number("H314"), Some(314));
        assert_eq!(base_number("H"), None);
    }
}
