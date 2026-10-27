//! cargo run -- measure <cost|report> : Step 11 실측용 (DB만 읽고 LLM은 부르지 않는다)
//!
//! - measure cost --since <RFC3339> : 그 시각 이후 총괄 실행(pipeline_run_id 있는 행)의 추정 비용 합계
//! - measure report --since <RFC3339> --requests <requests.json> --out <폴더> : results.json, summary.md
//!
//! 정확도 대조는 정답이 없어 원문(msds_pages)과 비교한 후보만 낸다. 원문은 CAS 대조 규칙(공백 제거·대시 통일)으로
//! 정규화해서 찾고(줄이 나뉜 "64742-54-" / "7", 공백이 낀 "64742 - 54 - 7"), 위치는 정규화 전 페이지·줄로 돌려준다.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::actors::translate::{check_saved, product_page_ranges, Targets, CAS_RE, CODE_RE};
use crate::api_view::{self, ComponentRow};
use crate::tools::cas::{check_cas, clean_cas, CasCheck};

/// 원문 위치 (page_no, 페이지 안의 줄 번호 1부터, str::lines 기준)
pub type Pos = (i32, usize);

/// 정규화한 원문과 글자마다의 원래 위치
pub struct NormText {
    pub text: String,
    /// text의 글자마다 (바이트 시작, 원래 위치, 바로 앞의 공백·줄바꿈을 지우고 붙였는지)
    origins: Vec<(usize, Pos, bool)>,
}

impl NormText {
    fn index(&self, byte: usize) -> Option<usize> {
        self.origins.partition_point(|(b, ..)| *b <= byte).checked_sub(1)
    }

    fn origin(&self, byte: usize) -> Option<Pos> {
        self.index(byte).map(|i| self.origins[i].1)
    }

    /// 이 글자가 원문에서 앞 글자와 공백 없이 붙어 있었는지
    fn attached(&self, byte: usize) -> bool {
        self.index(byte).is_some_and(|i| !self.origins[i].2)
    }
}

fn is_dash(c: char) -> bool {
    c == '-' || clean_cas(&c.to_string()) == "-"
}

/// 대시는 '-'로, 대시 앞뒤의 공백·줄바꿈은 지우고, 나머지 공백 묶음은 공백 하나로 바꾼다.
/// 줄이 대시에서 나뉜 CAS("64742-54-" + 다음 줄 "7")와 공백이 낀 CAS("64742 - 54 - 7")가 이어진다
pub fn normalize(lines: &[(Pos, &str)]) -> NormText {
    let mut raw: Vec<(char, Pos)> = Vec::new();
    for (i, (pos, line)) in lines.iter().enumerate() {
        if i > 0 {
            raw.push(('\n', *pos));
        }
        raw.extend(line.chars().map(|c| (c, *pos)));
    }
    let mut out = NormText { text: String::new(), origins: Vec::new() };
    let mut joined = false;
    let mut i = 0;
    while i < raw.len() {
        let (c, pos) = raw[i];
        if c.is_whitespace() {
            let start = i;
            while i < raw.len() && raw[i].0.is_whitespace() {
                i += 1;
            }
            let before_dash = out.text.ends_with('-');
            let after_dash = raw.get(i).is_some_and(|(n, _)| is_dash(*n));
            if before_dash || after_dash {
                joined = !out.text.is_empty();
            } else if !out.text.is_empty() && i < raw.len() {
                out.origins.push((out.text.len(), raw[start].1, false));
                out.text.push(' ');
            }
            continue;
        }
        out.origins.push((out.text.len(), pos, joined));
        out.text.push(if is_dash(c) { '-' } else { c });
        joined = false;
        i += 1;
    }
    out
}

/// 페이지 범위 [from, to)의 줄 (to가 None이면 끝까지). pages는 page_no 순서
pub fn lines_between<'a>(pages: &'a [(i32, String)], from: Pos, to: Option<Pos>) -> Vec<(Pos, &'a str)> {
    pages
        .iter()
        .flat_map(|(p, text)| text.lines().enumerate().map(move |(i, l)| ((*p, i + 1), l)))
        .filter(|(pos, _)| *pos >= from && to.is_none_or(|t| *pos < t))
        .collect()
}

fn digit_at(text: &str, byte: Option<usize>) -> bool {
    byte.and_then(|b| text[b..].chars().next()).is_some_and(|c| c.is_ascii_digit())
}

