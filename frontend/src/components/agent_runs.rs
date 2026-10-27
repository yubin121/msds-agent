use yew::prelude::*;

use crate::api::AgentRun;
use crate::labels::agent_label;

#[derive(Properties, PartialEq)]
pub struct AgentRunsProps {
    pub agents: Vec<AgentRun>,
}

/// 에이전트별 마지막 실행 결과 (실패한 에이전트는 이유를 빨간색으로). 순서는 서버가 정한다(추출 → 성분 → 그림문자 → 번역)
#[function_component]
pub fn AgentRuns(AgentRunsProps { agents }: &AgentRunsProps) -> Html {
    html! {
        <section class="rounded-lg border border-slate-200 bg-white shadow-sm">
            <h2 class="border-b border-slate-200 px-4 py-3 text-base font-semibold">{ "에이전트 실행" }</h2>
            <div class="overflow-x-auto">
                <table class="w-full text-left text-sm">
                    <thead class="bg-slate-50 text-xs text-slate-500">
                        <tr>
                            <th class="px-4 py-2 font-medium">{ "에이전트" }</th>
                            <th class="px-4 py-2 font-medium">{ "결과" }</th>
                            <th class="px-4 py-2 font-medium">{ "시간" }</th>
                            <th class="px-4 py-2 font-medium">{ "호출" }</th>
                            <th class="px-4 py-2 font-medium">{ "입력/출력 토큰" }</th>
                            <th class="px-4 py-2 font-medium">{ "추정 비용" }</th>
                            <th class="px-4 py-2 font-medium">{ "메모" }</th>
                        </tr>
                    </thead>
                    <tbody class="divide-y divide-slate-100">
                        { for agents.iter().map(row) }
                    </tbody>
                    <tfoot class="border-t-2 border-slate-200 bg-slate-50 font-semibold">
                        { total_row(agents) }
                    </tfoot>
                </table>
            </div>
        </section>
    }
}

fn millis(value: &Option<String>) -> Option<f64> {
    let t = js_sys::Date::new(&value.as_deref()?.into()).get_time();
    (!t.is_nan()).then_some(t)
}

/// 에이전트가 동시에 돌므로 시간은 합이 아니라 가장 이른 시작부터 가장 늦은 끝까지
fn elapsed_seconds(agents: &[AgentRun]) -> Option<f64> {
    let start = agents.iter().filter_map(|a| millis(&a.started_at)).reduce(f64::min)?;
    let end = agents.iter().filter_map(|a| millis(&a.finished_at)).reduce(f64::max)?;
    Some((end - start) / 1000.0)
}

fn total_row(agents: &[AgentRun]) -> Html {
    let rounds: i32 = agents.iter().map(|a| a.rounds.unwrap_or(0)).sum();
    let input: i64 = agents.iter().map(|a| a.input_tokens).sum();
    let output: i64 = agents.iter().map(|a| a.output_tokens).sum();
    let known: Vec<f64> = agents.iter().filter_map(|a| a.cost_usd).collect();
    let cost = match (known.is_empty(), known.len() < agents.len()) {
        (true, _) => "-".to_string(),
        (false, false) => format!("${:.4}", known.iter().sum::<f64>()),
        (false, true) => format!("${:.4} 이상(단가 모르는 모델 포함)", known.iter().sum::<f64>()),
    };
    html! {
        <tr>
            <td class="px-4 py-2">{ "합계" }</td>
            <td class="px-4 py-2"></td>
            <td class="whitespace-nowrap px-4 py-2">
                { elapsed_seconds(agents).map(|s| format!("전체 {s:.1}초")).unwrap_or_default() }
            </td>
            <td class="px-4 py-2">{ rounds }</td>
            <td class="whitespace-nowrap px-4 py-2">{ format!("{input} / {output}") }</td>
            <td class="px-4 py-2">{ cost }</td>
            <td class="px-4 py-2"></td>
        </tr>
    }
}

fn row(a: &AgentRun) -> Html {
    html! {
        <tr key={a.agent.clone()}>
            <td class="px-4 py-2 font-medium">{ agent_label(&a.agent) }</td>
            <td class="px-4 py-2">
                if a.ok {
                    <span class="text-emerald-700">{ "성공" }</span>
                } else {
                    <span class="font-semibold text-red-700">{ "실패" }</span>
                }
            </td>
            <td class="px-4 py-2">{ a.seconds.map(|s| format!("{s:.1}초")).unwrap_or_default() }</td>
            <td class="px-4 py-2">{ a.rounds.unwrap_or(0) }</td>
            <td class="whitespace-nowrap px-4 py-2">{ format!("{} / {}", a.input_tokens, a.output_tokens) }</td>
            <td class="px-4 py-2">{ a.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_else(|| "-".into()) }</td>
            <td class={classes!("px-4", "py-2", "text-xs", if a.ok { "text-slate-500" } else { "text-red-700" })}>
                { a.error.clone().unwrap_or_default() }
            </td>
        </tr>
    }
}
