//! 총괄 액터: LLM을 부르지 않고 에이전트 실행 순서, 시간 제한, 동시 분석 수, 최종 상태 판정만 맡는다.
//!
//! 흐름: Analyze → (대기열) → 추출 Run → 추출 ok면 성분·그림문자·번역 Run → AgentDone을 모아 판정.
//! 순서·판정 규칙은 순수 상태 기계 Pipeline에 두고(Action만 돌려줌), 액터는 Action을 DB와 tell로 실행한다.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::Duration;

use async_trait::async_trait;
use pekko_actor::{Actor, ActorContext, ActorRef, ActorSystem};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{info, warn};
use uuid::Uuid;

use super::{Agent, AgentActor, AgentDone, AgentMsg, TIMEOUT_ERROR};

const DEFAULT_TIMEOUT_SECS: u64 = 600;
const TIMEOUT_SECS_RANGE: std::ops::RangeInclusive<u64> = 10..=3600;
/// 기존 클라이언트의 "한 번에 최대 3건"
const DEFAULT_MAX_CONCURRENT_DOCS: usize = 3;
const MAX_CONCURRENT_DOCS_RANGE: std::ops::RangeInclusive<usize> = 1..=10;
/// error 요약에서 에이전트 하나의 이유 최대 글자 수
const MAX_REASON_CHARS: usize = 200;
const FOLLOWERS: [Agent; 3] = [Agent::Components, Agent::Hazards, Agent::Translate];

/// MSDS_DOC_TIMEOUT_SECS: 없거나 숫자가 아니면 600, 범위 밖이면 10~3600으로 맞춘다
pub fn parse_timeout_secs(raw: Option<&str>) -> u64 {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => DEFAULT_TIMEOUT_SECS,
        Some(s) => s
            .parse::<u64>()
            .map(|n| n.clamp(*TIMEOUT_SECS_RANGE.start(), *TIMEOUT_SECS_RANGE.end()))
            .unwrap_or(DEFAULT_TIMEOUT_SECS),
    }
}

/// MSDS_MAX_CONCURRENT_DOCS: 없거나 숫자가 아니면 3, 범위 밖이면 1~10으로 맞춘다
pub fn parse_max_concurrent(raw: Option<&str>) -> usize {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => DEFAULT_MAX_CONCURRENT_DOCS,
        Some(s) => s
            .parse::<usize>()
            .map(|n| n.clamp(*MAX_CONCURRENT_DOCS_RANGE.start(), *MAX_CONCURRENT_DOCS_RANGE.end()))
            .unwrap_or(DEFAULT_MAX_CONCURRENT_DOCS),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocStatus {
    Done,
    Partial,
    Failed,
}

impl DocStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DocStatus::Done => "done",
            DocStatus::Partial => "partial",
            DocStatus::Failed => "failed",
        }
    }
}

/// 판정 재료: 에이전트 하나의 결과
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentResult {
    Ok,
    Failed(String),
    /// 제한 시간 안에 AgentDone이 오지 않음
    Unfinished,
}

