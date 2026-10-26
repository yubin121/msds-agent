//! 번역 에이전트: 섹션마다 툴 루프를 따로 실행해 msds_translations에 저장한다.
//! 대화를 섹션마다 새로 시작해 입력 토큰이 쌓이지 않게 하고, 동시 실행 수는 MSDS_TRANSLATE_CONCURRENCY로 정한다.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, LazyLock};

use pekko_agent_llm::ClaudeClient;
use pekko_agent_tools::ToolRegistry;
use regex::Regex;
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};
use uuid::Uuid;

use super::{load_products, JobOutcome, NO_SECTIONS_ERROR};
use crate::prompts::{TRANSLATE_MAX_TOKENS, TRANSLATE_READ_CHARS, TRANSLATE_SYSTEM};
use crate::tool_loop::run_tool_loop;
use crate::tools::cas::{check_cas, CasCheck};
use crate::tools::{GetSectionTool, SaveTranslationTool};

pub const KO_SKIP_NOTE: &str = "한글 문서라 번역 생략";
const DEFAULT_CONCURRENCY: usize = 4;
const MAX_CONCURRENCY: usize = 16;

/// MSDS_TRANSLATE_SECTIONS 해석. 비어 있으면 1~16 전부. 예: "1-4,7,8"
pub fn parse_sections(raw: Option<&str>) -> Result<BTreeSet<i32>, String> {
    let raw = raw.unwrap_or("").trim();
    if raw.is_empty() {
        return Ok((1..=16).collect());
    }
    let err = |part: &str| format!("MSDS_TRANSLATE_SECTIONS 형식 오류: '{part}' (예: 1-4,7,8, 번호는 1~16)");
    let num = |s: &str, part: &str| -> Result<i32, String> {
        s.trim().parse::<i32>().ok().filter(|n| (1..=16).contains(n)).ok_or_else(|| err(part))
    };
    let mut out = BTreeSet::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (num(a, part)?, num(b, part)?);
                if a > b {
                    return Err(err(part));
                }
                out.extend(a..=b);
            }
            None => {
                out.insert(num(part, part)?);
            }
        }
    }
    if out.is_empty() {
        return Err(err(raw));
    }
    Ok(out)
}

/// MSDS_TRANSLATE_CONCURRENCY 해석. 없거나 숫자가 아니면 4, 1~16으로 맞춘다.
pub fn parse_concurrency(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, MAX_CONCURRENCY))
        .unwrap_or(DEFAULT_CONCURRENCY)
}

// 영어 원문에서 찾는다. ASCII 단어 경계라 한글 조사와 붙어 있어도("H301을") 찾는다
static CAS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\b)\d{2,7}-\d{2}-\d(?-u:\b)").unwrap());
// H200~H499, P100~P599. "H360Fd"는 H360으로 본다. EUH 코드는 제외(앞이 U라 경계가 아님)
static CODE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\b)(?:H[2-4]\d{2}|P[1-5]\d{2})").unwrap());

/// 번역문에 그대로 남아야 하는 원문 토큰: 체크섬이 맞는 CAS 번호, H·P 코드, 제품명(원문에 있을 때)
pub fn preserved_tokens(original: &str, product_name: Option<&str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let cas = CAS_RE
        .find_iter(original)
        .map(|m| m.as_str())
        .filter(|c| matches!(check_cas(Some(c)), CasCheck::Valid(_)));
    let codes = CODE_RE.find_iter(original).map(|m| m.as_str());
    for t in cas.chain(codes) {
        if seen.insert(t) {
            out.push(t.to_string());
        }
    }
    if let Some(name) = product_name.map(str::trim).filter(|n| !n.is_empty()) {
        if original.contains(name) && seen.insert(name) {
            out.push(name.to_string());
        }
    }
    out
}

/// 사후 검사: 원문의 보존 토큰 중 번역문에 없는 것
pub fn missing_in_translation(original: &str, translated: &str, product_name: Option<&str>) -> Vec<String> {
    preserved_tokens(original, product_name).into_iter().filter(|t| !translated.contains(t.as_str())).collect()
}

/// 번역할 섹션 하나
struct SectionJob {
    product_no: i32,
    product_name: Option<String>,
    section_no: i32,
    title: String,
    chars: usize,
}

struct SectionRun {
    product_no: i32,
    section_no: i32,
    rounds: u32,
    input_tokens: u32,
    output_tokens: u32,
    max_tokens_stops: u32,
    error: Option<String>,
}

