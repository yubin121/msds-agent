use std::collections::BTreeMap;

use async_trait::async_trait;
use pekko_agent_core::{Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::common::{check_section_no, ensure_doc, finish, first_product, input_err, parse_input, Fail};
use super::headings::{body_is_empty, check_products, is_long_section, slice_sections, LONG_SECTION_CHARS};

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
    #[serde(default = "first_product")]
    product_no: i32,
    #[serde(default)]
    product_name: Option<String>,
    section_no: i32,
    #[serde(default)]
    original_no: Option<i32>,
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
        for s in &input.sections {
            check_section_no(s.section_no)?;
        }
        let keys: Vec<(i32, i32)> = input.sections.iter().map(|s| (s.product_no, s.section_no)).collect();
        check_products(&keys).map_err(Fail::Input)?;

        let pages: Vec<(i32, Option<String>)> =
            sqlx::query_as("SELECT page_no, text FROM msds_pages WHERE doc_id = $1 ORDER BY page_no")
                .bind(doc_id)
                .fetch_all(&self.pool)
                .await?;
        let pages: Vec<(i32, String)> = pages.into_iter().map(|(p, t)| (p, t.unwrap_or_default())).collect();
        let starts: Vec<(i32, i32)> = input.sections.iter().map(|s| (s.page_no, s.line_no)).collect();
        let texts = slice_sections(&pages, &starts).map_err(Fail::Input)?;

        // 제품 이름은 그 제품 항목 중 처음 적힌 값
        let mut names: BTreeMap<i32, String> = BTreeMap::new();
        for s in &input.sections {
            if let Some(n) = s.product_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                names.entry(s.product_no).or_insert_with(|| n.to_string());
            }
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM msds_sections WHERE doc_id = $1").bind(doc_id).execute(&mut *tx).await?;
        for (s, text) in input.sections.iter().zip(&texts) {
            sqlx::query(
                "INSERT INTO msds_sections \
                 (doc_id, product_no, product_name, section_no, original_no, title, text, page_no, line_no) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(doc_id)
            .bind(s.product_no)
            .bind(names.get(&s.product_no))
            .bind(s.section_no)
            .bind(s.original_no)
            .bind(s.title.trim())
            .bind(text)
            .bind(s.page_no)
            .bind(s.line_no)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        // 제품별 결과
        let mut products: BTreeMap<i32, Vec<(&SectionStart, &String)>> = BTreeMap::new();
        for (s, t) in input.sections.iter().zip(&texts) {
            products.entry(s.product_no).or_default().push((s, t));
        }
        let products: Vec<Value> = products
            .iter()
            .map(|(&product_no, items)| {
                let mut warnings = Vec::new();
                let sections: Vec<Value> = items
                    .iter()
                    .map(|(s, t)| {
                        let chars = t.chars().count();
                        if body_is_empty(t) {
                            warnings.push(format!(
                                "{}번 섹션은 제목 줄 외에 내용이 없습니다. 위치가 맞는지 확인하라",
                                s.section_no
                            ));
                        }
                        if is_long_section(chars) {
                            warnings.push(format!(
                                "{}번 섹션이 {chars}자로 {LONG_SECTION_CHARS}자를 넘는다. \
                                 다음 제품이나 다음 섹션 제목을 빠뜨렸는지 확인하라",
                                s.section_no
                            ));
                        }
                        json!({
                            "section_no": s.section_no,
                            "original_no": s.original_no,
                            "title": s.title.trim(),
                            "chars": chars,
                        })
                    })
                    .collect();
                let missing: Vec<i32> =
                    (1..=16).filter(|n| !items.iter().any(|(s, _)| s.section_no == *n)).collect();
                json!({
                    "product_no": product_no,
                    "product_name": names.get(&product_no),
                    "sections": sections,
                    "missing_sections": missing,
                    "warnings": warnings,
                })
            })
            .collect();

        Ok(json!({ "doc_id": doc_id, "products": products }))
    }
}

#[async_trait]
impl Tool for SaveSectionsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "msds_save_sections".into(),
            description: "섹션 제목의 위치(page_no, line_no)를 받아 각 위치부터 다음 제목 전까지의 본문을 페이지 텍스트에서 \
                          잘라 저장한다. 본문은 보내지 않는다. 항목은 문서 위치 순서(오름차순)로 보낸다.\n\
                          - section_no는 문서에 찍힌 번호가 아니라 제목의 뜻에 따른 GHS 표준 번호(1~16)다. \
                          예: 옛 양식의 '2. Composition'은 section_no 3, original_no 2이고 '3. Hazards'는 section_no 2, \
                          original_no 3이다. 번호가 표준과 같으면 original_no는 생략한다.\n\
                          - 한 PDF에 제품이 여러 개면 product_no를 1부터 위치 순서대로 붙인다. 단일 제품이면 product_no는 \
                          생략한다(기본 1). product_name은 단일 제품이어도 그 제품 1번 섹션의 제품명을 적는다(모르면 생략). \
                          같은 제품 안에서 section_no는 중복될 수 없다.\n\
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
                                "product_no": { "type": "integer", "minimum": 1, "default": 1 },
                                "product_name": { "type": "string" },
                                "section_no": { "type": "integer", "minimum": 1, "maximum": 16,
                                                "description": "제목의 뜻에 따른 GHS 표준 번호" },
                                "original_no": { "type": "integer", "description": "문서에 찍힌 번호(표준과 다를 때만)" },
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
