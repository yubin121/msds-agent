mod api;
mod db;
mod pdf;
mod tool_loop;
mod tools;

use std::sync::Arc;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    // 로그는 stderr로 보내 tool-test의 JSON 출력(stdout)과 섞이지 않게 한다
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);

    // tool-test는 LLM을 쓰지 않으므로 CLAUDE_MODEL 검사를 건너뛴다
    let model = std::env::var("CLAUDE_MODEL").unwrap_or_default();
    if command != Some("tool-test") && model.trim().is_empty() {
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
        Some(other) => {
            tracing::error!("알 수 없는 명령: {other} (loop-test, tool-test 또는 인자 없이 서버 실행)");
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
