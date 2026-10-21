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
use uuid::Uuid;

use crate::pdf;

const UPLOAD_DIR: &str = "data/uploads";
const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;
const PREVIEW_CHARS: usize = 300;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/api/msds/upload",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/api/msds/:id/pages", get(pages))
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
    if !bytes.starts_with(b"%PDF-") {
        return Err(bad_request("PDF 파일이 아닙니다 (내용이 %PDF-로 시작하지 않음)"));
    }

    let file_path = format!("{UPLOAD_DIR}/{doc_id}.pdf");
    tokio::fs::create_dir_all(UPLOAD_DIR).await.map_err(internal)?;
    tokio::fs::write(&file_path, &bytes).await.map_err(internal)?;

    sqlx::query("INSERT INTO msds_documents (id, file_name, file_path) VALUES ($1, $2, $3)")
        .bind(doc_id)
        .bind(&file_name)
        .bind(&file_path)
        .execute(&st.pool)
        .await
        .map_err(internal)?;

    let pages = match pdf::extract_pages(bytes).await {
        Ok(pages) => pages,
        Err(e) => {
            tracing::warn!(%doc_id, "{e}");
            sqlx::query("UPDATE msds_documents SET status = 'failed', error = $2, updated_at = now() WHERE id = $1")
                .bind(doc_id)
                .bind(&e)
                .execute(&st.pool)
                .await
                .map_err(internal)?;
            return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, e));
        }
    };

    let language = pdf::detect_language(&pages.concat());
    let page_count = pages.len() as i32;

    let mut tx = st.pool.begin().await.map_err(internal)?;
    for (i, text) in pages.iter().enumerate() {
        // PostgreSQL TEXT는 NUL 문자를 저장할 수 없다
        let text = text.replace('\0', "");
        sqlx::query("INSERT INTO msds_pages (doc_id, page_no, text) VALUES ($1, $2, $3)")
            .bind(doc_id)
            .bind(i as i32 + 1)
            .bind(text)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
    }
    sqlx::query("UPDATE msds_documents SET language = $2, page_count = $3, updated_at = now() WHERE id = $1")
        .bind(doc_id)
        .bind(language)
        .bind(page_count)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    tracing::info!(%doc_id, page_count, language, "업로드 완료");
    Ok(Json(json!({
        "doc_id": doc_id,
        "file_name": file_name,
        "page_count": page_count,
        "language": language,
    })))
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
