//! 서버 API 호출과 응답 타입 (trunk proxy로 같은 출처의 /api를 부른다)

use gloo_net::http::{Request, Response};
use serde::Deserialize;

#[derive(Clone, PartialEq, Deserialize)]
pub struct DocSummary {
    pub id: String,
    pub file_name: Option<String>,
    pub language: Option<String>,
    pub page_count: Option<i32>,
    pub status: String,
    pub error: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct Component {
    pub component: Option<String>,
    pub cas_no: Option<String>,
    pub cas_valid: Option<bool>,
    pub percent: Option<String>,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct Pictogram {
    pub ghs_code: String,
    pub symbol_name: String,
    pub h_codes: Vec<String>,
    pub sources: Vec<String>,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct Product {
    pub product_no: i32,
    pub product_name: Option<String>,
    pub components: Vec<Component>,
    pub pictograms: Vec<Pictogram>,
    pub h_codes_without_pictogram: Vec<String>,
    pub signal_word: Option<String>,
    pub note: Option<String>,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct Translation {
    pub product_no: i32,
    pub section_no: i32,
    pub title_ko: Option<String>,
    pub text_ko: String,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct AgentRun {
    pub agent: String,
    pub ok: bool,
    pub error: Option<String>,
    pub seconds: Option<f64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub rounds: Option<i32>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct DocDetail {
    #[serde(flatten)]
    pub doc: DocSummary,
    pub products: Vec<Product>,
    pub translations: Vec<Translation>,
    pub agents: Vec<AgentRun>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

async fn error_message(resp: &Response) -> String {
    match resp.json::<ErrorBody>().await {
        Ok(e) => e.error,
        Err(_) => format!("요청 실패 (HTTP {})", resp.status()),
    }
}

async fn get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, String> {
    let resp = Request::get(url).send().await.map_err(|e| format!("서버에 연결하지 못했습니다: {e}"))?;
    if !resp.ok() {
        return Err(error_message(&resp).await);
    }
    resp.json().await.map_err(|e| format!("응답을 읽지 못했습니다: {e}"))
}

pub async fn list_docs() -> Result<Vec<DocSummary>, String> {
    get_json("/api/msds").await
}

pub async fn get_doc(id: &str) -> Result<DocDetail, String> {
    get_json(&format!("/api/msds/{id}")).await
}

/// 업로드: 검증 실패(400)는 서버의 error 메시지를 돌려준다
pub async fn upload(file: &web_sys::File) -> Result<(), String> {
    let form = web_sys::FormData::new().map_err(|_| "FormData를 만들지 못했습니다".to_string())?;
    form.append_with_blob_and_filename("file", file, &file.name())
        .map_err(|_| "파일을 담지 못했습니다".to_string())?;
    let resp = Request::post("/api/msds/upload")
        .body(form)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| format!("서버에 연결하지 못했습니다: {e}"))?;
    if resp.ok() {
        Ok(())
    } else {
        Err(error_message(&resp).await)
    }
}
