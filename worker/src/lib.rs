//! Cloudflare Worker 앞단: /api 요청을 서버로 넘긴다.
//! 업로드(/api/msds/upload, /api/legacy/casNoExtraction)는 먼저 파일 검사(시그니처·크기)를 하고,
//! 걸리면 서버로 보내지 않고 서버와 같은 모양의 오류로 응답한다(서버 DB에도 남지 않는다).
//! 텍스트 검사(글자 수·섹션 번호)는 PDF 텍스트 추출이 필요해 서버에서만 한다.
//! Worker 메모리는 isolate당 128MB인데 본문은 100MB까지 들어오므로, 본문을 읽기 전에 Content-Length로 먼저 거른다.

use worker::*;

const UPLOAD_PATH: &str = "/api/msds/upload";
const LEGACY_UPLOAD_PATH: &str = "/api/legacy/casNoExtraction";

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let server = env.var("SERVER_URL")?.to_string();
    let path = req.path();
    if !path.starts_with("/api/") {
        return Response::error("Not Found", 404);
    }

    let legacy = path == LEGACY_UPLOAD_PATH;
    if req.method() == Method::Post && (path == UPLOAD_PATH || legacy) {
        // 본문(최대 100MB)과 복제본을 메모리에 올리기 전에 서버 본문 한도(21MB)를 넘는 요청을 거른다
        let length = req.headers().get("Content-Length")?;
        if let Err(e) = msds_validate::validate_content_length(length.as_deref()) {
            console_log!("업로드 거부(본문 읽기 전, 서버로 보내지 않음): {path} Content-Length={length:?}");
            return reject(&e.message(), legacy);
        }
        // 원본 본문을 그대로 넘기려고 먼저 복제한다(파일 이름 인코딩 등을 바꾸지 않게)
        let original = req.clone()?;
        if let Some(message) = file_error(&mut req).await? {
            console_log!("업로드 거부(서버로 보내지 않음): {path} {message}");
            return reject(&message, legacy);
        }
        return forward(original, &server, legacy).await;
    }
    forward(req, &server, legacy).await
}

/// file 필드의 시그니처·크기 검사. file 필드가 없으면 서버가 판단하게 둔다
async fn file_error(req: &mut Request) -> Result<Option<String>> {
    let form = req.form_data().await?;
    let Some(FormEntry::File(file)) = form.get("file") else { return Ok(None) };
    let bytes = file.bytes().await?;
    Ok(msds_validate::validate_file(&bytes).err().map(|e| e.message()))
}

/// 서버와 같은 모양: 업로드는 HTTP 400 {"error"}, casNoExtraction은 기존 형식 HTTP 200 {"status":400,"error"}
fn reject(message: &str, legacy: bool) -> Result<Response> {
    if legacy {
        Response::from_json(&serde_json::json!({ "status": 400, "error": message }))
    } else {
        Ok(Response::from_json(&serde_json::json!({ "error": message }))?.with_status(400))
    }
}

const UNREACHABLE: &str = "서버에 연결할 수 없습니다";

/// 서버에 연결하지 못함: 502 {"error"}, casNoExtraction은 기존 형식 HTTP 200 {"status":500,"error"}
fn unreachable(legacy: bool) -> Result<Response> {
    if legacy {
        Response::from_json(&serde_json::json!({ "status": 500, "error": UNREACHABLE }))
    } else {
        Ok(Response::from_json(&serde_json::json!({ "error": UNREACHABLE }))?.with_status(502))
    }
}

/// 메서드·헤더·본문·쿼리를 그대로 서버로 보내고 응답을 그대로 돌려준다
async fn forward(mut req: Request, server: &str, legacy: bool) -> Result<Response> {
    let url = req.url()?;
    let mut target = format!("{}{}", server.trim_end_matches('/'), url.path());
    if let Some(q) = url.query() {
        target.push('?');
        target.push_str(q);
    }
    let method = req.method();
    let mut init = RequestInit::new();
    init.with_method(method.clone()).with_headers(req.headers().clone());
    if !matches!(method, Method::Get | Method::Head) {
        let body = req.bytes().await?;
        init.with_body(Some(js_sys::Uint8Array::from(body.as_slice()).into()));
    }
    match Fetch::Request(Request::new_with_init(&target, &init)?).send().await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            console_log!("서버 연결 실패: {target} {e}");
            unreachable(legacy)
        }
    }
}
