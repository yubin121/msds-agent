mod api;
mod db;
mod pdf;

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
