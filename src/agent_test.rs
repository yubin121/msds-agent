//! cargo run -- agent-test <extract|components|both|hazards|translate> <doc_id>
//! ActorSystem을 띄워 에이전트 액터에 Run을 보내고 AgentDone을 받아 결과를 출력한다.


use std::time::Instant;

use async_trait::async_trait;
use pekko_actor::{Actor, ActorContext, ActorSystem};
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::actors::{Agent, AgentActor, AgentDone, AgentMsg};

/// AgentDone을 받아 테스트 명령으로 넘기는 액터
struct DoneCollector {
    tx: mpsc::UnboundedSender<AgentDone>,
}

#[async_trait]
impl Actor for DoneCollector {
    type Message = AgentDone;

    async fn receive(&mut self, msg: AgentDone, _ctx: &mut ActorContext<Self>) {
        let _ = self.tx.send(msg);
    }
}

pub async fn run(pool: PgPool, mode: &str, doc_id: &str) -> Result<(), String> {
    let agents: &[Agent] = match mode {
        "extract" => &[Agent::Extract],
        "components" => &[Agent::Components],
        "both" => &[Agent::Extract, Agent::Components],
        "hazards" => &[Agent::Hazards],
        "translate" => &[Agent::Translate],
        other => return Err(format!("알 수 없는 모드: {other} (extract, components, both, hazards, translate)")),
    };
    let doc_id = Uuid::parse_str(doc_id.trim()).map_err(|_| format!("doc_id가 UUID 형식이 아닙니다: {doc_id}"))?;
    crate::actors::log_models();

    let system = ActorSystem::new("msds");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let collector = system.spawn(DoneCollector { tx }, "agent-test").await.map_err(|e| e.to_string())?;

    let total = Instant::now();
    let mut dones = Vec::new();
    for &agent in agents {
        let actor = system
            .spawn(AgentActor::<DoneCollector>::from_env(agent, pool.clone())?, agent.as_str())
            .await
            .map_err(|e| e.to_string())?;
        let started = Instant::now();
        actor
            .tell(AgentMsg::Run { doc_id, reply_to: collector.clone() })
            .map_err(|e| format!("{} 액터에 Run 전송 실패: {e}", agent.as_str()))?;
        let done = rx.recv().await.ok_or("AgentDone을 받지 못했습니다")?;
        if done.doc_id != doc_id {
            return Err(format!("다른 문서의 AgentDone이 왔습니다: {}", done.doc_id));
        }
        let elapsed = started.elapsed();
        println!(
            "[{}] model={} ok={} rounds={} input_tokens={} output_tokens={} 걸린 시간={:.1}s{}",
            done.agent.as_str(),
            done.model,
            done.ok,
            done.rounds,
            done.input_tokens,
            done.output_tokens,
            elapsed.as_secs_f64(),
            done.error.as_deref().map(|e| format!(" error={e}")).unwrap_or_default(),
        );
        let ok = done.ok;
        dones.push(done);
        if !ok && agent == Agent::Extract && agents.len() > 1 {
            println!("추출이 실패해 성분 에이전트는 실행하지 않습니다");
            break;
        }
    }

    print_sections(&pool, doc_id).await?;
    if agents.contains(&Agent::Components) {
        print_components(&pool, doc_id).await?;
    }
    if agents.contains(&Agent::Hazards) {
        print_hazards(&pool, doc_id).await?;
    }
    if agents.contains(&Agent::Translate) {
        print_translations(&pool, doc_id).await?;
    }
    let (rounds, input, output) = dones
        .iter()
        .fold((0, 0, 0), |(r, i, o), d| (r + d.rounds, i + d.input_tokens, o + d.output_tokens));
    println!(
        "\n합계: rounds={rounds} input_tokens={input} output_tokens={output} 걸린 시간={:.1}s",
        total.elapsed().as_secs_f64()
    );
    Ok(())
}

async fn print_sections(pool: &PgPool, doc_id: Uuid) -> Result<(), String> {
    let rows: Vec<(i32, Option<String>, i32, Option<i32>, Option<i32>, Option<i32>, i32)> = sqlx::query_as(
        "SELECT product_no, product_name, section_no, original_no, page_no, line_no, \
         coalesce(char_length(text), 0) FROM msds_sections WHERE doc_id = $1 ORDER BY product_no, section_no",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("섹션 조회 실패: {e}"))?;

    println!("\n## 섹션 (제품별 section_no[original_no]:글자 수)");
    let mut current = None;
    let mut line = String::new();
    for (p, name, s, orig, _page, _line, chars) in &rows {
        if current != Some(*p) {
            if !line.is_empty() {
                println!("{line}");
            }
            line = format!("제품 {p} ({}): ", name.as_deref().unwrap_or("-"));
            current = Some(*p);
        }
        let orig = orig.map(|o| format!("[{o}]")).unwrap_or_default();
        line.push_str(&format!("{s}{orig}:{chars} "));
    }
    if line.is_empty() {
        println!("(없음)");
    } else {
        println!("{line}");
    }
    Ok(())
}

