use axum::{
    extract::{
        multipart::{Field, MultipartError},
        DefaultBodyLimit, Multipart, Path, State,
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use tower_http::cors::CorsLayer;
use std::time::Duration;

use chrono::{DateTime, Utc};
use pekko_actor::ActorRef;
use uuid::Uuid;

use crate::actors::supervisor::{Supervisor, SupervisorMsg};
use crate::{api_view, validate};
use crate::{db, pdf};

const UPLOAD_DIR: &str = "data/uploads";
/// 요청 본문 한도. 파일 한도(validate::MAX_UPLOAD_BYTES, 20MB)에 multipart 헤더 여유를 더해
/// 20MB를 조금 넘는 파일도 413이 아니라 validate_file의 400으로 거른다
const MAX_BODY_BYTES: usize = 21 * 1024 * 1024;
const PREVIEW_CHARS: usize = 300;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub supervisor: ActorRef<Supervisor>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/api/msds/upload",
            post(upload).layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
        )
        .route("/api/msds", get(list))
        .route("/api/msds/:id", get(detail))
        .route("/api/msds/:id/legacy", get(legacy))
        .route("/api/msds/:id/pages", get(pages))
        .route("/api/msds/:id/analyze", post(analyze))
        .route(
            "/api/legacy/casNoExtraction",
            post(legacy_extract).layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
        )
        .layer(CorsLayer::permissive())
        .with_state(state)
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!("{e}");
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn multipart_error(e: MultipartError) -> ApiError {
    let msg = format!("multipart 파싱 오류: {}", e.body_text());
    tracing::warn!("{msg}");
    ApiError(e.status(), msg)
}

async fn upload(State(st): State<AppState>, multipart: Multipart) -> Result<Json<Value>, ApiError> {
    let doc = receive_upload(&st, multipart).await?;
    queue_analysis(&st, doc.doc_id).await?;
    Ok(Json(json!({
        "doc_id": doc.doc_id,
        "file_name": doc.file_name,
        "page_count": doc.page_count,
        "language": doc.language,
        "status": "queued",
    })))
}

struct Uploaded {
    doc_id: Uuid,
    file_name: String,
    page_count: i32,
    language: &'static str,
}

/// 업로드 공통: 파일 받기 → 검증 → 저장 → 페이지 추출. 검증 실패는 400(문서는 failed로 남김)
async fn receive_upload(st: &AppState, mut multipart: Multipart) -> Result<Uploaded, ApiError> {
    let doc_id = Uuid::new_v4();

    // 파일 파트는 필드 이름 "file"로 찾는다.
    // 파일 이름이 UTF-8이 아니면 field.file_name()이 None이므로 그걸로 판단하지 않는다.
    let mut file: Option<(String, Vec<u8>)> = None;
    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        if field.name() != Some("file") {
            continue;
        }
        let name = upload_file_name(&field, doc_id);
        let data = field.bytes().await.map_err(multipart_error)?;
        file = Some((name, data.to_vec()));
        break;
    }
    let (file_name, bytes) = file.ok_or_else(|| bad_request("\"file\" 필드가 없습니다"))?;

    // 문서 행을 먼저 만들어 검증 실패도 status 'failed'와 이유로 남긴다
    sqlx::query("INSERT INTO msds_documents (id, file_name) VALUES ($1, $2)")
        .bind(doc_id)
        .bind(&file_name)
        .execute(&st.pool)
        .await
        .map_err(internal)?;

    // 검증 1~2: 시그니처, 크기 (PDF가 아니면 디스크에 저장하지 않는다)
    if let Err(e) = validate::validate_file(&bytes) {
        return Err(reject(&st.pool, doc_id, &e.message()).await);
    }

    let file_path = format!("{UPLOAD_DIR}/{doc_id}.pdf");
    tokio::fs::create_dir_all(UPLOAD_DIR).await.map_err(internal)?;
    tokio::fs::write(&file_path, &bytes).await.map_err(internal)?;
    sqlx::query("UPDATE msds_documents SET file_path = $2 WHERE id = $1")
        .bind(doc_id)
        .bind(&file_path)
        .execute(&st.pool)
        .await
        .map_err(internal)?;

    let extracted = match pdf::extract_pages(bytes).await {
        Ok(x) => x,
        Err(e) => return Err(reject(&st.pool, doc_id, &e).await),
    };
    let saved = db::save_pages(&st.pool, doc_id, &extracted.pages).await.map_err(internal)?;
    let (page_count, language) = (saved.page_count, saved.language);

    // 검증 3~4: 텍스트 글자 수, MSDS 여부
    if let Err(e) = validate::validate_text(&extracted.pages) {
        tracing::warn!(%doc_id, chars = saved.chars, ?e, "업로드 검증 실패");
        return Err(reject(&st.pool, doc_id, &e.message()).await);
    }

    tracing::info!(%doc_id, page_count, language, fallback = extracted.fallback, "업로드 완료");
    Ok(Uploaded { doc_id, file_name, page_count, language })
}

