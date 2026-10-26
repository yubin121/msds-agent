use std::collections::HashMap;
use std::time::Duration;

use pekko_agent_core::{ToolContext, ToolDefinition};
use pekko_agent_llm::{
    ClaudeClient, ClaudeMessage, ClaudeTool, ClientError, ContentBlock, LlmConfig, LlmRequest, LlmResponse,
};
use pekko_agent_tools::ToolRegistry;
use tracing::{info, warn};
use uuid::Uuid;

/// 툴 루프 최대 라운드. MSDS_MAX_ROUNDS로 바꿀 수 있다(1~20).
const DEFAULT_MAX_ROUNDS: u32 = 10;
const MAX_ROUNDS_RANGE: std::ops::RangeInclusive<u32> = 1..=20;
const MAX_TOOL_RESULT_BYTES: usize = 30_000;
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const ROUND_LIMIT_NOTICE: &str =
    "도구 호출 한도에 도달했다. 더 이상 도구를 쓰지 말고 지금까지의 결과로 최종 답을 작성하라";

#[derive(Debug, Clone)]
pub struct LoopResult {
    pub final_text: String,
    pub tools_used: Vec<String>,
    pub rounds: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug)]
pub enum LoopError {
    Llm(ClientError),
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoopError::Llm(e) => write!(f, "LLM 호출 실패: {e}"),
        }
    }
}

impl std::error::Error for LoopError {}

impl From<ClientError> for LoopError {
    fn from(e: ClientError) -> Self {
        LoopError::Llm(e)
    }
}

/// .env의 CLAUDE_API_KEY, CLAUDE_MODEL로 클라이언트를 만든다. 키는 로그에 남기지 않는다.
pub fn claude_client_from_env() -> Result<ClaudeClient, String> {
    let api_key = std::env::var("CLAUDE_API_KEY").unwrap_or_default();
    if api_key.trim().is_empty() {
        return Err("CLAUDE_API_KEY가 비어 있습니다. .env를 확인하세요".into());
    }
    let model = std::env::var("CLAUDE_MODEL").unwrap_or_default();
    if model.trim().is_empty() {
        return Err("CLAUDE_MODEL이 비어 있습니다. .env를 확인하세요".into());
    }
    Ok(ClaudeClient::new(LlmConfig {
        api_key,
        model,
        timeout_secs: 180,
        temperature: None,
        ..LlmConfig::default()
    }))
}

/// MSDS_MAX_ROUNDS 값을 해석한다. 없거나 숫자가 아니면 기본 10, 범위를 벗어나면 1~20으로 맞춘다.
fn parse_max_rounds(raw: Option<&str>) -> u32 {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => DEFAULT_MAX_ROUNDS,
        Some(s) => match s.parse::<i64>() {
            Ok(n) => n.clamp(*MAX_ROUNDS_RANGE.start() as i64, *MAX_ROUNDS_RANGE.end() as i64) as u32,
            Err(_) => {
                warn!(value = s, "MSDS_MAX_ROUNDS가 숫자가 아닙니다. 기본값 {DEFAULT_MAX_ROUNDS}을 씁니다");
                DEFAULT_MAX_ROUNDS
            }
        },
    }
}

pub fn max_rounds() -> u32 {
    parse_max_rounds(std::env::var("MSDS_MAX_ROUNDS").ok().as_deref())
}

