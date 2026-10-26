mod actors;
mod agent_test;
mod api;
mod db;
mod pdf;
mod prompts;
mod tool_loop;
mod tools;
mod validate;

use std::sync::Arc;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    // 로그는 stderr로 보내 tool-test의 JSON 출력(stdout)과 섞이지 않게 한다
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);

    // tool-test, reextract, validate-docs는 LLM을 쓰지 않으므로 CLAUDE_MODEL 검사를 건너뛴다
    let model = std::env::var("CLAUDE_MODEL").unwrap_or_default();
    if !matches!(command, Some("tool-test" | "reextract" | "validate-docs")) && model.trim().is_empty() {
        tracing::error!("CLAUDE_MODEL이 비어 있습니다. .env를 확인하세요");
        std::process::exit(1);
    }

    let pool = match db::connect().await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("{e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = db::migrate(&pool).await {
        tracing::error!("마이그레이션 실패: {e}");
        std::process::exit(1);
    }

    match command {
        // cargo run -- loop-test <doc_id> : 서버 없이 툴 루프만 확인하고 종료
        Some("loop-test") => {
            let Some(doc_id) = args.get(1) else {
                tracing::error!("사용법: cargo run -- loop-test <doc_id>");
                std::process::exit(2);
            };
            if let Err(e) = loop_test(pool, doc_id).await {
                tracing::error!("{e}");
                std::process::exit(1);
            }
            return;
        }
        // cargo run -- agent-test <extract|components|both> <doc_id> : 에이전트 액터를 실행하고 결과 출력
        Some("agent-test") => {
            let (Some(mode), Some(doc_id)) = (args.get(1), args.get(2)) else {
                tracing::error!("사용법: cargo run -- agent-test <extract|components|both> <doc_id>");
                std::process::exit(2);
            };
            if let Err(e) = agent_test::run(pool, mode, doc_id).await {
                tracing::error!("{e}");
                std::process::exit(1);
            }
            return;
        }
        // cargo run -- tool-test <툴이름> '<JSON>' | @파일.json : LLM 없이 툴 하나만 실행
        Some("tool-test") => {
            let (Some(name), Some(raw)) = (args.get(1), args.get(2)) else {
                tracing::error!("사용법: cargo run -- tool-test <툴이름> '<JSON입력>' (또는 @파일.json)");
                std::process::exit(2);
            };
            if let Err(e) = tool_test(pool, name, raw).await {
                tracing::error!("{e}");
                std::process::exit(1);
            }
            return;
        }
        // cargo run -- validate-docs : 저장된 문서(원본 PDF + msds_pages)를 업로드 검증에 다시 통과시킨다
        Some("validate-docs") => {
            if let Err(e) = validate_docs(&pool).await {
                tracing::error!("{e}");
                std::process::exit(1);
            }
            return;
        }
        // cargo run -- reextract : data/uploads의 원본으로 모든 문서의 msds_pages를 다시 만든다
        Some("reextract") => {
            if let Err(e) = reextract(&pool).await {
                tracing::error!("{e}");
                std::process::exit(1);
            }
            return;
        }
        Some(other) => {
            tracing::error!("알 수 없는 명령: {other} (loop-test, agent-test, tool-test, validate-docs, reextract 또는 인자 없이 서버 실행)");
            std::process::exit(2);
        }
        None => {}
    }

    let port = std::env::var("PORT").unwrap_or_else(|_| "8090".into());
    let addr = format!("0.0.0.0:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("{addr} 바인드 실패: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!("msds-agent 시작: http://{addr} (model {model})");
    actors::log_models();

    let app = api::router(api::AppState { pool });
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("서버 오류: {e}");
    }
}

async fn loop_test(pool: sqlx::PgPool, doc_id: &str) -> Result<(), String> {
    let client = tool_loop::claude_client_from_env()?;
    let mut registry = pekko_agent_tools::ToolRegistry::new();
    registry.register(Arc::new(tools::DocInfoTool::new(pool)));

    let system_prompt = "너는 MSDS 문서 정보를 조회하는 도우미다. 필요한 정보는 도구로 확인하고 한국어로 간결하게 답한다.";
    let question = format!("doc_id {doc_id} 문서의 파일명, 페이지 수, 언어를 알려줘");

    let result = tool_loop::run_tool_loop(&client, &mut registry, system_prompt, question, 1024)
        .await
        .map_err(|e| e.to_string())?;

    println!("--- 최종 답변 ---\n{}", result.final_text);
    println!(
        "--- 통계 ---\nrounds={} tools_used={:?} input_tokens={} output_tokens={}",
        result.rounds, result.tools_used, result.input_tokens, result.output_tokens
    );
    Ok(())
}

async fn tool_test(pool: sqlx::PgPool, name: &str, raw: &str) -> Result<(), String> {
    // PowerShell은 네이티브 프로그램에 넘기는 인자의 큰따옴표를 지우는 경우가 있어 @파일 입력도 받는다
    let json_text = match raw.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("{path} 읽기 실패: {e}"))?,
        None => raw.to_string(),
    };
    let input: serde_json::Value =
        serde_json::from_str(json_text.trim_start_matches('\u{feff}')).map_err(|e| format!("JSON 입력 파싱 실패: {e}\n입력: {json_text}"))?;

    let mut registry = pekko_agent_tools::ToolRegistry::new();
    for tool in tools::all_tools(&pool) {
        registry.register(tool);
    }
    if !registry.has_tool(name) {
        let mut names: Vec<String> = registry.list_tools().into_iter().map(|d| d.name).collect();
        names.sort();
        return Err(format!("툴 {name}이 없습니다. 사용 가능: {}", names.join(", ")));
    }

    let ctx = pekko_agent_core::ToolContext {
        tenant_id: "local".into(),
        user_id: "msds-agent".into(),
        session_id: uuid::Uuid::new_v4(),
        credentials: Default::default(),
        timeout: std::time::Duration::from_secs(60),
    };
    match registry.execute(name, input, &ctx).await {
        Ok(out) => {
            println!("is_error: {}", out.is_error);
            println!("{}", serde_json::to_string_pretty(&out.content).unwrap_or_default());
            Ok(())
        }
        Err(e) => Err(format!("툴 실행 Err: {e}")),
    }
}

