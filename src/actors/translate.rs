//! 번역 에이전트: 툴 루프 없이 코드가 섹션을 조각으로 나눠 조각마다 Claude를 한 번 부르고 msds_translations에 저장한다.
//! 판단이 필요 없는 단계라 툴을 쓰지 않는다(툴 루프는 시스템 프롬프트·툴 정의가 라운드마다 다시 들어가 입력 토큰이 원문의 약 10배였다).
//! 섹션 단위로 동시에 실행하고, 동시 실행 수는 MSDS_TRANSLATE_CONCURRENCY로 정한다.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::RangeInclusive;
use std::sync::{Arc, LazyLock};

use pekko_agent_llm::ClaudeClient;
use regex::Regex;
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::{info, warn};
use uuid::Uuid;

use super::{load_products, past_deadline, JobOutcome, NO_SECTIONS_ERROR, TIMEOUT_ERROR};
use crate::prompts::{SECTION_TITLES_KO, TRANSLATE_CHUNK_CHARS, TRANSLATE_MAX_TOKENS, TRANSLATE_SYSTEM};
use crate::tool_loop::call_once;
use crate::tools::cas::{check_cas, CasCheck};
use crate::tools::save_translation::{save_chunk, TranslationChunk};

pub const KO_SKIP_NOTE: &str = "한글 문서라 번역 생략";
const DEFAULT_CONCURRENCY: usize = 4;
const MAX_CONCURRENCY: usize = 16;
const MAX_PRODUCT_NO: i32 = 999;
/// 원문에 없는 '→' 줄이 이 수 이상이면 원문·번역 쌍 형식으로 의심한다
const PAIR_LINES_WARN: usize = 1;
/// 페이지 머리말·꼬리말 후보: 페이지마다 맨 위·맨 아래에서 빈 줄을 뺀 이 수만큼의 줄
/// (ARALDITE 머리말이 7줄이라 4줄로는 'Version…', 'Print Date' 줄이 빠졌다)
const PAGE_EDGE_LINES: usize = 8;
/// 머리말·꼬리말로 보려면 최소 이 페이지 수 이상에 나와야 한다(그리고 범위 페이지의 절반 이상)
const MIN_FURNITURE_PAGES: usize = 3;
/// 경고에 남길 한자·가나 최대 개수
const MAX_CJK_REPORT: usize = 10;

/// MSDS_TRANSLATE_SECTIONS 해석. 비어 있으면 1~16 전부. 예: "1-4,7,8"
pub fn parse_sections(raw: Option<&str>) -> Result<BTreeSet<i32>, String> {
    Ok(parse_number_set(raw, "MSDS_TRANSLATE_SECTIONS", 16)?.unwrap_or_else(|| (1..=16).collect()))
}

/// MSDS_TRANSLATE_PRODUCTS 해석. 비어 있으면 None(모든 제품). 예: "1", "1-2"
pub fn parse_products(raw: Option<&str>) -> Result<Option<BTreeSet<i32>>, String> {
    parse_number_set(raw, "MSDS_TRANSLATE_PRODUCTS", MAX_PRODUCT_NO)
}

/// "1-4,7,8" 형식의 번호 목록(1~max). 비어 있으면 None
fn parse_number_set(raw: Option<&str>, name: &str, max: i32) -> Result<Option<BTreeSet<i32>>, String> {
    let raw = raw.unwrap_or("").trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let err = |part: &str| format!("{name} 형식 오류: '{part}' (예: 1-4,7,8, 번호는 1~{max})");
    let num = |s: &str, part: &str| -> Result<i32, String> {
        s.trim().parse::<i32>().ok().filter(|n| (1..=max).contains(n)).ok_or_else(|| err(part))
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
    Ok(Some(out))
}

/// 번역 대상: 섹션(MSDS_TRANSLATE_SECTIONS)과 제품(MSDS_TRANSLATE_PRODUCTS, None이면 전부)
#[derive(Debug, Clone)]
pub struct Targets {
    pub sections: BTreeSet<i32>,
    pub products: Option<BTreeSet<i32>>,
}

impl Targets {
    pub fn from_env() -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).ok();
        Ok(Self {
            sections: parse_sections(var("MSDS_TRANSLATE_SECTIONS").as_deref())?,
            products: parse_products(var("MSDS_TRANSLATE_PRODUCTS").as_deref())?,
        })
    }

    pub fn contains(&self, product_no: i32, section_no: i32) -> bool {
        self.sections.contains(&section_no) && self.products.as_ref().is_none_or(|p| p.contains(&product_no))
    }
}

