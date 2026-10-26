//! 기능별 에이전트 액터
//!
//! 에이전트 액터는 Run을 받으면 tokio::spawn으로 자기 툴 루프를 띄우고 바로 돌아온다(액터를 막지 않음).
//! 작업이 끝나면 msds_agent_runs에 기록하고 reply_to에 AgentDone을 tell한다.
//! 메시지에는 doc_id만 담고, 결과는 각 에이전트가 저장 툴로 DB에 직접 저장한다.

pub mod components;
pub mod extract;
pub mod hazards;
pub mod translate;

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use pekko_actor::{Actor, ActorContext, ActorRef};
use pekko_agent_llm::ClaudeClient;
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::tool_loop::LoopResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Extract,
    Components,
    Hazards,
    Translate,
}

impl Agent {
    pub const ALL: [Agent; 4] = [Agent::Extract, Agent::Components, Agent::Hazards, Agent::Translate];

    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Extract => "extract",
            Agent::Components => "components",
            Agent::Hazards => "hazards",
            Agent::Translate => "translate",
        }
    }

    /// 에이전트별 모델 환경변수 (비어 있으면 CLAUDE_MODEL)
    pub fn model_env(self) -> &'static str {
        match self {
            Agent::Extract => "CLAUDE_MODEL_EXTRACT",
            Agent::Components => "CLAUDE_MODEL_COMPONENTS",
            Agent::Hazards => "CLAUDE_MODEL_HAZARDS",
            Agent::Translate => "CLAUDE_MODEL_TRANSLATE",
        }
    }
}

/// 시작 로그: 에이전트별 모델 이름 (키는 출력하지 않는다)
pub fn log_models() {
    for agent in Agent::ALL {
        info!(agent = agent.as_str(), model = %crate::tool_loop::model_from_env(agent.model_env()), "에이전트 모델");
    }
}

pub const NO_SECTIONS_ERROR: &str = "추출 결과 없음";

/// msds_sections에 저장된 제품 목록 (product_no, product_name)
pub async fn load_products(pool: &PgPool, doc_id: Uuid) -> Result<Vec<(i32, Option<String>)>, String> {
    sqlx::query_as(
        "SELECT product_no, max(product_name) FROM msds_sections WHERE doc_id = $1          GROUP BY product_no ORDER BY product_no",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("섹션 조회 실패: {e}"))
}

/// 사용자 메시지에 넣을 제품 목록
pub fn product_list(products: &[(i32, Option<String>)]) -> String {
    products
        .iter()
        .map(|(no, name)| format!("- product_no {no}: {}", name.as_deref().unwrap_or("(제품명 없음)")))
        .collect::<Vec<_>>()
        .join("
")
}

/// 결과 테이블(msds_components, msds_hazards)의 제품별 행 수
pub async fn rows_per_product(pool: &PgPool, table: &str, doc_id: Uuid) -> Result<BTreeMap<i32, i64>, String> {
    let rows: Vec<(i32, i64)> =
        sqlx::query_as(&format!("SELECT product_no, count(*) FROM {table} WHERE doc_id = $1 GROUP BY product_no"))
            .bind(doc_id)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("{table} 조회 실패: {e}"))?;
    Ok(rows.into_iter().collect())
}

/// 성공 판정 공용: 제품마다 행이 1개 이상이어야 한다. what은 오류 메시지의 주어("성분이")
pub fn check_rows_per_product(products: &[i32], counts: &BTreeMap<i32, i64>, what: &str) -> Result<(), String> {
    let missing: Vec<String> = products
        .iter()
        .filter(|p| counts.get(p).copied().unwrap_or(0) == 0)
        .map(i32::to_string)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{what} 저장되지 않은 제품: {}", missing.join(", ")))
    }
}

/// 에이전트 액터가 받는 메시지. R은 AgentDone을 받을 액터다.
pub enum AgentMsg<R: Actor> {
    Run { doc_id: Uuid, reply_to: ActorRef<R> },
}

/// 에이전트 작업 완료 보고
#[derive(Debug, Clone)]
pub struct AgentDone {
    pub doc_id: Uuid,
    pub agent: Agent,
    pub ok: bool,
    pub error: Option<String>,
    pub rounds: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub model: String,
}