/// 저장된 CAS 하나를 정규화한 원문에서 찾는다(앞뒤가 숫자면 다른 번호의 일부로 보고 건너뜀). 첫 위치를 돌려준다
pub fn find_cas(norm: &NormText, cas: &str) -> Option<Pos> {
    let needle = clean_cas(cas);
    if needle.is_empty() {
        return None;
    }
    norm.text.match_indices(&needle).find_map(|(at, m)| {
        let before = norm.text[..at].char_indices().last().map(|(b, _)| b);
        let after = at + m.len();
        (!digit_at(&norm.text, before) && !digit_at(&norm.text, Some(after))).then(|| norm.origin(at)).flatten()
    })
}

/// 원문에서 바로 앞에 "숫자-"가 공백 없이 붙어 있으면 더 긴 번호의 뒷부분이다(EU 색인번호 603-073-00-2의 073-00-2 등).
/// 정규화가 지운 공백은 붙은 것으로 보지 않는다("bis(4,1- 1675-54-3"의 1675-54-3은 CAS)
fn part_of_longer_number(norm: &NormText, start: usize) -> bool {
    let mut before = norm.text[..start].char_indices().rev();
    let (Some((dash, '-')), Some((_, digit))) = (before.next(), before.next()) else { return false };
    digit.is_ascii_digit() && norm.attached(dash) && norm.attached(start)
}

/// 원문의 CAS 모양 문자열(중복 제거, 첫 위치). 더 긴 번호의 뒷부분은 뺀다
pub fn cas_in_text(norm: &NormText) -> Vec<(String, Pos)> {
    let mut seen = BTreeSet::new();
    CAS_RE
        .find_iter(&norm.text)
        .filter(|m| !part_of_longer_number(norm, m.start()))
        .filter(|m| seen.insert(m.as_str().to_string()))
        .filter_map(|m| Some((m.as_str().to_string(), norm.origin(m.start())?)))
        .collect()
}

/// 원문의 H코드(H200~H499, 번역 사후 검사와 같은 패턴: 묶음은 각각, H360Fd는 H360). 중복 제거, 첫 위치
pub fn h_codes_in(norm: &NormText) -> Vec<(String, Pos)> {
    let mut seen = BTreeSet::new();
    CODE_RE
        .find_iter(&norm.text)
        .filter(|m| m.as_str().starts_with('H') && seen.insert(m.as_str().to_string()))
        .filter_map(|m| Some((m.as_str().to_string(), norm.origin(m.start())?)))
        .collect()
}

/// 저장된 cas_no(여러 개면 쉼표·세미콜론)를 번호별로. "비공개"와 빈 값은 뺀다
pub fn split_stored_cas(cas_no: &str) -> Vec<String> {
    cas_no
        .split([',', ';'])
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != "비공개")
        .map(str::to_string)
        .collect()
}

/// H코드 차이: (원문에 있는데 저장 안 됨, 저장됐는데 원문 2번 섹션에 없음)
pub fn h_code_diff(in_text: &[(String, Pos)], stored: &BTreeSet<String>) -> (Vec<(String, Pos)>, Vec<String>) {
    let text: BTreeSet<&str> = in_text.iter().map(|(c, _)| c.as_str()).collect();
    let missing = in_text.iter().filter(|(c, _)| !stored.contains(c)).cloned().collect();
    let added = stored.iter().filter(|c| !text.contains(c.as_str())).cloned().collect();
    (missing, added)
}

/// 섹션 범위: 그 섹션의 시작부터 (문서 안 위치 순서로) 다음 섹션 시작 전까지. starts는 (product_no, section_no, 시작 위치)
pub fn section_span(starts: &[(i32, i32, Pos)], product_no: i32, section_no: i32) -> Option<(Pos, Option<Pos>)> {
    let mut sorted: Vec<&(i32, i32, Pos)> = starts.iter().collect();
    sorted.sort_by_key(|s| s.2);
    let i = sorted.iter().position(|s| s.0 == product_no && s.1 == section_no)?;
    Some((sorted[i].2, sorted.get(i + 1).map(|s| s.2)))
}

// ---- DB에서 모으기 ----

#[derive(Serialize)]
struct Candidate {
    kind: &'static str,
    product_no: i32,
    value: String,
    page: Option<i32>,
    line: Option<usize>,
    note: String,
}