/// MSDS_TRANSLATE_CONCURRENCY 해석. 없거나 숫자가 아니면 4, 1~16으로 맞춘다.
pub fn parse_concurrency(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, MAX_CONCURRENCY))
        .unwrap_or(DEFAULT_CONCURRENCY)
}

// 영어 원문에서 찾는다. ASCII 단어 경계라 한글 조사와 붙어 있어도("H301을") 찾는다
pub(crate) static CAS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\b)\d{2,7}-\d{2}-\d(?-u:\b)").unwrap());
// H200~H499, P100~P599. "H360Fd"는 H360으로 본다. EUH 코드는 제외(앞이 U라 경계가 아님)
pub(crate) static CODE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\b)(?:H[2-4]\d{2}|P[1-5]\d{2})").unwrap());

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

/// 사후 검사: '→'가 들어간 줄 중 원문보다 늘어난 줄 수 ("원문 → 번역" 쌍 형식 의심)
pub fn added_arrow_lines(original: &str, translated: &str) -> usize {
    let count = |t: &str| t.lines().filter(|l| l.contains('→')).count();
    count(translated).saturating_sub(count(original))
}

fn is_cjk(c: char) -> bool {
    // 한자(CJK 통합 한자), 히라가나·가타카나
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3040}'..='\u{30FF}')
}

/// 사후 검사: 원문에 없는 한자·가나(나온 순서, 중복 제거, 최대 10개)
pub fn foreign_cjk(original: &str, translated: &str) -> Vec<char> {
    let mut out = Vec::new();
    for c in translated.chars().filter(|&c| is_cjk(c) && !original.contains(c)) {
        if !out.contains(&c) {
            out.push(c);
            if out.len() == MAX_CJK_REPORT {
                break;
            }
        }
    }
    out
}

/// 공백을 뺀 글자 중 라틴 문자(ASCII 영문자와 À~ɏ) 비율. 번역문에 영어가 남은 정도를 본다. 빈 글은 0
pub fn latin_ratio(text: &str) -> f64 {
    let (mut total, mut latin) = (0usize, 0usize);
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        total += 1;
        if c.is_ascii_alphabetic() || (c.is_alphabetic() && ('\u{00C0}'..='\u{024F}').contains(&c)) {
            latin += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        latin as f64 / total as f64
    }
}

/// 사후 검사 경고 문구들 (보존 토큰, 쌍 형식, 한자·가나)
pub fn post_check(original: &str, translated: &str, product_name: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let lost = missing_in_translation(original, translated, product_name);
    if !lost.is_empty() {
        out.push(format!("번역문에 없음: {}", lost.join(", ")));
    }
    let arrows = added_arrow_lines(original, translated);
    if arrows >= PAIR_LINES_WARN {
        out.push(format!("원문·번역 쌍 형식 의심('→' 줄 {arrows}개)"));
    }
    let cjk = foreign_cjk(original, translated);
    if !cjk.is_empty() {
        out.push(format!("원문에 없는 한자·가나: {}", cjk.iter().map(char::to_string).collect::<Vec<_>>().join(" ")));
    }
    out
}

/// 사후 검사 경고가 난 조각을 한 번 다시 번역했을 때 저장할 결과: 다시 번역한 것의 경고가 더 적을 때만 바꾼다.
/// (번역문, 경고)와 다시 번역한 것을 채택했는지를 돌려준다
pub fn pick_translation(original: (String, Vec<String>), retry: (String, Vec<String>)) -> (String, Vec<String>, bool) {
    if retry.1.len() < original.1.len() {
        (retry.0, retry.1, true)
    } else {
        (original.0, original.1, false)
    }
}

