//! 화면 표기 (상태·에이전트 이름, 시간 형식)

/// 목록·상세를 다시 불러오는 간격
pub const REFRESH_MS: u32 = 3_000;

pub fn status_label(status: &str) -> &'static str {
    match status {
        "queued" => "대기",
        "analyzing" => "분석 중",
        "done" => "완료",
        "partial" => "부분 완료",
        "failed" => "실패",
        "uploaded" => "업로드됨",
        _ => "알 수 없음",
    }
}

/// 상태 배지 색 (Tailwind가 찾을 수 있게 클래스 이름을 그대로 적는다)
pub fn status_classes(status: &str) -> &'static str {
    match status {
        "queued" => "bg-slate-200 text-slate-700",
        "analyzing" => "bg-blue-100 text-blue-800 animate-pulse",
        "done" => "bg-emerald-100 text-emerald-800",
        "partial" => "bg-amber-100 text-amber-800",
        "failed" => "bg-red-100 text-red-800",
        _ => "bg-slate-100 text-slate-500",
    }
}

/// 분석이 아직 끝나지 않은 상태 (상세 화면을 다시 불러온다)
pub fn is_active(status: &str) -> bool {
    matches!(status, "queued" | "analyzing")
}

pub fn agent_label(agent: &str) -> &'static str {
    match agent {
        "extract" => "추출",
        "components" => "성분",
        "hazards" => "그림문자",
        "translate" => "번역",
        _ => "기타",
    }
}

pub fn source_label(source: &str) -> &'static str {
    match source {
        "document" => "문서",
        "classification" => "분류에서 변환",
        _ => "기타",
    }
}

/// RFC 3339 시각을 브라우저 지역 시간으로
pub fn format_time(value: &Option<String>) -> String {
    let Some(v) = value else { return "-".into() };
    let date = js_sys::Date::new(&v.as_str().into());
    if date.get_time().is_nan() {
        return v.clone();
    }
    String::from(date.to_locale_string("ko-KR", &wasm_bindgen::JsValue::UNDEFINED))
}
