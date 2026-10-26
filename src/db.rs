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
    // Step 6: 툴이 쓰는 컬럼 추가 (기존 DB와 새 DB 모두 같은 경로로 맞춘다)
    "ALTER TABLE msds_sections ADD COLUMN IF NOT EXISTS page_no INT",
    "ALTER TABLE msds_sections ADD COLUMN IF NOT EXISTS line_no INT",
    "ALTER TABLE msds_hazards ADD COLUMN IF NOT EXISTS h_code TEXT",
    "ALTER TABLE msds_hazards ADD COLUMN IF NOT EXISTS signal_word TEXT",
    "ALTER TABLE msds_translations ADD COLUMN IF NOT EXISTS chunk_no INT NOT NULL DEFAULT 0",
    "ALTER TABLE msds_translations ADD COLUMN IF NOT EXISTS title_ko TEXT",
    // 번역은 (doc_id, product_no, section_no, chunk_no) 단위. 기존 PK(doc_id, section_no)는 지운다
    "ALTER TABLE msds_translations DROP CONSTRAINT IF EXISTS msds_translations_pkey",
    // Step 6 보완: 한 PDF에 제품이 여러 개 (product_no 1부터, 단일 제품 문서는 1)
    "ALTER TABLE msds_sections ADD COLUMN IF NOT EXISTS product_no INT NOT NULL DEFAULT 1",
    "ALTER TABLE msds_sections ADD COLUMN IF NOT EXISTS product_name TEXT",
    "ALTER TABLE msds_sections ADD COLUMN IF NOT EXISTS original_no INT",
    "ALTER TABLE msds_sections DROP CONSTRAINT IF EXISTS msds_sections_pkey",
    "CREATE UNIQUE INDEX IF NOT EXISTS msds_sections_product_uq ON msds_sections (doc_id, product_no, section_no)",
    "ALTER TABLE msds_components ADD COLUMN IF NOT EXISTS product_no INT NOT NULL DEFAULT 1",
    "ALTER TABLE msds_hazards ADD COLUMN IF NOT EXISTS product_no INT NOT NULL DEFAULT 1",
    "ALTER TABLE msds_hazards ADD COLUMN IF NOT EXISTS note TEXT",
    // H코드 출처: document(문서에 적힌 코드) / classification(분류에서 변환)
    "ALTER TABLE msds_hazards ADD COLUMN IF NOT EXISTS source TEXT",
    "ALTER TABLE msds_translations ADD COLUMN IF NOT EXISTS product_no INT NOT NULL DEFAULT 1",
    "DROP INDEX IF EXISTS msds_translations_chunk_uq",
    "CREATE UNIQUE INDEX IF NOT EXISTS msds_translations_product_chunk_uq \
     ON msds_translations (doc_id, product_no, section_no, chunk_no)",
    // Step 7: 에이전트 실행 기록 (실행마다 한 행, 토큰·비용 집계용)
    "CREATE TABLE IF NOT EXISTS msds_agent_runs (
        id BIGSERIAL PRIMARY KEY,
        doc_id UUID,
        agent TEXT NOT NULL,
        ok BOOLEAN NOT NULL,
        error TEXT,
        rounds INT,
        input_tokens INT,
        output_tokens INT,
        started_at TIMESTAMPTZ,
        finished_at TIMESTAMPTZ
    )",
    "CREATE INDEX IF NOT EXISTS msds_agent_runs_doc_idx ON msds_agent_runs (doc_id)",
    // 에이전트별 모델 (CLAUDE_MODEL_*)
    "ALTER TABLE msds_agent_runs ADD COLUMN IF NOT EXISTS model TEXT",
];

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::Error> {
    for sql in MIGRATIONS {
        sqlx::query(sql).execute(pool).await?;
    }
    Ok(())
}

pub struct SavedPages {
    pub page_count: i32,
    pub language: &'static str,
    /// 공백을 뺀 글자 수 합
    pub chars: usize,
    /// 글자 수가 기준 미만이라 failed로 저장했는지
    pub no_text: bool,
}

/// 문서의 msds_pages를 통째로 바꾸고 page_count·language를 갱신한다 (업로드·재추출 공용).
/// 공백을 뺀 글자 수 합이 pdf::MIN_TEXT_CHARS 미만이면 status='failed'와 오류를 저장한다.
/// 그 외에는 이전 실패 상태만 'uploaded'로 되돌린다.
pub async fn save_pages(pool: &PgPool, doc_id: uuid::Uuid, pages: &[String]) -> Result<SavedPages, sqlx::Error> {
    let language = crate::pdf::detect_language(&pages.concat());
    let page_count = pages.len() as i32;
    let chars = crate::pdf::non_space_chars(pages);
    let no_text = chars < crate::pdf::MIN_TEXT_CHARS;

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM msds_pages WHERE doc_id = $1").bind(doc_id).execute(&mut *tx).await?;
    for (i, text) in pages.iter().enumerate() {
        // PostgreSQL TEXT는 NUL 문자를 저장할 수 없다
        sqlx::query("INSERT INTO msds_pages (doc_id, page_no, text) VALUES ($1, $2, $3)")
            .bind(doc_id)
            .bind(i as i32 + 1)
            .bind(text.replace('\0', ""))
            .execute(&mut *tx)
            .await?;
    }
    let status_sql = if no_text {
        "UPDATE msds_documents SET language = $2, page_count = $3, status = 'failed', error = $4, \
         updated_at = now() WHERE id = $1"
    } else {
        "UPDATE msds_documents SET language = $2, page_count = $3, \
         status = CASE WHEN status = 'failed' THEN 'uploaded' ELSE status END, \
         error = CASE WHEN status = 'failed' THEN NULL ELSE error END, \
         updated_at = now() WHERE id = $1"
    };
    let mut q = sqlx::query(status_sql).bind(doc_id).bind(language).bind(page_count);
    if no_text {
        q = q.bind(crate::pdf::NO_TEXT_ERROR);
    }
    q.execute(&mut *tx).await?;
    tx.commit().await?;

    Ok(SavedPages { page_count, language, chars, no_text })
}

pub struct AgentRun<'a> {
    pub doc_id: uuid::Uuid,
    pub agent: &'a str,
    pub model: &'a str,
    pub ok: bool,
    pub error: Option<&'a str>,
    pub rounds: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

/// 에이전트 실행 한 번을 msds_agent_runs에 기록한다.
pub async fn save_agent_run(pool: &PgPool, run: &AgentRun<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO msds_agent_runs \
         (doc_id, agent, model, ok, error, rounds, input_tokens, output_tokens, started_at, finished_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(run.doc_id)
    .bind(run.agent)
    .bind(run.model)
    .bind(run.ok)
    .bind(run.error)
    .bind(run.rounds as i32)
    .bind(run.input_tokens as i32)
    .bind(run.output_tokens as i32)
    .bind(run.started_at)
    .bind(run.finished_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// 문서를 status 'failed'로 바꾸고 이유를 남긴다.
pub async fn mark_failed(pool: &PgPool, doc_id: uuid::Uuid, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE msds_documents SET status = 'failed', error = $2, updated_at = now() WHERE id = $1")
        .bind(doc_id)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}