/// 머리말·꼬리말 비교용 정규화: 숫자 묶음을 #로, 공백을 한 칸으로 ("Page 2 of 6" → "Page # of #")
pub fn normalize_edge_line(line: &str) -> String {
    let mut out = String::new();
    let mut prev_digit = false;
    for c in line.chars() {
        if c.is_ascii_digit() {
            if !prev_digit {
                out.push('#');
            }
            prev_digit = true;
        } else {
            out.push(c);
            prev_digit = false;
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 페이지 머리말·꼬리말(정규화한 줄). 페이지마다 맨 위·맨 아래 PAGE_EDGE_LINES줄(빈 줄 제외)을 후보로 보고,
/// 페이지 수의 절반 이상(최소 3페이지)에 나오는 줄을 고른다. 페이지가 3장 미만이면 없음.
pub fn find_page_furniture(pages: &[&str]) -> BTreeSet<String> {
    let threshold = pages.len().div_ceil(2).max(MIN_FURNITURE_PAGES);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for page in pages {
        let lines: Vec<&str> = page.lines().filter(|l| !l.trim().is_empty()).collect();
        let n = lines.len();
        let edge = lines[..n.min(PAGE_EDGE_LINES)].iter().chain(&lines[n.saturating_sub(PAGE_EDGE_LINES)..]);
        // 한 페이지에서는 한 번만 센다
        let per_page: BTreeSet<String> = edge.map(|l| normalize_edge_line(l)).filter(|l| !l.is_empty()).collect();
        for line in per_page {
            *counts.entry(line).or_default() += 1;
        }
    }
    counts.into_iter().filter(|(_, c)| *c >= threshold).map(|(l, _)| l).collect()
}

/// 머리말·꼬리말 줄을 뺀다(정규화해서 비교). 다른 줄은 그대로
pub fn strip_furniture(text: &str, furniture: &BTreeSet<String>) -> String {
    if furniture.is_empty() {
        return text.to_string();
    }
    text.split_inclusive('\n')
        .filter(|l| !furniture.contains(&normalize_edge_line(l)))
        .collect()
}

/// 제품별 페이지 범위: 그 제품의 첫 섹션 페이지부터 다음 제품의 첫 페이지까지(경계 페이지는 양쪽에 포함), 마지막 제품은 끝 페이지까지.
/// 다중 제품 문서는 제품마다 머리말이 달라(ARALDITE RESIN 28쪽, HARDENER 23쪽) 문서 전체로 세면 절반에 못 미친다.
/// starts는 (product_no, 첫 페이지)이고 product_no 순서
pub fn product_page_ranges(starts: &[(i32, i32)], last_page: i32) -> Vec<(i32, RangeInclusive<i32>)> {
    starts
        .iter()
        .enumerate()
        .map(|(i, &(product_no, start))| {
            let end = starts.get(i + 1).map(|&(_, next)| next).unwrap_or(last_page).max(start);
            (product_no, start..=end)
        })
        .collect()
}

/// 제품마다 페이지 머리말·꼬리말을 찾는다(번역 입력과 사후 검사에서 뺀다). msds_pages는 바꾸지 않는다
pub async fn load_page_furniture(pool: &PgPool, doc_id: Uuid) -> Result<BTreeMap<i32, BTreeSet<String>>, String> {
    let pages: Vec<(i32, Option<String>)> =
        sqlx::query_as("SELECT page_no, text FROM msds_pages WHERE doc_id = $1 ORDER BY page_no")
            .bind(doc_id)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("페이지 조회 실패: {e}"))?;
    let starts: Vec<(i32, i32)> = sqlx::query_as(
        "SELECT product_no, min(page_no) FROM msds_sections WHERE doc_id = $1 AND page_no IS NOT NULL \
         GROUP BY product_no ORDER BY product_no",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("섹션 조회 실패: {e}"))?;
    let last_page = pages.last().map(|p| p.0).unwrap_or(0);
    Ok(product_page_ranges(&starts, last_page)
        .into_iter()
        .map(|(product_no, range)| {
            let texts: Vec<&str> =
                pages.iter().filter(|p| range.contains(&p.0)).map(|p| p.1.as_deref().unwrap_or("")).collect();
            (product_no, find_page_furniture(&texts))
        })
        .collect())
}

/// 섹션 본문을 max_chars자 안에서 마지막 줄바꿈 뒤에서 자른다. 줄바꿈이 없으면 max_chars자에서 자른다.
/// 조각을 이어 붙이면 원문과 같다(줄바꿈은 앞 조각 끝에 남는다).
pub fn split_chunks(text: &str, max_chars: usize) -> Vec<&str> {
    let max_chars = max_chars.max(1);
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        // max_chars번째 글자의 바이트 위치. 남은 글이 max_chars자 이하면 전부 한 조각
        let Some((limit, _)) = rest.char_indices().nth(max_chars) else {
            out.push(rest);
            break;
        };
        let cut = match rest[..limit].rfind('\n') {
            Some(i) if i > 0 => i + 1,
            _ => limit,
        };
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out
}

/// 첫 줄(앞의 빈 줄 무시)이 섹션 제목과 같으면(공백·대소문자 무시) 그 줄을 뺀 나머지, 아니면 그대로
pub fn strip_title_line<'a>(text: &'a str, title: &str) -> &'a str {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let title = norm(title);
    if title.is_empty() {
        return text;
    }
    let body = text.trim_start_matches(['\r', '\n']);
    let (first, rest) = body.split_once('\n').unwrap_or((body, ""));
    if norm(first) == title {
        rest
    } else {
        text
    }
}

