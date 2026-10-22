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
    // 번역은 (doc_id, section_no, chunk_no) 단위. 기존 PK(doc_id, section_no)를 유니크 인덱스로 바꾼다
    "ALTER TABLE msds_translations DROP CONSTRAINT IF EXISTS msds_translations_pkey",
    "CREATE UNIQUE INDEX IF NOT EXISTS msds_translations_chunk_uq ON msds_translations (doc_id, section_no, chunk_no)",
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