pub async fn run(client: &Arc<ClaudeClient>, pool: &PgPool, doc_id: Uuid) -> Result<JobOutcome, String> {
    let language: Option<(Option<String>,)> = sqlx::query_as("SELECT language FROM msds_documents WHERE id = $1")
        .bind(doc_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("문서 조회 실패: {e}"))?;
    let Some((language,)) = language else {
        return Ok(JobOutcome::failed(format!("doc_id {doc_id} 문서가 없습니다")));
    };
    if language.as_deref() == Some("ko") {
        info!(%doc_id, "{KO_SKIP_NOTE}");
        return Ok(JobOutcome::skipped(KO_SKIP_NOTE));
    }
    if load_products(pool, doc_id).await?.is_empty() {
        return Ok(JobOutcome::failed(NO_SECTIONS_ERROR));
    }
    let targets = match parse_sections(std::env::var("MSDS_TRANSLATE_SECTIONS").ok().as_deref()) {
        Ok(t) => t,
        Err(e) => return Ok(JobOutcome::failed(e)),
    };
    let concurrency = parse_concurrency(std::env::var("MSDS_TRANSLATE_CONCURRENCY").ok().as_deref());

    let rows: Vec<(i32, Option<String>, i32, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT product_no, product_name, section_no, title, text FROM msds_sections \
         WHERE doc_id = $1 ORDER BY product_no, section_no",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("섹션 조회 실패: {e}"))?;
    let jobs: Vec<SectionJob> = rows
        .into_iter()
        .filter(|r| targets.contains(&r.2))
        .map(|(product_no, product_name, section_no, title, text)| SectionJob {
            product_no,
            product_name,
            section_no,
            title: title.unwrap_or_default(),
            chars: text.unwrap_or_default().chars().count(),
        })
        .collect();
    info!(%doc_id, sections = jobs.len(), concurrency, "번역 시작");

    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut set = JoinSet::new();
    for job in jobs {
        let (client, pool, semaphore) = (client.clone(), pool.clone(), semaphore.clone());
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("세마포어가 닫혔습니다");
            translate_section(&client, &pool, doc_id, job).await
        });
    }

    let mut outcome = JobOutcome { ok: true, error: None, rounds: 0, input_tokens: 0, output_tokens: 0 };
    let mut warnings = Vec::new();
    while let Some(joined) = set.join_next().await {
        let r = match joined {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("섹션 작업이 비정상 종료되었습니다: {e}"));
                continue;
            }
        };
        outcome.rounds += r.rounds;
        outcome.input_tokens += r.input_tokens;
        outcome.output_tokens += r.output_tokens;
        if r.max_tokens_stops > 0 {
            warnings.push(format!(
                "제품 {} {}번: max_tokens로 잘린 응답 {}회",
                r.product_no, r.section_no, r.max_tokens_stops
            ));
        }
        if let Some(e) = r.error {
            warnings.push(format!("제품 {} {}번: {e}", r.product_no, r.section_no));
        }
    }

    // 성공 판정과 사후 검사는 DB에 저장된 결과로 한다
    let check = check_saved(pool, doc_id, &targets).await?;
    warnings.extend(check.warnings);
    if !check.missing.is_empty() {
        outcome.ok = false;
        warnings.insert(0, format!("번역되지 않은 섹션: {}", check.missing.join(", ")));
    }
    if !warnings.is_empty() {
        outcome.error = Some(warnings.join("; "));
    }
    Ok(outcome)
}

async fn translate_section(client: &ClaudeClient, pool: &PgPool, doc_id: Uuid, job: SectionJob) -> SectionRun {
    let mut run = SectionRun {
        product_no: job.product_no,
        section_no: job.section_no,
        rounds: 0,
        input_tokens: 0,
        output_tokens: 0,
        max_tokens_stops: 0,
        error: None,
    };
    // 조각 수가 줄면 옛 조각이 남으므로 섹션 번역을 먼저 지운다
    if let Err(e) = sqlx::query("DELETE FROM msds_translations WHERE doc_id = $1 AND product_no = $2 AND section_no = $3")
        .bind(doc_id)
        .bind(job.product_no)
        .bind(job.section_no)
        .execute(pool)
        .await
    {
        run.error = Some(format!("기존 번역 삭제 실패: {e}"));
        return run;
    }

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GetSectionTool::new(pool.clone())));
    registry.register(Arc::new(SaveTranslationTool::new(pool.clone())));

    let chunks = job.chars.div_ceil(TRANSLATE_READ_CHARS).max(1);
    let user = format!(
        "doc_id: {doc_id}\nproduct_no: {} ({})\nsection_no: {}\n제목: {}\n원문 {}자, 조각 {chunks}개(chunk_no 0~{}).\n\n\
         msds_get_section을 max_chars {TRANSLATE_READ_CHARS}으로 읽고, 읽은 조각마다 번역해 저장하라.",
        job.product_no,
        job.product_name.as_deref().unwrap_or("제품명 없음"),
        job.section_no,
        job.title,
        job.chars,
        chunks - 1,
    );
    match run_tool_loop(client, &mut registry, TRANSLATE_SYSTEM, user, TRANSLATE_MAX_TOKENS).await {
        Ok(r) => {
            info!(
                %doc_id,
                product_no = job.product_no,
                section_no = job.section_no,
                rounds = r.rounds,
                input_tokens = r.input_tokens,
                output_tokens = r.output_tokens,
                "섹션 번역 끝"
            );
            run.rounds = r.rounds;
            run.input_tokens = r.input_tokens;
            run.output_tokens = r.output_tokens;
            run.max_tokens_stops = r.max_tokens_stops;
        }
        Err(e) => {
            warn!(%doc_id, product_no = job.product_no, section_no = job.section_no, "섹션 번역 실패: {e}");
            run.error = Some(e.to_string());
        }
    }
    run
}