/// 표준 한국어 제목 "2. 유해성·위험성"
pub fn section_title_ko(section_no: i32) -> Option<String> {
    let title = SECTION_TITLES_KO.get(usize::try_from(section_no).ok()?.checked_sub(1)?)?;
    Some(format!("{section_no}. {title}"))
}

/// 번역할 섹션 하나
struct SectionJob {
    product_no: i32,
    product_name: Option<String>,
    section_no: i32,
    title: String,
    text: String,
    /// 그 제품의 페이지 머리말·꼬리말(번역 입력에서 뺀다)
    furniture: Arc<BTreeSet<String>>,
}

struct SectionRun {
    product_no: i32,
    section_no: i32,
    /// Claude 호출 수 (실행 기록 rounds에 더한다)
    rounds: u32,
    input_tokens: u32,
    output_tokens: u32,
    max_tokens_stops: u32,
    /// 사후 검사 경고로 다시 번역한 조각 수와 그중 다시 번역한 것을 채택한 수
    retries: u32,
    retries_used: u32,
    error: Option<String>,
}

impl SectionRun {
    fn count_call(&mut self, call: &crate::tool_loop::SingleCall) {
        self.rounds += 1;
        self.input_tokens += call.input_tokens;
        self.output_tokens += call.output_tokens;
        if call.stop_reason == "max_tokens" {
            self.max_tokens_stops += 1;
        }
    }
}

