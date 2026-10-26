//! 기능별 에이전트 액터
//!
//! 에이전트 액터는 Run을 받으면 tokio::spawn으로 자기 툴 루프를 띄우고 바로 돌아온다(액터를 막지 않음).
//! 작업이 끝나면 msds_agent_runs에 기록하고 reply_to에 AgentDone을 tell한다.
//! 메시지에는 doc_id만 담고, 결과는 각 에이전트가 저장 툴로 DB에 직접 저장한다.

pub mod components;
pub mod extract;

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
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Extract => "extract",
            Agent::Components => "components",
        }
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
    client: Arc<ClaudeClient>,
    pool: PgPool,
    _reply: PhantomData<fn() -> R>,
}

impl<R> AgentActor<R> {
    pub fn new(agent: Agent, client: Arc<ClaudeClient>, pool: PgPool) -> Self {
        Self { agent, client, pool, _reply: PhantomData }
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
                info!(agent = agent.as_str(), %doc_id, "에이전트 작업 시작");
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
    };
    result.unwrap_or_else(JobOutcome::failed)
}