pub async fn run_tool_loop(
    client: &ClaudeClient,
    registry: &mut ToolRegistry,
    system_prompt: &str,
    user_message: String,
    max_tokens: u32,
) -> Result<LoopResult, LoopError> {
    let tools = to_claude_tools(&registry.list_tools());
    let max_rounds = max_rounds();
    let ctx = ToolContext {
        tenant_id: "local".into(),
        user_id: "msds-agent".into(),
        session_id: Uuid::new_v4(),
        credentials: HashMap::new(),
        timeout: TOOL_TIMEOUT,
    };

    let mut messages = vec![ClaudeMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text { text: user_message }],
    }];
    let mut result = LoopResult {
        final_text: String::new(),
        tools_used: Vec::new(),
        rounds: 0,
        input_tokens: 0,
        output_tokens: 0,
    };

    while result.rounds < max_rounds {
        result.rounds += 1;
        let resp = call(client, system_prompt, &messages, &tools, max_tokens, &mut result).await?;
        let tool_uses = tool_uses(&resp);
        log_round(result.rounds, &resp, &tool_uses);

        if resp.stop_reason != "tool_use" {
            if resp.stop_reason == "max_tokens" {
                warn!(round = result.rounds, max_tokens, "max_tokens에 도달해 응답이 잘렸을 수 있습니다");
            }
            result.final_text = collect_text(&resp.content);
            return Ok(result);
        }

        // thinking 블록 포함, 응답 블록을 순서·내용 그대로 되돌린다
        messages.push(ClaudeMessage { role: "assistant".into(), content: resp.content.clone() });

        let mut results = Vec::with_capacity(tool_uses.len());
        for (id, name, input) in tool_uses {
            result.tools_used.push(name.clone());
            results.push(execute_tool(registry, &ctx, id, &name, input).await);
        }
        // 한 라운드의 tool_result는 user 메시지 하나에 모은다
        messages.push(ClaudeMessage { role: "user".into(), content: results });
    }

    // 한도 소진: 마지막 user 메시지(tool_result 모음) 뒤에 안내를 붙여 한 번 더 호출한다.
    // 대화에 tool_use/tool_result 블록이 있으면 API가 tools 정의를 요구하므로 tools는 그대로 보내고
    // 안내 문구로 툴 사용을 막는다.
    warn!(rounds = max_rounds, "도구 호출 한도 도달, 툴 사용 금지 안내를 붙여 마무리 호출");
    if let Some(last) = messages.last_mut() {
        last.content.push(ContentBlock::Text { text: ROUND_LIMIT_NOTICE.into() });
    }
    let resp = call(client, system_prompt, &messages, &tools, max_tokens, &mut result).await?;
    let tool_uses = tool_uses(&resp);
    log_round(result.rounds + 1, &resp, &tool_uses);
    if resp.stop_reason == "tool_use" {
        warn!("한도 도달 후에도 tool_use가 왔습니다. 텍스트 블록만 반환합니다");
    } else if resp.stop_reason == "max_tokens" {
        warn!(max_tokens, "max_tokens에 도달해 최종 답이 잘렸을 수 있습니다");
    }
    result.final_text = collect_text(&resp.content);
    Ok(result)
}

async fn call(
    client: &ClaudeClient,
    system_prompt: &str,
    messages: &[ClaudeMessage],
    tools: &[ClaudeTool],
    max_tokens: u32,
    result: &mut LoopResult,
) -> Result<LlmResponse, ClientError> {
    let req = LlmRequest {
        system_prompt: system_prompt.to_string(),
        messages: messages.to_vec(),
        tools: tools.to_vec(),
        max_tokens,
        temperature: None,
        cacheable: false,
    };
    let resp = client.send_message(&req).await?;
    result.input_tokens += resp.usage.input_tokens;
    result.output_tokens += resp.usage.output_tokens;
    Ok(resp)
}

async fn execute_tool(
    registry: &mut ToolRegistry,
    ctx: &ToolContext,
    tool_use_id: String,
    name: &str,
    input: serde_json::Value,
) -> ContentBlock {
    // ToolRegistry는 ctx.timeout을 적용하지 않으므로 여기서 감싼다
    let outcome = tokio::time::timeout(ctx.timeout, registry.execute(name, input, ctx)).await;
    let (content, is_error) = match outcome {
        Ok(Ok(out)) => {
            let json = serde_json::to_string(&out.content).unwrap_or_else(|e| format!("결과 직렬화 실패: {e}"));
            if out.is_error {
                warn!(tool = %name, "툴이 오류를 반환했습니다: {json}");
                (json, Some(true))
            } else {
                (json, None)
            }
        }
        Ok(Err(e)) => {
            warn!(tool = %name, error = %e, "툴 실행 실패");
            (serde_json::json!({ "error": e.to_string() }).to_string(), Some(true))
        }
        Err(_) => {
            warn!(tool = %name, timeout_secs = ctx.timeout.as_secs(), "툴 실행 시간 초과");
            let msg = format!("툴 실행 시간 초과 ({}초)", ctx.timeout.as_secs());
            (serde_json::json!({ "error": msg }).to_string(), Some(true))
        }
    };
    ContentBlock::ToolResult { tool_use_id, content: truncate_tool_result(&content), is_error }
}

