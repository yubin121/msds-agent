//! cargo run -- pipeline-test <doc_id>
//! 서버 없이 ActorSystem에 총괄 액터와 에이전트 4개를 띄워 Analyze를 보내고, 최종 판정을 출력한다.
//! 시간 초과로 판정된 뒤에도 일하는 에이전트가 있으면 실행 기록이 남을 때까지 기다려 함께 출력한다.

use std::time::Duration;

use chrono::{DateTime, Utc};
use pekko_actor::ActorSystem;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

use crate::actors::supervisor::{self, SupervisorMsg};

/// 판정 뒤 늦게 끝나는 에이전트를 기다리는 최대 시간 (Claude 호출 timeout 180초보다 길게)
const LATE_WAIT: Duration = Duration::from_secs(300);
const ERROR_PREVIEW_CHARS: usize = 120;

type RunRow = (String, Option<String>, bool, Option<String>, Option<i32>, Option<i32>, Option<i32>, DateTime<Utc>, DateTime<Utc>);

pub async fn run(pool: PgPool, doc_id: &str) -> Result<(), String> {
    let doc_id = Uuid::parse_str(doc_id.trim()).map_err(|_| format!("doc_id가 UUID 형식이 아닙니다: {doc_id}"))?;
    let file_name: Option<(Option<String>,)> = sqlx::query_as("SELECT file_name FROM msds_documents WHERE id = $1")
        .bind(doc_id)
        .fetch_optional(&pool)
        .await
        .map_err(|e| format!("문서 조회 실패: {e}"))?;
    let Some((file_name,)) = file_name else {
        return Err(format!("doc_id {doc_id} 문서가 없습니다"));
    };
    crate::actors::log_models();

    let system = ActorSystem::new("msds");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sup = supervisor::spawn(&system, pool.clone(), Some(tx)).await?;

    let started = Instant::now();
    sup.tell(SupervisorMsg::Analyze { doc_id }).map_err(|e| format!("총괄 액터에 Analyze 전송 실패: {e}"))?;
    let finished = loop {
        let f = rx.recv().await.ok_or("판정을 받지 못했습니다")?;
        if f.doc_id == doc_id {
            break f;
        }
    };
    let elapsed = started.elapsed();
    let decided_at = Utc::now();

    // 판정 뒤에 끝나는 에이전트(시간 초과)의 실행 기록을 기다린다
    let wait_until = Instant::now() + LATE_WAIT;
    let rows: Vec<RunRow> = loop {
        let rows: Vec<RunRow> = sqlx::query_as(
            "SELECT agent, model, ok, error, rounds, input_tokens, output_tokens, started_at, finished_at \
             FROM msds_agent_runs WHERE pipeline_run_id = $1 ORDER BY started_at, id",
        )
        .bind(finished.run_id)
        .fetch_all(&pool)
        .await
        .map_err(|e| format!("실행 기록 조회 실패: {e}"))?;
        if rows.len() >= finished.dispatched.len() || Instant::now() >= wait_until {
            break rows;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };

    println!("\n## {} ({doc_id})", file_name.unwrap_or_default());
    println!("run_id: {}", finished.run_id);
    println!("최종 status: {}", finished.status.as_str());
    println!("error: {}", finished.error.as_deref().map(|e| e.replace('\n', " / ")).unwrap_or_else(|| "-".into()));
    println!("전체 시간(판정까지): {:.1}s", elapsed.as_secs_f64());

    println!("\n| 에이전트 | ok | 시간(s) | 호출 수 | 입력 토큰 | 출력 토큰 | model | 추정 비용(USD) | 비고 |");
    println!("|---|---|---|---|---|---|---|---|---|");
    let (mut input_sum, mut output_sum, mut cost_sum, mut unknown_cost) = (0u64, 0u64, 0.0f64, false);
    for (agent, model, ok, error, rounds, input, output, started_at, finished_at) in &rows {
        let model = model.as_deref().unwrap_or("");
        let (input, output) = (input.unwrap_or(0) as u64, output.unwrap_or(0) as u64);
        input_sum += input;
        output_sum += output;
        let cost = crate::cost::estimate(model, input, output);
        match cost {
            Some(c) => cost_sum += c,
            None => unknown_cost = true,
        }
        let mut note: String = error.as_deref().unwrap_or("").chars().take(ERROR_PREVIEW_CHARS).collect();
        if *finished_at > decided_at {
            note = format!("판정 뒤 끝남. {note}");
        }
        println!(
            "| {agent} | {ok} | {:.1} | {} | {input} | {output} | {model} | {} | {note} |",
            (*finished_at - *started_at).num_milliseconds() as f64 / 1000.0,
            rounds.unwrap_or(0),
            cost.map(|c| format!("{c:.4}")).unwrap_or_else(|| "-".into()),
        );
    }
    if rows.len() < finished.dispatched.len() {
        println!("(실행 기록 {}/{}개: {}초 안에 끝나지 않은 에이전트가 있습니다)", rows.len(), finished.dispatched.len(), LATE_WAIT.as_secs());
    }
    println!(
        "\n문서 전체(pipeline_run_id 합계): 입력 토큰 {input_sum}, 출력 토큰 {output_sum}, 추정 비용 {}",
        if unknown_cost { format!("{cost_sum:.4} USD 이상(단가 모르는 모델 포함)") } else { format!("{cost_sum:.4} USD") }
    );
    Ok(())
}