/// 작업 한 번의 결과 (AgentDone과 실행 기록의 재료)
pub struct JobOutcome {
    pub ok: bool,
    pub error: Option<String>,
    pub rounds: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl JobOutcome {
    /// Claude를 부르기 전에 실패
    pub fn failed(error: impl Into<String>) -> Self {
        Self { ok: false, error: Some(error.into()), rounds: 0, input_tokens: 0, output_tokens: 0 }
    }

    /// Claude를 부르지 않고 성공으로 끝냄 (예: 한글 문서 번역 생략). note는 실행 기록 error에 남긴다
    pub fn skipped(note: impl Into<String>) -> Self {
        Self { ok: true, error: Some(note.into()), rounds: 0, input_tokens: 0, output_tokens: 0 }
    }

    /// 툴 루프를 마친 뒤 코드 판정 결과를 붙인다
    pub fn after_loop(result: &LoopResult, check: Result<(), String>) -> Self {
        Self {
            ok: check.is_ok(),
            error: check.err(),
            rounds: result.rounds,
            input_tokens: result.input_tokens,
            output_tokens: result.output_tokens,
        }
    }
}

pub struct AgentActor<R> {
    agent: Agent,
    model: Arc<str>,
    client: Arc<ClaudeClient>,
    pool: PgPool,
    _reply: PhantomData<fn() -> R>,
}

impl<R> AgentActor<R> {
    /// 에이전트 자기 모델(CLAUDE_MODEL_*, 비어 있으면 CLAUDE_MODEL)로 클라이언트를 만든다
    pub fn from_env(agent: Agent, pool: PgPool) -> Result<Self, String> {
        let model = crate::tool_loop::model_from_env(agent.model_env());
        let client = Arc::new(crate::tool_loop::claude_client(&model)?);
        Ok(Self { agent, model: model.into(), client, pool, _reply: PhantomData })
    }
}

#[async_trait]
impl<R> Actor for AgentActor<R>
where
    R: Actor,
    R::Message: From<AgentDone>,
{
    type Message = AgentMsg<R>;

    async fn receive(&mut self, msg: Self::Message, _ctx: &mut ActorContext<Self>) {
        match msg {
            AgentMsg::Run { doc_id, reply_to } => {
                let (agent, client, pool) = (self.agent, self.client.clone(), self.pool.clone());
                let model = self.model.clone();
                info!(agent = agent.as_str(), model = &*model, %doc_id, "에이전트 작업 시작");
                tokio::spawn(async move {
                    let started_at = chrono::Utc::now();
                    // 작업이 panic해도 기록과 보고는 남긴다
                    let outcome = match tokio::spawn(run_job(agent, client, pool.clone(), doc_id)).await {
                        Ok(o) => o,
                        Err(e) => JobOutcome::failed(format!("에이전트 작업이 비정상 종료되었습니다: {e}")),
                    };
                    let finished_at = chrono::Utc::now();

                    let run = crate::db::AgentRun {
                        doc_id,
                        agent: agent.as_str(),
                        model: &model,
                        ok: outcome.ok,
                        error: outcome.error.as_deref(),
                        rounds: outcome.rounds,
                        input_tokens: outcome.input_tokens,
                        output_tokens: outcome.output_tokens,
                        started_at,
                        finished_at,
                    };
                    if let Err(e) = crate::db::save_agent_run(&pool, &run).await {
                        warn!(agent = agent.as_str(), %doc_id, "실행 기록 저장 실패: {e}");
                    }
                    info!(
                        agent = agent.as_str(),
                        %doc_id,
                        ok = outcome.ok,
                        error = outcome.error.as_deref().unwrap_or(""),
                        rounds = outcome.rounds,
                        "에이전트 작업 끝"
                    );

                    let done = AgentDone {
                        doc_id,
                        agent,
                        ok: outcome.ok,
                        error: outcome.error,
                        rounds: outcome.rounds,
                        input_tokens: outcome.input_tokens,
                        output_tokens: outcome.output_tokens,
                        model: model.to_string(),
                    };
                    if reply_to.tell(done.into()).is_err() {
                        warn!(agent = agent.as_str(), %doc_id, "AgentDone을 받을 액터가 멈춰 있습니다");
                    }
                });
            }
        }
    }
}

async fn run_job(agent: Agent, client: Arc<ClaudeClient>, pool: PgPool, doc_id: Uuid) -> JobOutcome {
    let result = match agent {
        Agent::Extract => extract::run(&client, &pool, doc_id).await,
        Agent::Components => components::run(&client, &pool, doc_id).await,
        Agent::Hazards => hazards::run(&client, &pool, doc_id).await,
        Agent::Translate => translate::run(&client, &pool, doc_id).await,
    };
    result.unwrap_or_else(JobOutcome::failed)
}
