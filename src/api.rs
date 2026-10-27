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
use pekko_actor::ActorRef;
use uuid::Uuid;

use crate::actors::supervisor::{Supervisor, SupervisorMsg};
use crate::validate;
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
        .route("/api/msds/:id/pages", get(pages))
        .route("/api/msds/:id/analyze", post(analyze))
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

async fn upload(State(st): State<AppState>, mut multipart: Multipart) -> Result<Json<Value>, ApiError> {
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
    queue_analysis(&st, doc_id).await?;
    Ok(Json(json!({
        "doc_id": doc_id,
        "file_name": file_name,
        "page_count": page_count,
        "language": language,
        "status": "queued",
    })))
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
