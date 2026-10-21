use sqlx::postgres::{PgPool, PgPoolOptions};

/// .env의 DATABASE_URL로 커넥션 풀을 만든다.
pub async fn connect() -> Result<PgPool, String> {
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL이 설정되지 않았습니다".to_string())?;
    PgPoolOptions::new()
        .max_connections(10)
        .connect(&url)
        .await
        .map_err(|e| format!("DB 연결 실패: {e}"))
}

/// 테이블 생성. 외래키·프로시저·트리거는 쓰지 않는다.
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS msds_documents (
        id UUID PRIMARY KEY,
        file_name TEXT,
        file_path TEXT,
        language TEXT,
        page_count INT,
        status TEXT NOT NULL DEFAULT 'uploaded',
        error TEXT,
        created_at TIMESTAMPTZ DEFAULT now(),
        updated_at TIMESTAMPTZ DEFAULT now()
    )",
    "CREATE TABLE IF NOT EXISTS msds_pages (
        doc_id UUID,
        page_no INT,
        text TEXT,
        PRIMARY KEY (doc_id, page_no)
    )",
    "CREATE TABLE IF NOT EXISTS msds_sections (
        doc_id UUID,
        section_no INT,
        title TEXT,
        text TEXT,
        PRIMARY KEY (doc_id, section_no)
    )",
    "CREATE TABLE IF NOT EXISTS msds_components (
        id BIGSERIAL PRIMARY KEY,
        doc_id UUID,
        product_name TEXT,
        component TEXT,
        cas_no TEXT,
        percent TEXT,
        cas_valid BOOLEAN
    )",
    "CREATE TABLE IF NOT EXISTS msds_hazards (
        id BIGSERIAL PRIMARY KEY,
        doc_id UUID,
        ghs_code TEXT,
        symbol_name TEXT,
        evidence TEXT
    )",
    "CREATE TABLE IF NOT EXISTS msds_translations (
        doc_id UUID,
        section_no INT,
        text_ko TEXT,
        PRIMARY KEY (doc_id, section_no)
    )",
];

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::Error> {
    for sql in MIGRATIONS {
        sqlx::query(sql).execute(pool).await?;
    }
    Ok(())
}