fn candidate(kind: &'static str, product_no: i32, value: &str, pos: Option<Pos>, note: impl Into<String>) -> Candidate {
    Candidate { kind, product_no, value: value.to_string(), page: pos.map(|p| p.0), line: pos.map(|p| p.1), note: note.into() }
}

async fn total_cost(pool: &PgPool, since: DateTime<Utc>) -> Result<(f64, u64, u64), String> {
    let rows: Vec<(Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT model, sum(input_tokens)::bigint, sum(output_tokens)::bigint FROM msds_agent_runs \
         WHERE pipeline_run_id IS NOT NULL AND started_at >= $1 GROUP BY model",
    )
    .bind(since)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("실행 기록 조회 실패: {e}"))?;
    let mut out = (0.0, 0, 0);
    for (model, i, o) in rows {
        let (i, o) = (i.unwrap_or(0) as u64, o.unwrap_or(0) as u64);
        out.0 += crate::cost::estimate(model.as_deref().unwrap_or(""), i, o).unwrap_or(0.0);
        out.1 += i;
        out.2 += o;
    }
    Ok(out)
}

fn arg(args: &[String], name: &str) -> Result<String, String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone()).ok_or_else(|| format!("{name} 값이 없습니다"))
}

fn parse_time(s: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s.trim()).map(|t| t.with_timezone(&Utc)).map_err(|e| format!("시각 형식 오류({s}): {e}"))
}

pub async fn run(pool: PgPool, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("cost") => {
            let since = parse_time(&arg(args, "--since")?)?;
            let (usd, i, o) = total_cost(&pool, since).await?;
            println!("{usd:.4} {i} {o}");
            Ok(())
        }
        Some("report") => {
            let since = parse_time(&arg(args, "--since")?)?;
            let requests = arg(args, "--requests")?;
            let out = arg(args, "--out")?;
            report(&pool, since, &requests, &out).await
        }
        _ => Err("사용법: cargo run -- measure cost --since <RFC3339> | measure report --since <RFC3339> --requests <파일> --out <폴더>".into()),
    }
}

#[derive(serde::Deserialize)]
struct RequestRow {
    doc_id: Uuid,
    requested_at: String,
}

