//! 툴 공통 처리
//!
//! ToolRegistry::execute는 툴의 Err를 그대로 돌려주고, run_tool_loop가 Err를
//! `{"error": ...}` + is_error로 바꾼다. 모델이 고칠 수 있는 입력 오류는
//! Ok(ToolOutput::error("오류: 이유"))로 돌려주고, DB 장애만 Err로 돌려준다.

use pekko_agent_core::{ToolError, ToolOutput};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub enum Fail {
    /// 모델이 읽고 고칠 수 있는 오류
    Input(String),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        Fail::Db(e)
    }
}

pub fn input_err<T>(msg: impl Into<String>) -> Result<T, Fail> {
    Err(Fail::Input(msg.into()))
}

pub fn finish(name: &str, result: Result<Value, Fail>) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(v) => Ok(ToolOutput::success(v)),
        Err(Fail::Input(msg)) => Ok(ToolOutput::error(format!("오류: {msg}"))),
        Err(Fail::Db(e)) => {
            tracing::error!(tool = name, "DB 오류: {e}");
            Err(ToolError::ExecutionFailed(format!("DB 오류: {e}")))
        }
    }
}

pub fn parse_input<T: DeserializeOwned>(input: Value) -> Result<T, Fail> {
    serde_json::from_value(input).map_err(|e| Fail::Input(format!("입력 형식이 잘못되었습니다: {e}")))
}

/// doc_id를 파싱하고 msds_documents에 있는지 확인한다.
pub async fn ensure_doc(pool: &PgPool, raw: &str) -> Result<Uuid, Fail> {
    let Ok(id) = Uuid::parse_str(raw.trim()) else {
        return input_err(format!("doc_id가 UUID 형식이 아닙니다: {raw}"));
    };
    let found: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM msds_documents WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    match found {
        Some(_) => Ok(id),
        None => input_err(format!("doc_id {id} 문서가 없습니다")),
    }
}

pub fn check_section_no(n: i32) -> Result<(), Fail> {
    if (1..=16).contains(&n) {
        Ok(())
    } else {
        input_err(format!("section_no는 1~16이어야 합니다: {n}"))
    }
}

/// 문자열·숫자·null을 받아 Option<String>으로 (percent 등에 숫자가 와도 받아 준다)
pub fn de_opt_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    use serde::Deserialize;
    Ok(match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s),
        Some(other) => Some(other.to_string()),
    })
}
