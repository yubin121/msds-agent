#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();
    let port = std::env::var("PORT").unwrap_or_else(|_| "8090".into());
    tracing::info!("msds-agent 뼈대 실행 (port {port})");
}