/// 이미 올린 문서를 다시 분석한다. 분석 중이거나 대기 중이면 409.
async fn analyze(State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<(StatusCode, Json<Value>), ApiError> {
    let status: Option<(String,)> = sqlx::query_as("SELECT status FROM msds_documents WHERE id = $1")
        .bind(id)
        .fetch_optional(&st.pool)
        .await
        .map_err(internal)?;
    let Some((status,)) = status else {
        return Err(ApiError(StatusCode::NOT_FOUND, "문서를 찾을 수 없습니다".into()));
    };
    if matches!(status.as_str(), "analyzing" | "queued") {
        return Err(ApiError(StatusCode::CONFLICT, "이미 분석 중인 문서입니다".into()));
    }
    queue_analysis(&st, id).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "doc_id": id, "status": "queued" }))))
}

/// status를 'queued'로 저장하고 총괄 액터에 Analyze를 보낸다(응답은 기다리지 않음)
async fn queue_analysis(st: &AppState, doc_id: Uuid) -> Result<(), ApiError> {
    db::set_status(&st.pool, doc_id, "queued", None).await.map_err(internal)?;
    st.supervisor.tell(SupervisorMsg::Analyze { doc_id }).map_err(|e| internal(format!("총괄 액터에 Analyze 전송 실패: {e}")))
}

type DocRow = (Uuid, Option<String>, Option<String>, Option<i32>, String, Option<String>, Option<DateTime<Utc>>);

const DOC_COLUMNS: &str = "id, file_name, language, page_count, status, error, created_at";
/// 분석이 끝난 상태 (casNoExtraction이 기다리는 대상)
const FINAL_STATUSES: [&str; 3] = ["done", "partial", "failed"];
/// casNoExtraction이 판정을 확인하는 간격
const LEGACY_POLL: Duration = Duration::from_millis(500);
/// 총괄 액터의 타이머는 Analyze를 받은 뒤에 시작하므로, 기다리는 시간에 여유를 둔다
const LEGACY_WAIT_MARGIN: Duration = Duration::from_secs(10);

fn doc_json((id, file_name, language, page_count, status, error, created_at): DocRow) -> Value {
    json!({
        "id": id,
        "file_name": file_name,
        "language": language,
        "page_count": page_count,
        "status": status,
        "error": error,
        "created_at": created_at,
    })
}