impl AgentResult {
    fn from_done(done: Option<&AgentDone>) -> Self {
        match done {
            None => AgentResult::Unfinished,
            Some(d) if d.ok => AgentResult::Ok,
            Some(d) => AgentResult::Failed(d.error.clone().unwrap_or_default()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub status: DocStatus,
    /// 실패한 에이전트마다 "번역: 시간 초과"처럼 한 줄
    pub error: Option<String>,
}

/// 최종 상태 판정. followers는 추출이 ok일 때 실행한 성분·그림문자·번역의 결과다.
/// 추출 실패 → failed / 나머지 모두 ok → done / 일부 ok → partial / 모두 실패 → failed.
/// 끝나지 않은 에이전트는 "시간 초과"로 실패 처리한다. 한글 문서 번역 생략은 에이전트가 ok로 보낸다.
pub fn decide(extract: &AgentResult, followers: &[(Agent, AgentResult)]) -> Verdict {
    if *extract != AgentResult::Ok {
        return Verdict { status: DocStatus::Failed, error: Some(error_line(Agent::Extract, extract)) };
    }
    let failed: Vec<String> =
        followers.iter().filter(|(_, r)| *r != AgentResult::Ok).map(|(a, r)| error_line(*a, r)).collect();
    let status = if failed.is_empty() {
        DocStatus::Done
    } else if failed.len() < followers.len() {
        DocStatus::Partial
    } else {
        DocStatus::Failed
    };
    Verdict { status, error: (!failed.is_empty()).then(|| failed.join("\n")) }
}

/// 에이전트 error는 경고를 "; "로 이어 붙여 길 수 있으므로 첫 항목만 남긴다
fn error_line(agent: Agent, result: &AgentResult) -> String {
    let reason = match result {
        AgentResult::Ok => "",
        AgentResult::Failed(e) => e.lines().next().unwrap_or("").split("; ").next().unwrap_or(""),
        AgentResult::Unfinished => TIMEOUT_ERROR,
    };
    let reason = if reason.trim().is_empty() { "이유 없음" } else { reason.trim() };
    let mut short: String = reason.chars().take(MAX_REASON_CHARS).collect();
    if reason.chars().count() > MAX_REASON_CHARS {
        short.push('…');
    }
    format!("{}: {short}", agent.label_ko())
}

/// Pipeline이 돌려주는 할 일. 액터가 순서대로 실행한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 대기열에 넣음 (status 'queued')
    Queued { doc_id: Uuid },
    /// 분석 시작: status 'analyzing', 이전 결과 삭제, 시간 제한 타이머
    Start { doc_id: Uuid, run_id: Uuid },
    Dispatch { doc_id: Uuid, run_id: Uuid, agents: Vec<Agent> },
    Finish { doc_id: Uuid, run_id: Uuid, verdict: Verdict, dispatched: Vec<Agent> },
}

struct DocRun {
    run_id: Uuid,
    extract: Option<AgentDone>,
    /// 추출이 ok일 때만 채운다(그때 Run을 보냄)
    followers: BTreeMap<Agent, Option<AgentDone>>,
}

impl DocRun {
    fn dispatched(&self) -> Vec<Agent> {
        std::iter::once(Agent::Extract).chain(self.followers.keys().copied()).collect()
    }

    fn unfinished(&self) -> BTreeSet<Agent> {
        let mut out: BTreeSet<Agent> = self.followers.iter().filter(|(_, d)| d.is_none()).map(|(a, _)| *a).collect();
        if self.extract.is_none() {
            out.insert(Agent::Extract);
        }
        out
    }
}

/// 문서별 분석 상태 기계 (DB·액터 없이 단위 테스트한다)
pub struct Pipeline {
    max_concurrent: usize,
    running: HashMap<Uuid, DocRun>,
    queue: VecDeque<Uuid>,
    /// 시간 초과로 판정했지만 아직 일하는 에이전트가 있는 실행 (doc_id → (run_id, 에이전트)).
    /// 이 에이전트들이 끝나기 전에는 같은 문서의 새 분석을 시작하지 않는다(이전 실행이 새 결과를 지우거나 덮지 않게)
    lingering: HashMap<Uuid, (Uuid, BTreeSet<Agent>)>,
}

impl Pipeline {
    pub fn new(max_concurrent: usize) -> Self {
        Self { max_concurrent: max_concurrent.max(1), running: HashMap::new(), queue: VecDeque::new(), lingering: HashMap::new() }
    }

    pub fn is_active(&self, doc_id: Uuid) -> bool {
        self.running.contains_key(&doc_id) || self.queue.contains(&doc_id)
    }

    pub fn on_analyze(&mut self, doc_id: Uuid) -> Vec<Action> {
        if self.is_active(doc_id) {
            info!(%doc_id, "이미 분석 중이거나 대기 중인 문서라 Analyze를 무시합니다");
            return Vec::new();
        }
        self.queue.push_back(doc_id);
        let mut actions = vec![Action::Queued { doc_id }];
        actions.extend(self.start_ready());
        actions
    }

    pub fn on_done(&mut self, done: AgentDone) -> Vec<Action> {
        let doc_id = done.doc_id;
        let current = self.running.get(&doc_id).map(|r| r.run_id);
        if done.run_id.is_none() || current != done.run_id {
            return self.on_stale_done(&done);
        }
        let run = self.running.get_mut(&doc_id).expect("실행 중인 문서");
        let run_id = run.run_id;
        match done.agent {
            Agent::Extract => {
                if run.extract.is_some() {
                    warn!(%doc_id, "추출 AgentDone이 두 번 왔습니다");
                    return Vec::new();
                }
                let ok = done.ok;
                run.extract = Some(done);
                if !ok {
                    return self.finish(doc_id);
                }
                run.followers = FOLLOWERS.iter().map(|a| (*a, None)).collect();
                vec![Action::Dispatch { doc_id, run_id, agents: FOLLOWERS.to_vec() }]
            }
            agent => {
                match run.followers.get_mut(&agent) {
                    Some(slot @ None) => *slot = Some(done),
                    _ => {
                        warn!(%doc_id, agent = agent.as_str(), "보내지 않았거나 이미 받은 에이전트의 AgentDone입니다");
                        return Vec::new();
                    }
                }
                if run.followers.values().all(Option::is_some) {
                    self.finish(doc_id)
                } else {
                    Vec::new()
                }
            }
        }
    }

    pub fn on_timeout(&mut self, doc_id: Uuid, run_id: Uuid) -> Vec<Action> {
        if self.running.get(&doc_id).map(|r| r.run_id) != Some(run_id) {
            return Vec::new(); // 이미 판정한 실행의 타이머
        }
        warn!(%doc_id, %run_id, "분석 제한 시간이 지났습니다");
        self.finish(doc_id)
    }

    /// 판정 뒤에 늦게 온 AgentDone: 상태는 바꾸지 않는다. 마지막 남은 에이전트면 대기 중인 새 분석을 시작한다
    fn on_stale_done(&mut self, done: &AgentDone) -> Vec<Action> {
        info!(
            doc_id = %done.doc_id,
            run_id = ?done.run_id,
            agent = done.agent.as_str(),
            ok = done.ok,
            "지금 실행 중이 아닌 실행의 AgentDone이라 상태를 바꾸지 않습니다"
        );
        let Some((run_id, agents)) = self.lingering.get_mut(&done.doc_id) else { return Vec::new() };
        if Some(*run_id) != done.run_id {
            return Vec::new();
        }
        agents.remove(&done.agent);
        if !agents.is_empty() {
            return Vec::new();
        }
        self.lingering.remove(&done.doc_id);
        self.start_ready()
    }

    fn finish(&mut self, doc_id: Uuid) -> Vec<Action> {
        let run = self.running.remove(&doc_id).expect("실행 중인 문서");
        let extract = AgentResult::from_done(run.extract.as_ref());
        let followers: Vec<(Agent, AgentResult)> =
            run.followers.iter().map(|(a, d)| (*a, AgentResult::from_done(d.as_ref()))).collect();
        let verdict = decide(&extract, &followers);

        let unfinished = run.unfinished();
        if !unfinished.is_empty() {
            self.lingering.insert(doc_id, (run.run_id, unfinished));
        }
        let mut actions =
            vec![Action::Finish { doc_id, run_id: run.run_id, verdict, dispatched: run.dispatched() }];
        actions.extend(self.start_ready());
        actions
    }

    /// 자리가 있으면 대기열 앞에서부터 시작한다(이전 실행의 에이전트가 남은 문서는 건너뜀)
    fn start_ready(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        while self.running.len() < self.max_concurrent {
            let Some(pos) = self.queue.iter().position(|d| !self.lingering.contains_key(d)) else { break };
            let doc_id = self.queue.remove(pos).expect("대기열 위치");
            let run_id = Uuid::new_v4();
            self.running.insert(doc_id, DocRun { run_id, extract: None, followers: BTreeMap::new() });
            actions.push(Action::Start { doc_id, run_id });
            actions.push(Action::Dispatch { doc_id, run_id, agents: vec![Agent::Extract] });
        }
        actions
    }
}

pub enum SupervisorMsg {
    Analyze { doc_id: Uuid },
    Done(AgentDone),
    Timeout { doc_id: Uuid, run_id: Uuid },
}

impl From<AgentDone> for SupervisorMsg {
    fn from(done: AgentDone) -> Self {
        SupervisorMsg::Done(done)
    }
}

/// 판정 결과 알림 (pipeline-test용)
#[derive(Debug, Clone)]
pub struct Finished {
    pub doc_id: Uuid,
    pub run_id: Uuid,
    pub status: DocStatus,
    pub error: Option<String>,
    pub dispatched: Vec<Agent>,
}

pub struct Supervisor {
    pool: PgPool,
    pipeline: Pipeline,
    timeout: Duration,
    agents: HashMap<Agent, ActorRef<AgentActor<Supervisor>>>,
    deadlines: HashMap<Uuid, Instant>,
    on_finish: Option<mpsc::UnboundedSender<Finished>>,
}

/// 에이전트 4개와 총괄 액터를 띄운다. 제한 시간과 동시 분석 수는 환경변수에서 읽는다.
pub async fn spawn(
    system: &ActorSystem,
    pool: PgPool,
    on_finish: Option<mpsc::UnboundedSender<Finished>>,
) -> Result<ActorRef<Supervisor>, String> {
    let mut agents = HashMap::new();
    for agent in Agent::ALL {
        let actor = AgentActor::<Supervisor>::from_env(agent, pool.clone())?;
        let r = system.spawn(actor, agent.as_str()).await.map_err(|e| e.to_string())?;
        agents.insert(agent, r);
    }
    let timeout_secs = parse_timeout_secs(std::env::var("MSDS_DOC_TIMEOUT_SECS").ok().as_deref());
    let max_concurrent = parse_max_concurrent(std::env::var("MSDS_MAX_CONCURRENT_DOCS").ok().as_deref());
    info!(timeout_secs, max_concurrent, "총괄 액터 시작");
    let supervisor = Supervisor {
        pool,
        pipeline: Pipeline::new(max_concurrent),
        timeout: Duration::from_secs(timeout_secs),
        agents,
        deadlines: HashMap::new(),
        on_finish,
    };
    system.spawn(supervisor, "supervisor").await.map_err(|e| e.to_string())
}

#[async_trait]
impl Actor for Supervisor {
    type Message = SupervisorMsg;

    async fn receive(&mut self, msg: SupervisorMsg, ctx: &mut ActorContext<Self>) {
        let actions = match msg {
            SupervisorMsg::Analyze { doc_id } => self.pipeline.on_analyze(doc_id),
            SupervisorMsg::Done(done) => self.pipeline.on_done(done),
            SupervisorMsg::Timeout { doc_id, run_id } => self.pipeline.on_timeout(doc_id, run_id),
        };
        for action in actions {
            self.execute(action, ctx).await;
        }
    }
}

impl Supervisor {
    async fn execute(&mut self, action: Action, ctx: &mut ActorContext<Self>) {
        match action {
            Action::Queued { doc_id } => {
                info!(%doc_id, "분석 대기열에 넣음");
                if let Err(e) = crate::db::set_status(&self.pool, doc_id, "queued", None).await {
                    warn!(%doc_id, "상태 저장 실패: {e}");
                }
            }
            Action::Start { doc_id, run_id } => {
                info!(%doc_id, %run_id, timeout_secs = self.timeout.as_secs(), "분석 시작");
                if let Err(e) = crate::db::set_status(&self.pool, doc_id, "analyzing", None).await {
                    warn!(%doc_id, "상태 저장 실패: {e}");
                }
                // 재분석에서 추출이 실패하거나 제품 수가 줄어도 예전 결과가 남지 않게 지운다
                if let Err(e) = crate::db::clear_results(&self.pool, doc_id).await {
                    warn!(%doc_id, "이전 결과 삭제 실패: {e}");
                }
                self.deadlines.insert(run_id, Instant::now() + self.timeout);
                let me = ctx.self_ref().clone();
                ctx.system().scheduler().schedule_once(self.timeout, move || async move {
                    let _ = me.tell(SupervisorMsg::Timeout { doc_id, run_id });
                });
            }
            Action::Dispatch { doc_id, run_id, agents } => {
                let deadline = self.deadlines.get(&run_id).copied();
                for agent in agents {
                    let msg = AgentMsg::Run { doc_id, run_id: Some(run_id), deadline, reply_to: ctx.self_ref().clone() };
                    if let Err(e) = self.agents[&agent].tell(msg) {
                        // 판정이 멈추지 않게 실패한 AgentDone을 스스로 보낸다
                        warn!(%doc_id, agent = agent.as_str(), "Run 전송 실패: {e}");
                        let done = AgentDone {
                            doc_id,
                            run_id: Some(run_id),
                            agent,
                            ok: false,
                            error: Some(format!("에이전트에 Run을 보내지 못했습니다: {e}")),
                            rounds: 0,
                            input_tokens: 0,
                            output_tokens: 0,
                            model: String::new(),
                        };
                        let _ = ctx.self_ref().tell(done.into());
                    }
                }
            }
            Action::Finish { doc_id, run_id, verdict, dispatched } => {
                self.deadlines.remove(&run_id);
                info!(
                    %doc_id,
                    %run_id,
                    status = verdict.status.as_str(),
                    error = verdict.error.as_deref().unwrap_or(""),
                    "분석 판정"
                );
                if let Err(e) =
                    crate::db::set_status(&self.pool, doc_id, verdict.status.as_str(), verdict.error.as_deref()).await
                {
                    warn!(%doc_id, "상태 저장 실패: {e}");
                }
                if let Some(tx) = &self.on_finish {
                    let _ = tx.send(Finished {
                        doc_id,
                        run_id,
                        status: verdict.status,
                        error: verdict.error,
                        dispatched,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(doc_id: Uuid, run_id: Uuid, agent: Agent, ok: bool, error: Option<&str>) -> AgentDone {
        AgentDone {
            doc_id,
            run_id: Some(run_id),
            agent,
            ok,
            error: error.map(str::to_string),
            rounds: 1,
            input_tokens: 0,
            output_tokens: 0,
            model: "m".into(),
        }
    }

    fn started_run(actions: &[Action], doc_id: Uuid) -> Uuid {
        actions
            .iter()
            .find_map(|a| match a {
                Action::Start { doc_id: d, run_id } if *d == doc_id => Some(*run_id),
                _ => None,
            })
            .expect("Start 없음")
    }

    fn finish(actions: &[Action]) -> Option<&Verdict> {
        actions.iter().find_map(|a| match a {
            Action::Finish { verdict, .. } => Some(verdict),
            _ => None,
        })
    }

    #[test]
    fn decide_rules() {
        let ok = |a| (a, AgentResult::Ok);
        let all_ok = [ok(Agent::Components), ok(Agent::Hazards), ok(Agent::Translate)];
        assert_eq!(decide(&AgentResult::Ok, &all_ok), Verdict { status: DocStatus::Done, error: None });

        let some = [
            ok(Agent::Components),
            (Agent::Hazards, AgentResult::Failed("유해성 정보가 저장되지 않은 제품: 2".into())),
            (Agent::Translate, AgentResult::Unfinished),
        ];
        assert_eq!(
            decide(&AgentResult::Ok, &some),
            Verdict {
                status: DocStatus::Partial,
                error: Some("그림문자: 유해성 정보가 저장되지 않은 제품: 2\n번역: 시간 초과".into())
            }
        );

        let none = [
            (Agent::Components, AgentResult::Failed("a".into())),
            (Agent::Hazards, AgentResult::Failed("".into())),
            (Agent::Translate, AgentResult::Failed("번역되지 않은 섹션: 제품 1 3번; 제품 1 3번: 시간 초과".into())),
        ];
        assert_eq!(
            decide(&AgentResult::Ok, &none),
            Verdict {
                status: DocStatus::Failed,
                error: Some("성분: a\n그림문자: 이유 없음\n번역: 번역되지 않은 섹션: 제품 1 3번".into())
            }
        );

        assert_eq!(
            decide(&AgentResult::Failed("섹션 순서가 뒤섞인 문서입니다".into()), &[]),
            Verdict { status: DocStatus::Failed, error: Some("추출: 섹션 순서가 뒤섞인 문서입니다".into()) }
        );
        assert_eq!(decide(&AgentResult::Unfinished, &[]).error.as_deref(), Some("추출: 시간 초과"));
    }

    #[test]
    fn long_reason_is_cut_on_char_boundary() {
        let long = "가".repeat(MAX_REASON_CHARS + 5);
        let line = error_line(Agent::Components, &AgentResult::Failed(long));
        assert_eq!(line.chars().count(), "성분: ".chars().count() + MAX_REASON_CHARS + 1);
        assert!(line.ends_with('…'));
    }

    #[test]
    fn normal_flow_done_with_korean_skip() {
        let mut p = Pipeline::new(3);
        let doc = Uuid::new_v4();
        let a = p.on_analyze(doc);
        let run = started_run(&a, doc);
        assert_eq!(a.last(), Some(&Action::Dispatch { doc_id: doc, run_id: run, agents: vec![Agent::Extract] }));

        let a = p.on_done(done(doc, run, Agent::Extract, true, None));
        assert_eq!(a, vec![Action::Dispatch { doc_id: doc, run_id: run, agents: FOLLOWERS.to_vec() }]);
        assert!(p.on_done(done(doc, run, Agent::Components, true, None)).is_empty());
        assert!(p.on_done(done(doc, run, Agent::Hazards, true, None)).is_empty());
        // 한글 문서 번역 생략: ok=true에 error 메모
        let a = p.on_done(done(doc, run, Agent::Translate, true, Some("한글 문서라 번역 생략")));
        assert_eq!(finish(&a), Some(&Verdict { status: DocStatus::Done, error: None }));
        assert!(!p.is_active(doc));
    }

    #[test]
    fn extract_failure_skips_followers() {
        let mut p = Pipeline::new(3);
        let doc = Uuid::new_v4();
        let run = started_run(&p.on_analyze(doc), doc);
        let a = p.on_done(done(doc, run, Agent::Extract, false, Some("MSDS 문서가 아닌 것으로 보입니다(섹션 제목 부족)")));
        assert_eq!(a.len(), 1);
        match &a[0] {
            Action::Finish { verdict, dispatched, .. } => {
                assert_eq!(verdict.status, DocStatus::Failed);
                assert_eq!(verdict.error.as_deref(), Some("추출: MSDS 문서가 아닌 것으로 보입니다(섹션 제목 부족)"));
                assert_eq!(dispatched, &vec![Agent::Extract]);
            }
            other => panic!("Finish가 아님: {other:?}"),
        }
    }

    #[test]
    fn duplicate_analyze_is_ignored() {
        let mut p = Pipeline::new(1);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        p.on_analyze(a);
        assert!(p.on_analyze(a).is_empty()); // 실행 중
        assert_eq!(p.on_analyze(b), vec![Action::Queued { doc_id: b }]);
        assert!(p.on_analyze(b).is_empty()); // 대기 중
    }

    #[test]
    fn queue_starts_next_when_one_finishes() {
        let mut p = Pipeline::new(2);
        let docs: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        let r0 = started_run(&p.on_analyze(docs[0]), docs[0]);
        p.on_analyze(docs[1]);
        assert_eq!(p.on_analyze(docs[2]), vec![Action::Queued { doc_id: docs[2] }]);
        assert_eq!(p.on_analyze(docs[3]), vec![Action::Queued { doc_id: docs[3] }]);

        // 0번이 끝나면 대기열 순서대로 2번이 시작된다
        let a = p.on_done(done(docs[0], r0, Agent::Extract, false, Some("x")));
        assert!(matches!(a[0], Action::Finish { .. }));
        started_run(&a, docs[2]);
        assert!(!a.iter().any(|x| matches!(x, Action::Start { doc_id, .. } if *doc_id == docs[3])));
        assert!(p.is_active(docs[3]));
    }

    #[test]
    fn stale_and_late_done_do_not_change_state() {
        let mut p = Pipeline::new(3);
        let doc = Uuid::new_v4();
        let run = started_run(&p.on_analyze(doc), doc);
        // 다른 실행 번호, 실행 번호 없음(agent-test)
        assert!(p.on_done(done(doc, Uuid::new_v4(), Agent::Extract, true, None)).is_empty());
        let mut no_run = done(doc, run, Agent::Extract, true, None);
        no_run.run_id = None;
        assert!(p.on_done(no_run).is_empty());

        let a = p.on_done(done(doc, run, Agent::Extract, false, Some("x")));
        assert!(finish(&a).is_some());
        // 판정 뒤에 온 AgentDone과 타이머
        assert!(p.on_done(done(doc, run, Agent::Extract, true, None)).is_empty());
        assert!(p.on_timeout(doc, run).is_empty());
    }

    #[test]
    fn timeout_marks_unfinished_and_blocks_rerun_until_late_done() {
        let mut p = Pipeline::new(3);
        let doc = Uuid::new_v4();
        let run = started_run(&p.on_analyze(doc), doc);
        p.on_done(done(doc, run, Agent::Extract, true, None));
        p.on_done(done(doc, run, Agent::Components, true, None));
        p.on_done(done(doc, run, Agent::Hazards, true, None));

        let a = p.on_timeout(doc, run);
        assert_eq!(
            finish(&a),
            Some(&Verdict { status: DocStatus::Partial, error: Some("번역: 시간 초과".into()) })
        );

        // 번역이 아직 일하는 동안 재분석은 대기열에서 기다린다
        assert_eq!(p.on_analyze(doc), vec![Action::Queued { doc_id: doc }]);
        // 늦게 온 번역 AgentDone은 상태를 바꾸지 않고, 남은 에이전트가 없어지면 재분석을 시작한다
        let a = p.on_done(done(doc, run, Agent::Translate, false, Some("시간 초과")));
        assert!(finish(&a).is_none());
        let rerun = started_run(&a, doc);
        assert_ne!(rerun, run);
    }

    #[test]
    fn timeout_before_extract_fails() {
        let mut p = Pipeline::new(3);
        let doc = Uuid::new_v4();
        let run = started_run(&p.on_analyze(doc), doc);
        let a = p.on_timeout(doc, run);
        assert_eq!(
            finish(&a),
            Some(&Verdict { status: DocStatus::Failed, error: Some("추출: 시간 초과".into()) })
        );
    }

    #[test]
    fn env_parsing() {
        assert_eq!(parse_timeout_secs(None), 600);
        assert_eq!(parse_timeout_secs(Some("x")), 600);
        assert_eq!(parse_timeout_secs(Some(" 15 ")), 15);
        assert_eq!(parse_timeout_secs(Some("1")), 10);
        assert_eq!(parse_timeout_secs(Some("99999")), 3600);
        assert_eq!(parse_max_concurrent(None), 3);
        assert_eq!(parse_max_concurrent(Some("0")), 1);
        assert_eq!(parse_max_concurrent(Some("50")), 10);
        assert_eq!(parse_max_concurrent(Some("2")), 2);
    }
}