pub struct SavedCheck {
    /// 원문에 있는데 번역이 저장되지 않은 섹션 ("제품 1 3번")
    pub missing: Vec<String>,
    /// 사후 검사 경고
    pub warnings: Vec<String>,
}

/// 대상 섹션마다 번역 저장 여부와 보존 토큰을 확인한다 (agent-test 출력에도 쓴다)
pub async fn check_saved(pool: &PgPool, doc_id: Uuid, targets: &BTreeSet<i32>) -> Result<SavedCheck, String> {
    let rows: Vec<(i32, Option<String>, i32, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT s.product_no, s.product_name, s.section_no, s.text, \
         (SELECT string_agg(t.text_ko, E'\\n' ORDER BY t.chunk_no) FROM msds_translations t \
          WHERE t.doc_id = s.doc_id AND t.product_no = s.product_no AND t.section_no = s.section_no) \
         FROM msds_sections s WHERE s.doc_id = $1 ORDER BY s.product_no, s.section_no",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("번역 조회 실패: {e}"))?;

    let mut out = SavedCheck { missing: Vec::new(), warnings: Vec::new() };
    for (p, name, s, original, translated) in rows {
        if !targets.contains(&s) {
            continue;
        }
        let Some(translated) = translated else {
            out.missing.push(format!("제품 {p} {s}번"));
            continue;
        };
        let lost = missing_in_translation(original.as_deref().unwrap_or(""), &translated, name.as_deref());
        if !lost.is_empty() {
            out.warnings.push(format!("제품 {p} {s}번 번역문에 없음: {}", lost.join(", ")));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_env() {
        assert_eq!(parse_sections(None).unwrap(), (1..=16).collect());
        assert_eq!(parse_sections(Some("  ")).unwrap(), (1..=16).collect());
        assert_eq!(parse_sections(Some("1-4,7,8")).unwrap(), BTreeSet::from([1, 2, 3, 4, 7, 8]));
        assert_eq!(parse_sections(Some(" 2 , 8 ,")).unwrap(), BTreeSet::from([2, 8]));
        assert_eq!(parse_sections(Some("3-3,2")).unwrap(), BTreeSet::from([2, 3]));
        for bad in ["0", "17", "4-2", "a", "1-", "1-20", ","] {
            assert!(parse_sections(Some(bad)).unwrap_err().contains("형식 오류"), "{bad}");
        }
    }

    #[test]
    fn concurrency_env() {
        assert_eq!(parse_concurrency(None), 4);
        assert_eq!(parse_concurrency(Some("x")), 4);
        assert_eq!(parse_concurrency(Some("0")), 1);
        assert_eq!(parse_concurrency(Some(" 8 ")), 8);
        assert_eq!(parse_concurrency(Some("100")), 16);
    }

    #[test]
    fn tokens_from_original() {
        let original = "Air Tool Lubricant\nPetroleum oil 64742-54-7, 64742-65-0 >99%\n\
                        H301+H311 Toxic. H360Fd May damage fertility. EUH066 dryness.\n\
                        P280 Wear gloves. P301+P310: IF SWALLOWED\n\
                        Revised 2021-09-27, tel (216) 566-2902, bad CAS 7732-18-4";
        assert_eq!(
            preserved_tokens(original, Some("Air Tool Lubricant")),
            ["64742-54-7", "64742-65-0", "H301", "H311", "H360", "P280", "P301", "P310", "Air Tool Lubricant"]
        );
        // 제품명이 원문 섹션에 없으면 확인하지 않는다
        assert!(!preserved_tokens("H225", Some("Other")).contains(&"Other".to_string()));
    }

    #[test]
    fn post_check_reports_missing() {
        let original = "Product: Air Tool Lubricant\nH315 Causes skin irritation. CAS 128-37-0";
        let good = "제품: Air Tool Lubricant\nH315 피부에 자극을 일으킴. CAS 128-37-0";
        assert!(missing_in_translation(original, good, Some("Air Tool Lubricant")).is_empty());
        // 한글 조사가 붙어 있어도 남아 있으면 통과
        assert!(missing_in_translation("H315", "H315을 보라", None).is_empty());

        let bad = "제품: 에어 툴 윤활유\nH316 피부 자극. CAS 128-37-0";
        assert_eq!(missing_in_translation(original, bad, Some("Air Tool Lubricant")), ["H315", "Air Tool Lubricant"]);
    }
}