fn to_claude_tools(defs: &[ToolDefinition]) -> Vec<ClaudeTool> {
    defs.iter()
        .map(|d| ClaudeTool {
            name: d.name.clone(),
            description: d.description.clone(),
            input_schema: d.input_schema.clone(),
        })
        .collect()
}

fn tool_uses(resp: &LlmResponse) -> Vec<(String, String, serde_json::Value)> {
    resp.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
            _ => None,
        })
        .collect()
}

fn collect_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn log_round(round: u32, resp: &LlmResponse, tool_uses: &[(String, String, serde_json::Value)]) {
    let names: Vec<&str> = tool_uses.iter().map(|(_, n, _)| n.as_str()).collect();
    info!(
        round,
        stop_reason = %resp.stop_reason,
        tools = ?names,
        input_tokens = resp.usage.input_tokens,
        output_tokens = resp.usage.output_tokens,
        "툴 루프 라운드"
    );
}

/// MAX_TOOL_RESULT_BYTES를 넘으면 문자 경계에서 자르고 안내를 붙인다.
fn truncate_tool_result(s: &str) -> String {
    truncate_at_char_boundary(s, MAX_TOOL_RESULT_BYTES)
}

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[결과가 너무 길어 잘렸습니다: 전체 {}바이트 중 앞 {}바이트만 표시]",
        &s[..end],
        s.len(),
        end
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_string_is_unchanged() {
        assert_eq!(truncate_at_char_boundary("물질안전보건자료", 100), "물질안전보건자료");
    }

    #[test]
    fn korean_is_cut_on_char_boundary() {
        // 한글 한 글자 = 3바이트. 10바이트 제한이면 3글자(9바이트)까지만 남아야 한다
        let out = truncate_at_char_boundary("가나다라마", 10);
        assert!(out.starts_with("가나다\n\n["), "{out}");
        assert!(out.contains("전체 15바이트 중 앞 9바이트"), "{out}");
    }

    #[test]
    fn every_cut_point_is_valid_utf8() {
        let s = "ab한글®テスト😀섞인 문자열".repeat(3);
        for max in 0..s.len() {
            let out = truncate_at_char_boundary(&s, max);
            let kept = out.split("\n\n[").next().unwrap();
            assert!(kept.len() <= max);
            assert!(s.starts_with(kept));
        }
    }

    #[test]
    fn max_rounds_env() {
        assert_eq!(parse_max_rounds(None), 10);
        assert_eq!(parse_max_rounds(Some("")), 10);
        assert_eq!(parse_max_rounds(Some("abc")), 10);
        assert_eq!(parse_max_rounds(Some(" 1 ")), 1);
        assert_eq!(parse_max_rounds(Some("20")), 20);
        assert_eq!(parse_max_rounds(Some("0")), 1);
        assert_eq!(parse_max_rounds(Some("-3")), 1);
        assert_eq!(parse_max_rounds(Some("99")), 20);
    }

    #[test]
    fn real_limit_with_korean() {
        let s = "가".repeat(20_000); // 60,000바이트
        let out = truncate_tool_result(&s);
        assert!(out.starts_with(&"가".repeat(10_000))); // 30,000바이트 = 정확히 10,000자
        assert!(!out.starts_with(&"가".repeat(10_001)));
        assert!(out.contains("잘렸습니다"));
    }
}