async fn report(pool: &PgPool, since: DateTime<Utc>, requests_path: &str, out_dir: &str) -> Result<(), String> {
    let raw = std::fs::read_to_string(requests_path).map_err(|e| format!("{requests_path} 읽기 실패: {e}"))?;
    let requests: Vec<RequestRow> = serde_json::from_str(&raw).map_err(|e| format!("{requests_path} 형식 오류: {e}"))?;
    let targets = Targets::from_env()?;

    let mut docs = Vec::new();
    for r in &requests {
        docs.push(doc_result(pool, r.doc_id, parse_time(&r.requested_at)?, since, &targets).await?);
    }
    let (usd, i, o) = total_cost(pool, since).await?;
    let results = json!({
        "since": since,
        "total": { "cost_usd": usd, "input_tokens": i, "output_tokens": o },
        "documents": docs,
    });
    std::fs::create_dir_all(out_dir).map_err(|e| format!("{out_dir} 만들기 실패: {e}"))?;
    let json_path = format!("{out_dir}/results.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(&results).unwrap_or_default())
        .map_err(|e| format!("{json_path} 쓰기 실패: {e}"))?;
    let md_path = format!("{out_dir}/summary.md");
    std::fs::write(&md_path, summary_md(&results)).map_err(|e| format!("{md_path} 쓰기 실패: {e}"))?;
    println!("{json_path}\n{md_path}");
    Ok(())
}

type RunRow = (String, Option<String>, bool, Option<String>, Option<i32>, Option<i32>, Option<i32>, DateTime<Utc>, DateTime<Utc>);

async fn doc_result(
    pool: &PgPool,
    doc_id: Uuid,
    requested_at: DateTime<Utc>,
    since: DateTime<Utc>,
    targets: &Targets,
) -> Result<Value, String> {
    let db = |e: sqlx::Error| format!("{doc_id} 조회 실패: {e}");
    let (file_name, status, error, language): (Option<String>, String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT file_name, status, error, language FROM msds_documents WHERE id = $1")
            .bind(doc_id)
            .fetch_one(pool)
            .await
            .map_err(db)?;

    // 측정 시작 이후 가장 최근 총괄 실행
    let runs: Vec<RunRow> = sqlx::query_as(
        "SELECT agent, model, ok, error, rounds, input_tokens, output_tokens, started_at, finished_at \
         FROM msds_agent_runs WHERE doc_id = $1 AND pipeline_run_id = \
         (SELECT pipeline_run_id FROM msds_agent_runs WHERE doc_id = $1 AND pipeline_run_id IS NOT NULL \
          AND started_at >= $2 ORDER BY id DESC LIMIT 1) \
         ORDER BY array_position(ARRAY['extract', 'components', 'hazards', 'translate'], agent)",
    )
    .bind(doc_id)
    .bind(since)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let mut agents = Vec::new();
    let (mut cost, mut cost_known, mut calls, mut tin, mut tout) = (0.0, true, 0, 0i64, 0i64);
    for (agent, model, ok, err, rounds, i, o, s, f) in &runs {
        let (i, o) = (i.unwrap_or(0), o.unwrap_or(0));
        let c = crate::cost::estimate(model.as_deref().unwrap_or(""), i as u64, o as u64);
        match c {
            Some(c) => cost += c,
            None => cost_known = false,
        }
        calls += rounds.unwrap_or(0);
        tin += i as i64;
        tout += o as i64;
        agents.push(json!({
            "agent": agent, "ok": ok, "error": err, "model": model,
            "seconds": (*f - *s).num_milliseconds() as f64 / 1000.0,
            "rounds": rounds, "input_tokens": i, "output_tokens": o, "cost_usd": c,
        }));
    }
    let start = runs.iter().map(|r| r.7).min();
    let end = runs.iter().map(|r| r.8).max();
    let extract_start = runs.iter().find(|r| r.0 == "extract").map(|r| r.7);

    // 결과 행
    let pages: Vec<(i32, String)> = sqlx::query("SELECT page_no, text FROM msds_pages WHERE doc_id = $1 ORDER BY page_no")
        .bind(doc_id)
        .fetch_all(pool)
        .await
        .map_err(db)?
        .iter()
        .map(|r| (r.get("page_no"), r.get::<Option<String>, _>("text").unwrap_or_default()))
        .collect();
    let section_rows: Vec<(i32, i32, Option<i32>, Option<i32>, Option<String>)> = sqlx::query_as(
        "SELECT product_no, section_no, page_no, line_no, product_name FROM msds_sections WHERE doc_id = $1",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let components: Vec<ComponentRow> = sqlx::query(
        "SELECT product_no, product_name, component, cas_no, percent, cas_valid FROM msds_components \
         WHERE doc_id = $1 ORDER BY product_no, id",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(db)?
    .iter()
    .map(|r| ComponentRow {
        product_no: r.get("product_no"),
        product_name: r.get("product_name"),
        component: r.get("component"),
        cas_no: r.get("cas_no"),
        percent: r.get("percent"),
        cas_valid: r.get("cas_valid"),
    })
    .collect();
    let hazards: Vec<(i32, Option<String>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT product_no, h_code, ghs_code, source, note FROM msds_hazards WHERE doc_id = $1 ORDER BY product_no, id",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let (translated,): (i64,) = sqlx::query_as(
        "SELECT count(DISTINCT (product_no, section_no)) FROM msds_translations WHERE doc_id = $1",
    )
    .bind(doc_id)
    .fetch_one(pool)
    .await
    .map_err(db)?;
    let translation_warnings = if language.as_deref() == Some("ko") || translated == 0 {
        0
    } else {
        check_saved(pool, doc_id, targets).await?.warnings.len()
    };

    let mut names: BTreeMap<i32, Option<String>> = BTreeMap::new();
    let mut starts: Vec<(i32, i32, Pos)> = Vec::new();
    for (p, s, page, line, name) in &section_rows {
        let entry = names.entry(*p).or_default();
        if entry.is_none() {
            *entry = name.clone();
        }
        if let (Some(page), Some(line)) = (page, line) {
            starts.push((*p, *s, (*page, *line as usize)));
        }
    }
    let product_starts: Vec<(i32, i32)> = names
        .keys()
        .filter_map(|p| starts.iter().filter(|s| s.0 == *p).map(|s| s.2 .0).min().map(|m| (*p, m)))
        .collect();
    let last_page = pages.last().map(|p| p.0).unwrap_or(0);
    let ranges: BTreeMap<i32, std::ops::RangeInclusive<i32>> =
        product_page_ranges(&product_starts, last_page).into_iter().collect();

    let product_list: Vec<(i32, Option<String>)> = names.clone().into_iter().collect();
    let legacy = api_view::legacy_status(&api_view::legacy_products(&product_list, &components));

    let mut products = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    for (p, name) in &names {
        let comps: Vec<&ComponentRow> = components.iter().filter(|c| c.product_no == *p).collect();
        let hz: Vec<_> = hazards.iter().filter(|h| h.0 == *p).collect();
        let ghs: BTreeSet<&str> = hz.iter().filter_map(|h| h.2.as_deref()).collect();
        let h_all: BTreeSet<&str> = hz.iter().filter_map(|h| h.1.as_deref()).collect();
        let h_document: BTreeSet<String> =
            hz.iter().filter(|h| h.3.as_deref() == Some("document")).filter_map(|h| h.1.clone()).collect();
        let h_classification: BTreeSet<&str> =
            hz.iter().filter(|h| h.3.as_deref() == Some("classification")).filter_map(|h| h.1.as_deref()).collect();
        let no_h_codes = h_all.is_empty() && hz.iter().any(|h| h.4.is_some());

        // 1) 저장된 CAS가 제품 페이지 범위 원문에 있는가
        let range = ranges.get(p).cloned().unwrap_or(1..=last_page);
        let product_lines = lines_between(&pages, (*range.start(), 0), Some((*range.end() + 1, 0)));
        let product_norm = normalize(&product_lines);
        let mut stored_cas = BTreeSet::new();
        for c in &comps {
            for cas in split_stored_cas(c.cas_no.as_deref().unwrap_or("")) {
                stored_cas.insert(clean_cas(&cas));
                if find_cas(&product_norm, &cas).is_none() {
                    candidates.push(candidate(
                        "CAS 원문에 없음",
                        *p,
                        &cas,
                        None,
                        format!("성분 {}", c.component.as_deref().unwrap_or("")),
                    ));
                }
            }
        }
        // 2) 성분 섹션(표준 3번)의 CAS 모양 중 저장 안 된 것
        if let Some((from, to)) = section_span(&starts, *p, 3) {
            let norm = normalize(&lines_between(&pages, from, to));
            for (cas, pos) in cas_in_text(&norm) {
                if !stored_cas.contains(&cas) {
                    let check = match check_cas(Some(&cas)) {
                        CasCheck::Valid(_) => "체크섬 맞음",
                        _ => "체크섬 틀림",
                    };
                    candidates.push(candidate("CAS 빠뜨림", *p, &cas, Some(pos), check));
                }
            }
        }
        // 3) 유해성 섹션(표준 2번) H코드와 저장된 document H코드 차이
        if let Some((from, to)) = section_span(&starts, *p, 2) {
            let norm = normalize(&lines_between(&pages, from, to));
            let (missing, added) = h_code_diff(&h_codes_in(&norm), &h_document);
            for (code, pos) in missing {
                let note = if h_classification.contains(code.as_str()) { "분류에서 변환한 코드로는 저장됨" } else { "" };
                candidates.push(candidate("H코드 빠짐", *p, &code, Some(pos), note));
            }
            for code in added {
                let elsewhere = h_codes_in(&product_norm).into_iter().find(|(c, _)| *c == code).map(|(_, pos)| pos);
                let note = if elsewhere.is_some() { "2번 섹션 밖에서만 보임" } else { "제품 원문에 없음" };
                candidates.push(candidate("H코드 추가", *p, &code, elsewhere, note));
            }
        }

        products.push(json!({
            "product_no": p,
            "product_name": name,
            "components": comps.len(),
            "cas_invalid": comps.iter().filter(|c| c.cas_valid == Some(false)).count(),
            "confidential": comps.iter().filter(|c| c.cas_no.as_deref() == Some("비공개")).count(),
            "ghs_codes": ghs,
            "h_codes": h_all.len(),
            "h_codes_classification": h_classification,
            "no_h_codes": no_h_codes,
        }));
    }

    Ok(json!({
        "doc_id": doc_id,
        "file_name": file_name,
        "language": language,
        "status": status,
        "error": error,
        "requested_at": requested_at,
        "elapsed_seconds": start.zip(end).map(|(s, e)| (e - s).num_milliseconds() as f64 / 1000.0),
        "queue_wait_seconds": extract_start.map(|s| (s - requested_at).num_milliseconds() as f64 / 1000.0),
        "agents": agents,
        "totals": { "rounds": calls, "input_tokens": tin, "output_tokens": tout,
                    "cost_usd": cost, "cost_complete": cost_known },
        "products": products,
        "translated_sections": translated,
        "translation_warnings": translation_warnings,
        "legacy_status": legacy,
        "candidates": candidates,
    }))
}

// ---- summary.md ----

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn summary_md(results: &Value) -> String {
    let docs = results["documents"].as_array().cloned().unwrap_or_default();
    let mut md = String::new();
    let _ = writeln!(md, "# Step 11 실측 결과\n\n측정 시작: {}\n", results["since"].as_str().unwrap_or(""));
    let _ = writeln!(
        md,
        "| 문서 | 상태 | 전체(s) | 대기(s) | 제품 | 성분 | CAS 오류 | 비공개 | GHS | H코드 | no_h | 번역 섹션 | 번역 경고 | legacy | 호출 | 입력/출력 토큰 | 비용($) | 대조 후보 |"
    );
    let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let sum_products = |d: &Value, key: &str| -> u64 {
        d["products"].as_array().map(|ps| ps.iter().map(|p| p[key].as_u64().unwrap_or(0)).sum()).unwrap_or(0)
    };
    for d in &docs {
        let ps = d["products"].as_array().cloned().unwrap_or_default();
        let ghs: BTreeSet<String> = ps
            .iter()
            .flat_map(|p| p["ghs_codes"].as_array().cloned().unwrap_or_default())
            .filter_map(|g| g.as_str().map(str::to_string))
            .collect();
        let no_h = ps.iter().filter(|p| p["no_h_codes"].as_bool() == Some(true)).count();
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {}/{} | {} | {} | {} | {} | {} / {} | {:.4} | {} |",
            d["file_name"].as_str().unwrap_or(""),
            d["status"].as_str().unwrap_or(""),
            d["elapsed_seconds"].as_f64().map(|s| format!("{s:.1}")).unwrap_or_else(|| "-".into()),
            d["queue_wait_seconds"].as_f64().map(|s| format!("{s:.1}")).unwrap_or_else(|| "-".into()),
            ps.len(),
            sum_products(d, "components"),
            sum_products(d, "cas_invalid"),
            sum_products(d, "confidential"),
            if ghs.is_empty() { "-".to_string() } else { ghs.into_iter().collect::<Vec<_>>().join(" ") },
            sum_products(d, "h_codes"),
            no_h,
            ps.len(),
            d["translated_sections"],
            d["translation_warnings"],
            d["legacy_status"],
            d["totals"]["rounds"],
            d["totals"]["input_tokens"],
            d["totals"]["output_tokens"],
            f(&d["totals"]["cost_usd"]),
            d["candidates"].as_array().map(|c| c.len()).unwrap_or(0),
        );
    }

    let n = docs.len().max(1) as f64;
    let count = |s: &str| docs.iter().filter(|d| d["status"] == s).count();
    let elapsed: Vec<f64> = docs.iter().filter_map(|d| d["elapsed_seconds"].as_f64()).collect();
    let waits: Vec<f64> = docs.iter().filter_map(|d| d["queue_wait_seconds"].as_f64()).collect();
    let avg = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
    let cost: f64 = docs.iter().map(|d| f(&d["totals"]["cost_usd"])).sum();
    let _ = writeln!(md, "\n## 전체\n");
    let _ = writeln!(
        md,
        "- 문서 {}건: done {}, partial {}, failed {} (성공률 {:.0}%, partial 포함 {:.0}%)",
        docs.len(),
        count("done"),
        count("partial"),
        count("failed"),
        count("done") as f64 / n * 100.0,
        (count("done") + count("partial")) as f64 / n * 100.0,
    );
    let _ = writeln!(
        md,
        "- 추정 비용 합계 ${cost:.4} (문서 평균 ${:.4}), 입력 토큰 {}, 출력 토큰 {}",
        cost / n,
        results["total"]["input_tokens"],
        results["total"]["output_tokens"]
    );
    let _ = writeln!(
        md,
        "- 문서 전체 시간 평균 {:.1}s (최대 {:.1}s), 대기열 대기 평균 {:.1}s (최대 {:.1}s)",
        avg(&elapsed),
        elapsed.iter().cloned().fold(0.0, f64::max),
        avg(&waits),
        waits.iter().cloned().fold(0.0, f64::max),
    );
    let _ = writeln!(md, "\n## 에이전트별\n\n| 에이전트 | 성공 | 평균 시간(s) | 호출 | 입력/출력 토큰 | 비용($) |\n|---|---|---|---|---|---|");
    for agent in ["extract", "components", "hazards", "translate"] {
        let rows: Vec<Value> =
            docs.iter().flat_map(|d| d["agents"].as_array().cloned().unwrap_or_default()).filter(|a| a["agent"] == agent).collect();
        let ok = rows.iter().filter(|a| a["ok"] == true).count();
        let secs: Vec<f64> = rows.iter().map(|a| f(&a["seconds"])).collect();
        let _ = writeln!(
            md,
            "| {agent} | {ok}/{} | {:.1} | {} | {} / {} | {:.4} |",
            rows.len(),
            avg(&secs),
            rows.iter().map(|a| a["rounds"].as_i64().unwrap_or(0)).sum::<i64>(),
            rows.iter().map(|a| a["input_tokens"].as_i64().unwrap_or(0)).sum::<i64>(),
            rows.iter().map(|a| a["output_tokens"].as_i64().unwrap_or(0)).sum::<i64>(),
            rows.iter().map(|a| f(&a["cost_usd"])).sum::<f64>(),
        );
    }

    let _ = writeln!(md, "\n## 실패·부분 완료 이유\n");
    for d in docs.iter().filter(|d| d["status"] != "done") {
        let _ = writeln!(
            md,
            "- {} ({}): {}",
            d["file_name"].as_str().unwrap_or(""),
            d["status"].as_str().unwrap_or(""),
            d["error"].as_str().unwrap_or("-").replace('\n', " / ")
        );
    }

    let _ = writeln!(md, "\n## 정확도 대조 후보 (원문 텍스트 기준, 고치지 않음)\n\n| 문서 | 제품 | 종류 | 값 | 위치 | 메모 |\n|---|---|---|---|---|---|");
    for d in &docs {
        for c in d["candidates"].as_array().cloned().unwrap_or_default() {
            let pos = match (c["page"].as_i64(), c["line"].as_i64()) {
                (Some(p), Some(l)) => format!("p.{p} 줄 {l}"),
                _ => "-".into(),
            };
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {} | {} |",
                d["file_name"].as_str().unwrap_or(""),
                c["product_no"],
                c["kind"].as_str().unwrap_or(""),
                c["value"].as_str().unwrap_or(""),
                pos,
                c["note"].as_str().unwrap_or(""),
            );
        }
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(lines: &[(i32, usize, &str)]) -> NormText {
        let v: Vec<(Pos, &str)> = lines.iter().map(|(p, l, t)| ((*p, *l), *t)).collect();
        normalize(&v)
    }

    #[test]
    fn cas_split_across_lines_is_found_at_original_position() {
        let n = norm(&[(2, 3, "Mineral oil 64742-54-"), (2, 4, "7 60-100%")]);
        assert_eq!(find_cas(&n, "64742-54-7"), Some((2, 3)));
        // 줄이 대시 앞에서 나뉜 경우도
        let n = norm(&[(1, 9, "Xylene 1330-20"), (2, 1, "-7")]);
        assert_eq!(find_cas(&n, "1330-20-7"), Some((1, 9)));
    }

    #[test]
    fn cas_with_spaces_and_dash_variants() {
        let n = norm(&[(1, 5, "Petroleum oil  64742 - 54 - 7   >99%"), (1, 6, "BHT 128\u{2013}37\u{2013}0 <1%")]);
        assert_eq!(find_cas(&n, "64742-54-7"), Some((1, 5)));
        assert_eq!(find_cas(&n, "128-37-0"), Some((1, 6)));
        // 저장값 쪽의 공백·대시도 같은 규칙으로
        assert_eq!(find_cas(&n, " 64742 \u{2011}54-7"), Some((1, 5)));
    }

    #[test]
    fn cas_boundaries() {
        let n = norm(&[(1, 1, "Water 7732-18-5 100"), (1, 2, "other 17732-18-55")]);
        assert_eq!(find_cas(&n, "7732-18-5"), Some((1, 1)));
        let n = norm(&[(1, 1, "17732-18-55")]);
        assert_eq!(find_cas(&n, "7732-18-5"), None);
        // 다른 줄의 숫자와 붙지 않는다(대시가 아닌 줄바꿈은 공백 하나)
        let n = norm(&[(1, 1, "100"), (1, 2, "7732-18-5")]);
        assert_eq!(find_cas(&n, "7732-18-5"), Some((1, 2)));
        assert_eq!(find_cas(&n, ""), None);
    }

    #[test]
    fn missed_cas_candidates_include_split_ones() {
        let n = norm(&[(3, 10, "Toluene 108-88-3 12"), (3, 11, "Ethylbenzene 100-41-"), (3, 12, "4 1"), (3, 13, "again 108-88-3")]);
        let found = cas_in_text(&n);
        assert_eq!(found, vec![("108-88-3".to_string(), (3, 10)), ("100-41-4".to_string(), (3, 11))]);
    }

    #[test]
    fn eu_index_number_is_not_cas() {
        // 51쪽 다중 제품 문서 p.3: CAS 1675-54-3 옆의 EC 번호 216-823-5, EU 색인번호 603-073-00-2
        let n = norm(&[(3, 9, "bis(4,1- 1675-54-3  Skin Irrit. 2"), (3, 10, "bisoxir 216-823-5"), (3, 11, "ane  603-073-00-2  Skin Sens. 1")]);
        let found: Vec<String> = cas_in_text(&n).into_iter().map(|(c, _)| c).collect();
        assert_eq!(found, vec!["1675-54-3"]);
        // 원문에서 공백·줄바꿈으로 떨어져 있으면 붙은 것으로 보지 않는다(줄이 나뉜 색인번호는 후보로 남는다)
        let n = norm(&[(1, 1, "603-"), (1, 2, "073-00-2")]);
        assert_eq!(cas_in_text(&n).len(), 1);
        // 앞이 숫자 없는 대시면(목록 기호 등) CAS로 센다
        let n = norm(&[(1, 1, "- 7732-18-5 Water"), (1, 2, "x-108-88-3")]);
        let found: Vec<String> = cas_in_text(&n).into_iter().map(|(c, _)| c).collect();
        assert_eq!(found, vec!["7732-18-5", "108-88-3"]);
    }

    #[test]
    fn h_codes_and_diff() {
        let n = norm(&[(1, 4, "H225 Highly flammable. H302+H312 Harmful"), (1, 5, "H360Fd May damage. EUH066 P210")]);
        let codes = h_codes_in(&n);
        let names: Vec<&str> = codes.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(names, vec!["H225", "H302", "H312", "H360"]);
        assert_eq!(codes[3].1, (1, 5));

        let stored: BTreeSet<String> = ["H225", "H302", "H319"].iter().map(|s| s.to_string()).collect();
        let (missing, added) = h_code_diff(&codes, &stored);
        let missing: Vec<&str> = missing.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(missing, vec!["H312", "H360"]);
        assert_eq!(added, vec!["H319"]);
    }

    #[test]
    fn stored_cas_split() {
        assert_eq!(split_stored_cas("64742-54-7, 64742-65-0"), vec!["64742-54-7", "64742-65-0"]);
        assert_eq!(split_stored_cas("1330-20-7; 100-41-4"), vec!["1330-20-7", "100-41-4"]);
        assert!(split_stored_cas("비공개").is_empty());
        assert!(split_stored_cas("").is_empty());
    }

    #[test]
    fn section_spans_follow_position_order() {
        // 옛 양식: 표준 3번(성분)이 문서에서 2번째 위치
        let starts = [(1, 1, (1, 1)), (1, 3, (1, 20)), (1, 2, (2, 5)), (1, 4, (3, 1)), (2, 1, (5, 1))];
        assert_eq!(section_span(&starts, 1, 3), Some(((1, 20), Some((2, 5)))));
        assert_eq!(section_span(&starts, 1, 4), Some(((3, 1), Some((5, 1)))));
        assert_eq!(section_span(&starts, 2, 1), Some(((5, 1), None)));
        assert_eq!(section_span(&starts, 2, 2), None);
    }

    #[test]
    fn lines_between_pages() {
        let pages = vec![(1, "a\nb\nc".to_string()), (2, "d\ne".to_string())];
        let lines: Vec<&str> = lines_between(&pages, (1, 2), Some((2, 2))).iter().map(|(_, l)| *l).collect();
        assert_eq!(lines, vec!["b", "c", "d"]);
        assert_eq!(lines_between(&pages, (2, 1), None).len(), 2);
    }
}