pub async fn run(
    client: &Arc<ClaudeClient>,
    pool: &PgPool,
    doc_id: Uuid,
    deadline: Option<Instant>,
) -> Result<JobOutcome, String> {
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
    let targets = match Targets::from_env() {
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
    let furniture: BTreeMap<i32, Arc<BTreeSet<String>>> =
        load_page_furniture(pool, doc_id).await?.into_iter().map(|(p, f)| (p, Arc::new(f))).collect();
    for (p, f) in &furniture {
        info!(%doc_id, product_no = p, lines = ?f, "페이지 머리말·꼬리말");
    }
    let jobs: Vec<SectionJob> = rows
        .into_iter()
        .filter(|r| targets.contains(r.0, r.2))
        .map(|(product_no, product_name, section_no, title, text)| SectionJob {
            product_no,
            product_name,
            section_no,
            title: title.unwrap_or_default(),
            text: text.unwrap_or_default(),
            furniture: furniture.get(&product_no).cloned().unwrap_or_default(),
        })
        .collect();
    info!(%doc_id, sections = jobs.len(), concurrency, "번역 시작");

    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut set = JoinSet::new();
    for job in jobs {
        let (client, pool, semaphore) = (client.clone(), pool.clone(), semaphore.clone());
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("세마포어가 닫혔습니다");
            translate_section(&client, &pool, doc_id, job, deadline).await
        });
    }

    let mut outcome = JobOutcome { ok: true, error: None, rounds: 0, input_tokens: 0, output_tokens: 0 };
    let mut warnings = Vec::new();
    let (mut retries, mut retries_used) = (0, 0);
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
        retries += r.retries;
        retries_used += r.retries_used;
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

    if retries > 0 {
        warnings.push(format!("사후 검사 경고로 다시 번역한 조각 {retries}개(다시 번역한 것 채택 {retries_used}개)"));
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

async fn translate_section(
    client: &ClaudeClient,
    pool: &PgPool,
    doc_id: Uuid,
    job: SectionJob,
    deadline: Option<Instant>,
) -> SectionRun {
    let mut run = SectionRun {
        product_no: job.product_no,
        section_no: job.section_no,
        rounds: 0,
        input_tokens: 0,
        output_tokens: 0,
        max_tokens_stops: 0,
        retries: 0,
        retries_used: 0,
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

    let title_ko = section_title_ko(job.section_no);
    let mut chunks = split_chunks(&job.text, TRANSLATE_CHUNK_CHARS);
    if chunks.is_empty() {
        chunks.push(""); // 본문이 빈 섹션도 제목만 저장한다
    }
    let total = chunks.len();
    for (i, chunk) in chunks.iter().copied().enumerate() {
        if past_deadline(deadline) {
            info!(%doc_id, product_no = job.product_no, section_no = job.section_no, chunk_no = i, "마감이 지나 새 조각을 시작하지 않음");
            run.error = Some(TIMEOUT_ERROR.into());
            break;
        }
        // 제목은 코드가 title_ko로 넣으므로 조각 0의 제목 줄은 번역하지 않는다
        let source = if i == 0 { strip_title_line(chunk, &job.title) } else { chunk };
        // 페이지 머리말·꼬리말은 번역하지 않는다(페이지 경계에서 지우거나 문장을 중복하는 일이 있었다)
        let source = strip_furniture(source, &job.furniture);
        let source = source.as_str();
        let text_ko = if source.trim().is_empty() {
            String::new()
        } else {
            info!(%doc_id, product_no = job.product_no, section_no = job.section_no, chunk_no = i, "번역 조각 시작");
            let text = match translate_chunk(client, &job, source, i, total).await {
                Ok((text, call)) => {
                    run.count_call(&call);
                    text
                }
                Err(e) => {
                    warn!(%doc_id, product_no = job.product_no, section_no = job.section_no, chunk_no = i, "번역 실패: {e}");
                    run.error = Some(format!("조각 {i}: {e}"));
                    break;
                }
            };
            // 사후 검사 경고(한자 혼입, 보존 토큰 빠짐 등)가 나면 한 번만 다시 번역한다. 새 조각과 같은 마감 규칙
            let warnings = post_check(source, &text, job.product_name.as_deref());
            if warnings.is_empty() || past_deadline(deadline) {
                text
            } else {
                run.retries += 1;
                info!(%doc_id, product_no = job.product_no, section_no = job.section_no, chunk_no = i, ?warnings, "사후 검사 경고로 조각을 다시 번역");
                match translate_chunk(client, &job, source, i, total).await {
                    Ok((retry, call)) => {
                        run.count_call(&call);
                        let retry_warnings = post_check(source, &retry, job.product_name.as_deref());
                        let (text, left, used) = pick_translation((text, warnings), (retry, retry_warnings));
                        if used {
                            run.retries_used += 1;
                        }
                        info!(%doc_id, product_no = job.product_no, section_no = job.section_no, chunk_no = i, used, ?left, "다시 번역 결과");
                        text
                    }
                    // 다시 번역이 실패하면 처음 번역을 쓴다(경고는 사후 검사에 그대로 남는다)
                    Err(e) => {
                        warn!(%doc_id, product_no = job.product_no, section_no = job.section_no, chunk_no = i, "다시 번역 실패: {e}");
                        text
                    }
                }
            }
        };
        // 마감 뒤에 끝난 조각은 저장하지 않는다(다음 실행의 결과에 섞이지 않게)
        if past_deadline(deadline) {
            info!(%doc_id, product_no = job.product_no, section_no = job.section_no, chunk_no = i, "마감이 지나 조각을 저장하지 않음");
            run.error = Some(TIMEOUT_ERROR.into());
            break;
        }
        let saved = TranslationChunk {
            doc_id,
            product_no: job.product_no,
            section_no: job.section_no,
            chunk_no: i as i32,
            title_ko: if i == 0 { title_ko.as_deref() } else { None },
            text_ko: &text_ko,
        };
        if let Err(e) = save_chunk(pool, &saved).await {
            run.error = Some(format!("조각 {i} 저장 실패: {e}"));
            break;
        }
    }

    if run.error.is_some() {
        // 일부 조각만 남으면 성공 판정에서 빠진 섹션으로 잡히지 않으므로 섹션 번역을 지운다
        let _ = sqlx::query("DELETE FROM msds_translations WHERE doc_id = $1 AND product_no = $2 AND section_no = $3")
            .bind(doc_id)
            .bind(job.product_no)
            .bind(job.section_no)
            .execute(pool)
            .await;
    } else {
        info!(
            %doc_id,
            product_no = job.product_no,
            section_no = job.section_no,
            chunks = total,
            calls = run.rounds,
            input_tokens = run.input_tokens,
            output_tokens = run.output_tokens,
            "섹션 번역 끝"
        );
    }
    run
}

/// 조각 하나를 툴 없이 한 번 호출해 번역한다. 빈 응답은 오류로 본다.
async fn translate_chunk(
    client: &ClaudeClient,
    job: &SectionJob,
    source: &str,
    index: usize,
    total: usize,
) -> Result<(String, crate::tool_loop::SingleCall), String> {
    let user = format!(
        "제품명: {}\n섹션: {} (원문 제목: {})\n조각: {}/{}\n\n<원문>\n{}\n</원문>",
        job.product_name.as_deref().unwrap_or("없음"),
        section_title_ko(job.section_no).unwrap_or_else(|| job.section_no.to_string()),
        job.title,
        index + 1,
        total,
        source.trim_end(),
    );
    let call = call_once(client, TRANSLATE_SYSTEM, user, TRANSLATE_MAX_TOKENS)
        .await
        .map_err(|e| format!("LLM 호출 실패: {e}"))?;
    let text = call.text.trim().to_string();
    if text.is_empty() {
        return Err(format!("빈 응답(stop_reason={})", call.stop_reason));
    }
    Ok((text, call))
}

pub struct SavedCheck {
    /// 원문에 있는데 번역이 저장되지 않은 섹션 ("제품 1 3번")
    pub missing: Vec<String>,
    /// 사후 검사 경고
    pub warnings: Vec<String>,
    /// 번역된 섹션마다 (제품, 섹션, 번역문 라틴 문자 비율). 경고는 아니고 출력용
    pub latin: Vec<(i32, i32, f64)>,
    /// 제품별 페이지 머리말·꼬리말(정규화한 줄, 출력용)
    pub furniture: BTreeMap<i32, BTreeSet<String>>,
}

/// 대상 섹션마다 번역 저장 여부와 보존 토큰을 확인한다 (agent-test 출력에도 쓴다)
pub async fn check_saved(pool: &PgPool, doc_id: Uuid, targets: &Targets) -> Result<SavedCheck, String> {
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

    let furniture = load_page_furniture(pool, doc_id).await?;
    let mut out = SavedCheck { missing: Vec::new(), warnings: Vec::new(), latin: Vec::new(), furniture: BTreeMap::new() };
    for (p, name, s, original, translated) in rows {
        if !targets.contains(p, s) {
            continue;
        }
        let Some(translated) = translated else {
            out.missing.push(format!("제품 {p} {s}번"));
            continue;
        };
        // 번역 입력과 같게 머리말·꼬리말을 뺀 원문으로 본다(꼬리말에만 있는 제품명은 확인하지 않는다)
        let original = strip_furniture(original.as_deref().unwrap_or(""), furniture.get(&p).unwrap_or(&BTreeSet::new()));
        for w in post_check(&original, &translated, name.as_deref()) {
            out.warnings.push(format!("제품 {p} {s}번 {w}"));
        }
        out.latin.push((p, s, latin_ratio(&translated)));
    }
    out.furniture = furniture;
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
    fn products_env() {
        assert_eq!(parse_products(None).unwrap(), None);
        assert_eq!(parse_products(Some(" ")).unwrap(), None);
        assert_eq!(parse_products(Some("1")).unwrap(), Some(BTreeSet::from([1])));
        assert_eq!(parse_products(Some("1-2,5")).unwrap(), Some(BTreeSet::from([1, 2, 5])));
        for bad in ["0", "x", "2-1"] {
            assert!(parse_products(Some(bad)).unwrap_err().contains("MSDS_TRANSLATE_PRODUCTS 형식 오류"), "{bad}");
        }

        let all = Targets { sections: BTreeSet::from([11]), products: None };
        assert!(all.contains(2, 11) && !all.contains(2, 10));
        let one = Targets { sections: BTreeSet::from([11]), products: Some(BTreeSet::from([1])) };
        assert!(one.contains(1, 11) && !one.contains(2, 11));
    }

    #[test]
    fn pair_format_detected() {
        let original = "Result: No effects\nSpecies: Rabbit\nRoute: Oral\nA → B";
        let pairs = "Result: No effects → 결과: 영향 없음\nSpecies: Rabbit → 시험 종: 토끼\nRoute: Oral → 경로: 경구\nA → B";
        assert_eq!(added_arrow_lines(original, pairs), 3);
        // 원문에 있던 '→' 줄은 세지 않는다
        assert_eq!(added_arrow_lines(original, "결과: 영향 없음\n시험 종: 토끼\n경로: 경구\nA → B"), 0);
        assert!(post_check(original, pairs, None).iter().any(|w| w.contains("쌍 형식 의심('→' 줄 3개)")));
        // 한 줄이라도 늘면 경고
        assert!(post_check("Route: Oral", "Route: Oral → 경로: 경구", None)[0].contains("'→' 줄 1개"));
        assert!(post_check("A → B", "가 → 나", None).is_empty());
    }

    #[test]
    fn cjk_detected() {
        // "운輸부", "経피" (한자 輸, 経), 가나 テ
        let translated = "미국 운\u{8F38}부\n경구 및 \u{7D4C}피 독성\n\u{30C6}스트 \u{8F38}";
        assert_eq!(foreign_cjk("DOT", translated), ['\u{8F38}', '\u{7D4C}', '\u{30C6}']);
        // 원문에 있던 글자는 제외
        assert_eq!(foreign_cjk("\u{8F38}", translated), ['\u{7D4C}', '\u{30C6}']);
        assert!(foreign_cjk("", "한글과 Latin, 화학식 H2O").is_empty());
        // 최대 10개
        let many: String = ('\u{4E00}'..='\u{4E20}').collect();
        assert_eq!(foreign_cjk("", &many).len(), 10);
        assert!(post_check("x", "운\u{8F38}부", None)[0].starts_with("원문에 없는 한자·가나: \u{8F38}"));
    }

    #[test]
    fn latin_ratio_of_translation() {
        assert_eq!(latin_ratio(""), 0.0);
        assert_eq!(latin_ratio("  \n"), 0.0);
        assert_eq!(latin_ratio("abcd"), 1.0);
        // 공백 제외 8글자 중 라틴 4글자 (숫자·기호는 분모에만)
        assert_eq!(latin_ratio("건강성 NFPA 1"), 0.5);
        assert_eq!(latin_ratio("Café 카"), 0.8);
    }

    #[test]
    fn edge_line_normalized() {
        assert_eq!(normalize_edge_line("  Page 2 of 6 "), "Page # of #");
        assert_eq!(normalize_edge_line("SDS_GB-AM – 6N – 400001015903  13 / 28"), "SDS_GB-AM – #N – # # / #");
        assert_eq!(normalize_edge_line("21-OCT-05  24-JAN-14"), "#-OCT-# #-JAN-#");
        assert_eq!(normalize_edge_line(" \t "), "");
    }

    #[test]
    fn furniture_found_on_half_of_pages() {
        let page = |n: usize, body: &str| format!("ACME SDS\nPage {n} of 4\n{body}\n\nRev 2021-09-27\n");
        let pages: Vec<String> = vec![
            page(1, "Section 1 Identification\nProduct name: X"),
            page(2, "Section 2 Hazards\nH315"),
            page(3, "Section 3 Composition\nH315"),
            page(4, "Section 9 Properties"),
        ];
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        // 4페이지 중 3페이지 이상(절반 2, 최소 3). 본문 'H315'는 2페이지라 아님
        assert_eq!(
            find_page_furniture(&refs),
            BTreeSet::from(["ACME SDS".to_string(), "Page # of #".into(), "Rev #-#-#".into()])
        );
        // 3페이지 미만이면 찾지 않는다
        assert!(find_page_furniture(&refs[..2]).is_empty());
    }

    #[test]
    fn furniture_stripped_from_chunk() {
        let furniture = BTreeSet::from(["ACME SDS".to_string(), "Page # of #".into()]);
        let text = "Extinguishing media: foam and\nACME SDS\nPage 2 of 6\nwater spray.\n";
        assert_eq!(strip_furniture(text, &furniture), "Extinguishing media: foam and\nwater spray.\n");
        // 줄 전체가 같을 때만 뺀다
        assert_eq!(strip_furniture("See ACME SDS page 3\n", &furniture), "See ACME SDS page 3\n");
        assert_eq!(strip_furniture(text, &BTreeSet::new()), text);
    }

    #[test]
    fn page_ranges_per_product() {
        assert_eq!(product_page_ranges(&[(1, 1)], 6), [(1, 1..=6)]);
        // ARALDITE: 제품 2가 29쪽에서 시작 → 경계 페이지는 양쪽에
        assert_eq!(product_page_ranges(&[(1, 1), (2, 29)], 51), [(1, 1..=29), (2, 29..=51)]);
        assert!(product_page_ranges(&[], 5).is_empty());
    }

    #[test]
    fn retry_kept_only_when_fewer_warnings() {
        let w = |n: usize| (0..n).map(|i| format!("경고 {i}")).collect::<Vec<_>>();
        let first = || ("처음".to_string(), w(1));
        // 다시 번역한 것의 경고가 적으면 채택
        assert_eq!(pick_translation(first(), ("다시".into(), w(0))), ("다시".into(), w(0), true));
        // 같거나 많으면 처음 것
        assert_eq!(pick_translation(first(), ("다시".into(), w(1))), ("처음".into(), w(1), false));
        assert_eq!(pick_translation(first(), ("다시".into(), w(2))), ("처음".into(), w(1), false));
        // 한자 경고가 사라지고 다른 경고 하나가 남아도 수가 같으면 처음 것
        let original = ("가".to_string(), vec!["원문에 없는 한자·가나: 吸".to_string()]);
        let retry = ("나".to_string(), vec!["번역문에 없음: H302".to_string()]);
        assert!(!pick_translation(original, retry).2);
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
    fn chunks_cut_at_last_newline() {
        let text = "aaaa
bb
cccc
dd";
        let chunks = split_chunks(text, 8);
        assert_eq!(chunks, ["aaaa
bb
", "cccc
dd"]);
        assert_eq!(chunks.concat(), text);
        // 남은 글이 max 이하면 한 조각
        assert_eq!(split_chunks("abc
", 4), ["abc
"]);
        assert!(split_chunks("", 10).is_empty());
    }

    #[test]
    fn chunks_without_newline_cut_at_max() {
        // 한글도 글자 단위로 자른다
        assert_eq!(split_chunks("가나다라마바", 4), ["가나다라", "마바"]);
        // 맨 앞 줄바꿈만 있으면 쓰지 않고 max에서 자른다
        assert_eq!(split_chunks("
12345", 3), ["
12", "345"]);
    }

    #[test]
    fn chunks_real_size() {
        let line = "x".repeat(99) + "
"; // 100자 줄
        let text = line.repeat(150); // 15,000자
        let chunks = split_chunks(&text, 6000);
        assert_eq!(chunks.iter().map(|c| c.chars().count()).collect::<Vec<_>>(), [6000, 6000, 3000]);
        assert!(chunks.iter().all(|c| c.ends_with('\n')));
        // 줄 경계가 6000자에 걸리지 않으면 그 앞 줄바꿈에서 자른다
        let text = "y".repeat(5990) + "
" + &"z".repeat(20);
        assert_eq!(split_chunks(&text, 6000).iter().map(|c| c.chars().count()).collect::<Vec<_>>(), [5991, 20]);
    }

    #[test]
    fn title_line_removed() {
        let text = "Section 2 : Hazard Identification
HMIS : 1 1 0
";
        assert_eq!(strip_title_line(text, "Section 2 : Hazard Identification"), "HMIS : 1 1 0
");
        assert_eq!(strip_title_line("
2.  HAZARDS   identification
body", "2. HAZARDS IDENTIFICATION"), "body");
        // 제목 줄에 다른 내용이 붙어 있거나 제목이 다르면 그대로
        let merged = "2. HAZARDS IDENTIFICATION Danger
body";
        assert_eq!(strip_title_line(merged, "2. HAZARDS IDENTIFICATION"), merged);
        assert_eq!(strip_title_line("body", ""), "body");
        assert_eq!(strip_title_line("Title only", "Title only"), "");
    }

    #[test]
    fn standard_titles() {
        assert_eq!(section_title_ko(2).unwrap(), "2. 유해성·위험성");
        assert_eq!(section_title_ko(16).unwrap(), "16. 그 밖의 참고사항");
        assert!(section_title_ko(0).is_none() && section_title_ko(17).is_none());
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