/// 재업로드 없이 DB의 문서를 validate_upload에 통과시킨다. 상태는 바꾸지 않는다.
async fn validate_docs(pool: &sqlx::PgPool) -> Result<(), String> {
    let docs: Vec<(uuid::Uuid, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, file_name, file_path FROM msds_documents ORDER BY file_name")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("문서 조회 실패: {e}"))?;

    println!("| 파일명 | 크기(MB) | 공백 제외 글자 | 서로 다른 섹션 번호 | 결과 |");
    println!("|---|---|---|---|---|");
    let mut failed = 0;
    for (id, file_name, file_path) in docs {
        let name = file_name.unwrap_or_else(|| id.to_string());
        let path = file_path.unwrap_or_else(|| format!("data/uploads/{id}.pdf"));
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                failed += 1;
                println!("| {name} | - | - | - | 원본 읽기 실패: {e} |");
                continue;
            }
        };
        let pages: Vec<(Option<String>,)> =
            sqlx::query_as("SELECT text FROM msds_pages WHERE doc_id = $1 ORDER BY page_no")
                .bind(id)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("페이지 조회 실패: {e}"))?;
        let pages: Vec<String> = pages.into_iter().map(|(t,)| t.unwrap_or_default()).collect();
        let distinct: std::collections::BTreeSet<u32> =
            pages.iter().flat_map(|p| p.lines().filter_map(tools::headings::heading_number)).collect();
        let result = match validate::validate_upload(&bytes, &pages) {
            Ok(()) => "통과".to_string(),
            Err(e) => {
                failed += 1;
                format!("거부: {}", e.message())
            }
        };
        println!(
            "| {name} | {:.2} | {} | {} | {result} |",
            bytes.len() as f64 / (1024.0 * 1024.0),
            pdf::non_space_chars(&pages),
            distinct.len(),
        );
    }
    println!("\n거부 {failed}건");
    Ok(())
}

/// 모든 문서를 원본 PDF로 다시 추출하고, 기존 대비 비교 표를 출력한다.
async fn reextract(pool: &sqlx::PgPool) -> Result<(), String> {
    use tools::headings::{backsteps, heading_number};

    fn heading_numbers(pages: &[String]) -> Vec<u32> {
        pages.iter().flat_map(|p| p.lines().filter_map(heading_number)).collect()
    }
    fn seq(nums: &[u32]) -> String {
        nums.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
    }

    let docs: Vec<(uuid::Uuid, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, file_name, file_path FROM msds_documents ORDER BY file_name")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("문서 조회 실패: {e}"))?;

    println!("| 파일명 | 페이지 | 공백 제외 글자 (기존 → 새, 차이) | 후보 수 (기존 → 새) | 뒤로 가기 (기존 → 새) | 새 후보 번호 순서 | 비고 |");
    println!("|---|---|---|---|---|---|---|");
    for (id, file_name, file_path) in docs {
        let name = file_name.unwrap_or_else(|| id.to_string());
        let path = file_path.unwrap_or_else(|| format!("data/uploads/{id}.pdf"));

        let old: Vec<(Option<String>,)> =
            sqlx::query_as("SELECT text FROM msds_pages WHERE doc_id = $1 ORDER BY page_no")
                .bind(id)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("페이지 조회 실패: {e}"))?;
        let old: Vec<String> = old.into_iter().map(|(t,)| t.unwrap_or_default()).collect();

        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                println!("| {name} | - | - | - | - | - | 원본 읽기 실패: {e} |");
                continue;
            }
        };
        let extracted = match pdf::extract_pages(bytes).await {
            Ok(x) => x,
            Err(e) => {
                println!("| {name} | - | - | - | - | - | 추출 실패(기존 페이지 유지): {e} |");
                continue;
            }
        };
        let saved = db::save_pages(pool, id, &extracted.pages).await.map_err(|e| format!("{name} 저장 실패: {e}"))?;

        let (old_chars, new_chars) = (pdf::non_space_chars(&old), saved.chars);
        let diff = if old_chars == 0 { 0.0 } else { (new_chars as f64 - old_chars as f64) / old_chars as f64 * 100.0 };
        let (old_h, new_h) = (heading_numbers(&old), heading_numbers(&extracted.pages));

        let mut notes = Vec::new();
        if extracted.fallback {
            notes.push("기본 추출로 대신함".to_string());
        }
        if saved.no_text {
            notes.push(format!("failed: {}", pdf::NO_TEXT_ERROR));
        }
        let (sections,): (i64,) = sqlx::query_as("SELECT count(*) FROM msds_sections WHERE doc_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("섹션 조회 실패: {e}"))?;
        if sections > 0 {
            notes.push(format!("저장된 섹션 {sections}개의 줄 번호가 바뀌었을 수 있음"));
        }

        println!(
            "| {name} | {} → {} | {old_chars} → {new_chars} ({diff:+.2}%) | {} → {} | {} → {} | {} | {} |",
            old.len(),
            saved.page_count,
            old_h.len(),
            new_h.len(),
            backsteps(&old_h).len(),
            backsteps(&new_h).len(),
            seq(&new_h),
            notes.join(", "),
        );
    }
    Ok(())
}