async fn find_doc(pool: &PgPool, id: Uuid) -> Result<DocRow, ApiError> {
    sqlx::query_as(&format!("SELECT {DOC_COLUMNS} FROM msds_documents WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "문서를 찾을 수 없습니다".into()))
}

/// 문서 목록 (최신순)
async fn list(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows: Vec<DocRow> =
        sqlx::query_as(&format!("SELECT {DOC_COLUMNS} FROM msds_documents ORDER BY created_at DESC NULLS LAST, id"))
            .fetch_all(&st.pool)
            .await
            .map_err(internal)?;
    Ok(Json(Value::Array(rows.into_iter().map(doc_json).collect())))
}

/// 제품 목록, 성분, 유해성 행 (상세·legacy 공용)
async fn load_results(
    pool: &PgPool,
    id: Uuid,
) -> Result<(Vec<(i32, Option<String>)>, Vec<api_view::ComponentRow>, Vec<api_view::HazardRow>), ApiError> {
    let sections: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT product_no, max(product_name) FROM msds_sections WHERE doc_id = $1 GROUP BY product_no ORDER BY product_no",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(internal)?;
    let components = sqlx::query(
        "SELECT product_no, product_name, component, cas_no, percent, cas_valid FROM msds_components \
         WHERE doc_id = $1 ORDER BY product_no, id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(internal)?
    .iter()
    .map(|r| api_view::ComponentRow {
        product_no: r.get("product_no"),
        product_name: r.get("product_name"),
        component: r.get("component"),
        cas_no: r.get("cas_no"),
        percent: r.get("percent"),
        cas_valid: r.get("cas_valid"),
    })
    .collect::<Vec<_>>();
    let hazards = sqlx::query(
        "SELECT product_no, h_code, ghs_code, symbol_name, source, signal_word, note FROM msds_hazards \
         WHERE doc_id = $1 ORDER BY product_no, id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(internal)?
    .iter()
    .map(|r| api_view::HazardRow {
        product_no: r.get("product_no"),
        h_code: r.get("h_code"),
        ghs_code: r.get("ghs_code"),
        symbol_name: r.get("symbol_name"),
        source: r.get("source"),
        signal_word: r.get("signal_word"),
        note: r.get("note"),
    })
    .collect::<Vec<_>>();
    let products = api_view::product_list(&sections, &components, &hazards);
    Ok((products, components, hazards))
}

/// 에이전트별 마지막 실행: 가장 최근 총괄 실행(pipeline_run_id)의 행, 없으면 에이전트마다 최신 행
async fn last_agent_runs(pool: &PgPool, id: Uuid) -> Result<Vec<Value>, ApiError> {
    let columns = "agent, model, ok, error, rounds, input_tokens, output_tokens, started_at, finished_at";
    let rows: Vec<(String, Option<String>, bool, Option<String>, Option<i32>, Option<i32>, Option<i32>, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> =
        sqlx::query_as(&format!(
            "SELECT {columns} FROM msds_agent_runs WHERE doc_id = $1 AND pipeline_run_id = \
             (SELECT pipeline_run_id FROM msds_agent_runs WHERE doc_id = $1 AND pipeline_run_id IS NOT NULL \
              ORDER BY id DESC LIMIT 1) ORDER BY id"
        ))
        .bind(id)
        .fetch_all(pool)
        .await
        .map_err(internal)?;
    let rows = if rows.is_empty() {
        sqlx::query_as(&format!(
            "SELECT {columns} FROM (SELECT DISTINCT ON (agent) * FROM msds_agent_runs WHERE doc_id = $1 \
             ORDER BY agent, id DESC) r ORDER BY started_at"
        ))
        .bind(id)
        .fetch_all(pool)
        .await
        .map_err(internal)?
    } else {
        rows
    };
    Ok(rows
        .into_iter()
        .map(|(agent, model, ok, error, rounds, input, output, started_at, finished_at)| {
            let (input, output) = (input.unwrap_or(0), output.unwrap_or(0));
            let seconds = started_at
                .zip(finished_at)
                .map(|(s, f)| (f - s).num_milliseconds() as f64 / 1000.0);
            let cost = crate::cost::estimate(model.as_deref().unwrap_or(""), input as u64, output as u64);
            json!({
                "agent": agent,
                "ok": ok,
                "error": error,
                "seconds": seconds,
                "rounds": rounds,
                "input_tokens": input,
                "output_tokens": output,
                "model": model,
                "cost_usd": cost,
            })
        })
        .collect())
}

/// 문서 상세: 제품별 성분·그림문자, 섹션별 번역, 에이전트별 마지막 실행
async fn detail(State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiError> {
    let doc = find_doc(&st.pool, id).await?;
    let (products, components, hazards) = load_results(&st.pool, id).await?;
    let translations: Vec<(i32, i32, i32, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT product_no, section_no, chunk_no, title_ko, text_ko FROM msds_translations \
         WHERE doc_id = $1 ORDER BY product_no, section_no, chunk_no",
    )
    .bind(id)
    .fetch_all(&st.pool)
    .await
    .map_err(internal)?;

    let mut body = doc_json(doc);
    body["products"] = Value::Array(api_view::detail_products(&products, &components, &hazards));
    body["translations"] = Value::Array(api_view::merge_translations(&translations));
    body["agents"] = Value::Array(last_agent_runs(&st.pool, id).await?);
    Ok(Json(body))
}

/// 기존 서비스(5001)와 같은 모양의 응답
async fn legacy(State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiError> {
    legacy_response(&st.pool, id).await.map(Json)
}

async fn legacy_response(pool: &PgPool, id: Uuid) -> Result<Value, ApiError> {
    let (_, _, _, _, status, error, _) = find_doc(pool, id).await?;
    match status.as_str() {
        "done" | "partial" => {
            let (products, components, hazards) = load_results(pool, id).await?;
            let products = api_view::legacy_products(&products, &components);
            Ok(api_view::legacy_body(&products, &hazards))
        }
        "failed" => {
            let (runs,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM msds_agent_runs WHERE doc_id = $1 AND pipeline_run_id IS NOT NULL",
            )
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(internal)?;
            Ok(json!({ "status": 400, "error": api_view::legacy_failed_error(error.as_deref(), runs > 0) }))
        }
        // queued·analyzing·uploaded는 기존 서비스에 없는 상태다
        _ => Err(ApiError(StatusCode::CONFLICT, "분석 중입니다".into())),
    }
}

/// 기존 host-runner 호환(동기식): 업로드 → 분석 → 판정까지 기다린 뒤 legacy 모양으로 응답한다.
/// companycode·companyname은 기존처럼 받지만 쓰지 않는다(기존은 통계 저장용)
async fn legacy_extract(State(st): State<AppState>, multipart: Multipart) -> Result<Json<Value>, ApiError> {
    let doc = match receive_upload(&st, multipart).await {
        Ok(d) => d,
        // 검증 실패도 기존처럼 HTTP 200에 본문 status 400
        Err(ApiError(code, msg)) if code.is_client_error() => {
            return Ok(Json(json!({ "status": 400, "error": msg })));
        }
        Err(e) => return Err(e),
    };
    queue_analysis(&st, doc.doc_id).await?;

    let timeout = crate::actors::supervisor::parse_timeout_secs(std::env::var("MSDS_DOC_TIMEOUT_SECS").ok().as_deref());
    let wait_until = tokio::time::Instant::now() + Duration::from_secs(timeout) + LEGACY_WAIT_MARGIN;
    loop {
        let (status,): (String,) = sqlx::query_as("SELECT status FROM msds_documents WHERE id = $1")
            .bind(doc.doc_id)
            .fetch_one(&st.pool)
            .await
            .map_err(internal)?;
        if FINAL_STATUSES.contains(&status.as_str()) || tokio::time::Instant::now() >= wait_until {
            break;
        }
        tokio::time::sleep(LEGACY_POLL).await;
    }
    tracing::info!(doc_id = %doc.doc_id, "casNoExtraction 응답");
    legacy_response(&st.pool, doc.doc_id).await.map(Json)
}

/// 업로드 검증 실패: 문서를 failed로 남기고 400으로 응답한다
async fn reject(pool: &PgPool, doc_id: Uuid, msg: &str) -> ApiError {
    tracing::warn!(%doc_id, "업로드 거부: {msg}");
    if let Err(e) = db::mark_failed(pool, doc_id, msg).await {
        return internal(e);
    }
    bad_request(msg)
}

/// 파일 이름: file_name() → Content-Disposition 원본 바이트(UTF-8 → CP949) → upload-{uuid}.pdf
fn upload_file_name(field: &Field<'_>, doc_id: Uuid) -> String {
    if let Some(name) = field.file_name().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    field
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| raw_filename(v.as_bytes()))
        .and_then(decode_filename)
        .unwrap_or_else(|| format!("upload-{doc_id}.pdf"))
}

/// Content-Disposition 값에서 filename 파라미터의 원본 바이트를 꺼낸다.
fn raw_filename(header: &[u8]) -> Option<Vec<u8>> {
    const KEY: &[u8] = b"filename";
    let mut start = 0;
    while let Some(pos) = header[start..].windows(KEY.len()).position(|w| w.eq_ignore_ascii_case(KEY)) {
        let i = start + pos;
        start = i + KEY.len();
        // "xfilename" 같은 다른 파라미터는 건너뛴다
        if i > 0 && !(header[i - 1].is_ascii_whitespace() || header[i - 1] == b';') {
            continue;
        }
        let rest = header[start..].trim_ascii_start();
        // "filename*=" 등은 건너뛴다
        let Some(rest) = rest.strip_prefix(b"=") else { continue };
        let rest = rest.trim_ascii_start();

        if let Some(quoted) = rest.strip_prefix(b"\"") {
            let mut out = Vec::new();
            let mut bytes = quoted.iter();
            while let Some(&b) = bytes.next() {
                match b {
                    b'"' => return Some(out),
                    b'\\' => out.push(*bytes.next()?),
                    _ => out.push(b),
                }
            }
            return None; // 닫는 따옴표 없음
        }
        let end = rest.iter().position(|&b| b == b';').unwrap_or(rest.len());
        return Some(rest[..end].trim_ascii_end().to_vec());
    }
    None
}

/// UTF-8로 읽고, 안 되면 CP949(encoding_rs의 EUC_KR = windows-949)로 읽는다.
fn decode_filename(bytes: Vec<u8>) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let bytes = match String::from_utf8(bytes) {
        Ok(s) => return Some(s),
        Err(e) => e.into_bytes(),
    };
    encoding_rs::EUC_KR
        .decode_without_bom_handling_and_without_replacement(&bytes)
        .map(|s| s.into_owned())
}

async fn pages(State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query("SELECT page_no, text FROM msds_pages WHERE doc_id = $1 ORDER BY page_no")
        .bind(id)
        .fetch_all(&st.pool)
        .await
        .map_err(internal)?;
    if rows.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, "문서를 찾을 수 없습니다".into()));
    }

    let pages: Vec<Value> = rows
        .iter()
        .map(|r| {
            let text: String = r.get::<Option<String>, _>("text").unwrap_or_default();
            // 문자 경계에서 자른다
            let preview: String = text.chars().take(PREVIEW_CHARS).collect();
            json!({ "page_no": r.get::<i32, _>("page_no"), "preview": preview })
        })
        .collect();
    Ok(Json(json!({ "doc_id": id, "pages": pages })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_utf8() {
        let h = "form-data; name=\"file\"; filename=\"물질안전®.pdf\"".as_bytes();
        assert_eq!(decode_filename(raw_filename(h).unwrap()).unwrap(), "물질안전®.pdf");
    }

    #[test]
    fn filename_cp949() {
        let (name, _, _) = encoding_rs::EUC_KR.encode("물질안전®.pdf");
        let mut h = b"form-data; name=\"file\"; filename=\"".to_vec();
        h.extend_from_slice(&name);
        h.push(b'"');
        assert_eq!(decode_filename(raw_filename(&h).unwrap()).unwrap(), "물질안전®.pdf");
    }

    #[test]
    fn filename_unquoted_and_missing() {
        assert_eq!(raw_filename(b"form-data; name=file; filename=a.pdf").unwrap(), b"a.pdf");
        assert!(raw_filename(b"form-data; name=\"file\"").is_none());
        assert!(raw_filename(b"form-data; filename*=UTF-8''a.pdf").is_none());
        assert!(decode_filename(Vec::new()).is_none());
    }
}