async fn print_components(pool: &PgPool, doc_id: Uuid) -> Result<(), String> {
    let rows: Vec<(i32, Option<String>, Option<String>, Option<String>, Option<bool>, Option<String>)> =
        sqlx::query_as(
            "SELECT product_no, product_name, component, cas_no, cas_valid, percent FROM msds_components \
             WHERE doc_id = $1 ORDER BY product_no, id",
        )
        .bind(doc_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("성분 조회 실패: {e}"))?;

    println!("\n## 성분");
    println!("| 제품 | 성분 | CAS | cas_valid | 함유량 |");
    println!("|---|---|---|---|---|");
    for (p, name, comp, cas, valid, pct) in rows {
        println!(
            "| {p} {} | {} | {} | {} | {} |",
            name.unwrap_or_default(),
            comp.unwrap_or_default(),
            cas.unwrap_or_default(),
            valid.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            pct.unwrap_or_default(),
        );
    }
    Ok(())
}

async fn print_hazards(pool: &PgPool, doc_id: Uuid) -> Result<(), String> {
    let rows: Vec<(i32, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as(
            "SELECT product_no, h_code, ghs_code, source, signal_word, note FROM msds_hazards \
             WHERE doc_id = $1 ORDER BY product_no, id",
        )
        .bind(doc_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("유해성 조회 실패: {e}"))?;

    println!("\n## 그림문자 (제품별)");
    let mut products: std::collections::BTreeMap<i32, Vec<_>> = std::collections::BTreeMap::new();
    for r in rows {
        products.entry(r.0).or_default().push(r);
    }
    for (p, rows) in products {
        let ghs: std::collections::BTreeSet<&str> = rows.iter().filter_map(|r| r.2.as_deref()).collect();
        let mut codes: Vec<String> = Vec::new();
        for r in &rows {
            if let Some(h) = &r.1 {
                let label = format!("{h}({})", r.3.as_deref().unwrap_or("-"));
                if !codes.contains(&label) {
                    codes.push(label);
                }
            }
        }
        let first = &rows[0];
        println!(
            "제품 {p}: 그림문자 [{}] 신호어={} H코드(출처)={}{}",
            ghs.into_iter().collect::<Vec<_>>().join(", "),
            first.4.as_deref().unwrap_or("-"),
            if codes.is_empty() { "없음(no_h_codes)".to_string() } else { codes.join(" ") },
            first.5.as_deref().map(|n| format!(" note={n}")).unwrap_or_default(),
        );
    }
    Ok(())
}

async fn print_translations(pool: &PgPool, doc_id: Uuid) -> Result<(), String> {
    let (language,): (Option<String>,) = sqlx::query_as("SELECT language FROM msds_documents WHERE id = $1")
        .bind(doc_id)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("문서 조회 실패: {e}"))?;
    if language.as_deref() == Some("ko") {
        println!("\n## 번역\n{}", crate::actors::translate::KO_SKIP_NOTE);
        return Ok(());
    }
    let rows: Vec<(i32, i32, i32, i32, i64)> = sqlx::query_as(
        "SELECT s.product_no, s.section_no, coalesce(char_length(s.text), 0), \
         coalesce((SELECT sum(char_length(t.text_ko)) FROM msds_translations t WHERE t.doc_id = s.doc_id \
           AND t.product_no = s.product_no AND t.section_no = s.section_no), 0)::int, \
         (SELECT count(*) FROM msds_translations t WHERE t.doc_id = s.doc_id \
           AND t.product_no = s.product_no AND t.section_no = s.section_no) \
         FROM msds_sections s WHERE s.doc_id = $1 ORDER BY s.product_no, s.section_no",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("번역 조회 실패: {e}"))?;

    let targets = crate::actors::translate::Targets::from_env()?;
    let check = crate::actors::translate::check_saved(pool, doc_id, &targets).await?;
    println!(
        "\n## 번역 (대상 섹션 {:?}, 제품 {})",
        targets.sections,
        targets.products.as_ref().map(|p| format!("{p:?}")).unwrap_or_else(|| "전부".into())
    );
    for (p, lines) in &check.furniture {
        println!("제품 {p} 페이지 머리말·꼬리말 {}줄 (번역 입력에서 뺌)", lines.len());
        for l in lines {
            println!("  - {l}");
        }
    }
    println!("| 제품 | 섹션 | 원문 글자 | 번역 글자 | 조각 | 라틴 문자 비율 |");
    println!("|---|---|---|---|---|---|");
    for (p, s, src, ko, chunks) in rows.iter().filter(|r| targets.contains(r.0, r.1)) {
        let latin = check
            .latin
            .iter()
            .find(|l| l.0 == *p && l.1 == *s)
            .map(|l| format!("{:.1}%", l.2 * 100.0))
            .unwrap_or_else(|| "-".into());
        println!("| {p} | {s} | {src} | {ko} | {chunks} | {latin} |");
    }
    if !check.latin.is_empty() {
        let avg = check.latin.iter().map(|l| l.2).sum::<f64>() / check.latin.len() as f64;
        println!("섹션 평균 라틴 문자 비율: {:.1}%", avg * 100.0);
    }
    println!("빠진 섹션: {}", if check.missing.is_empty() { "없음".into() } else { check.missing.join(", ") });
    println!("사후 검사 경고 {}건", check.warnings.len());
    for w in &check.warnings {
        println!("- {w}");
    }
    Ok(())
}
