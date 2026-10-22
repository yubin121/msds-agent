mod api;
mod db;
mod pdf;
mod tool_loop;
mod tools;

use std::sync::Arc;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let model = std::env::var("CLAUDE_MODEL").unwrap_or_default();
    if model.trim().is_empty() {
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

    // cargo run -- loop-test <doc_id> : 서버 없이 툴 루프만 확인하고 종료
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("loop-test") {
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

    if let Err(e) = db::migrate(&pool).await {
        tracing::error!("마이그레이션 실패: {e}");
        std::process::exit(1);
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
