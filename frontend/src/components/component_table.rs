use yew::prelude::*;

use crate::api::Component;

#[derive(Properties, PartialEq)]
pub struct ComponentTableProps {
    pub components: Vec<Component>,
}

/// 성분 표. CAS 체크섬이 틀린 번호(cas_valid=false)는 빨간색으로 표시한다(원문 그대로 저장한 값)
#[function_component]
pub fn ComponentTable(ComponentTableProps { components }: &ComponentTableProps) -> Html {
    let invalid = components.iter().filter(|c| c.cas_valid == Some(false)).count();
    html! {
        <section class="rounded-lg border border-slate-200 bg-white shadow-sm">
            <div class="flex items-center gap-3 border-b border-slate-200 px-4 py-3">
                <h3 class="text-base font-semibold">{ format!("구성성분 {}개", components.len()) }</h3>
                if invalid > 0 {
                    <span class="rounded bg-red-100 px-2 py-0.5 text-xs font-semibold text-red-800">
                        { format!("CAS 확인 필요 {invalid}건") }
                    </span>
                }
            </div>
            if components.is_empty() {
                <p class="px-4 py-4 text-sm text-slate-500">{ "저장된 성분이 없습니다" }</p>
            } else {
                <div class="overflow-x-auto">
                    <table class="w-full text-left text-sm">
                        <thead class="bg-slate-50 text-xs text-slate-500">
                            <tr>
                                <th class="px-4 py-2 font-medium">{ "성분" }</th>
                                <th class="px-4 py-2 font-medium">{ "CAS" }</th>
                                <th class="px-4 py-2 font-medium">{ "함유량" }</th>
                            </tr>
                        </thead>
                        <tbody class="divide-y divide-slate-100">
                            { for components.iter().map(row) }
                        </tbody>
                    </table>
                </div>
            }
        </section>
    }
}

fn row(c: &Component) -> Html {
    let cas = c.cas_no.clone().unwrap_or_else(|| "-".into());
    html! {
        <tr>
            <td class="px-4 py-2">{ c.component.clone().unwrap_or_default() }</td>
            <td class="whitespace-nowrap px-4 py-2 font-mono">
                if c.cas_valid == Some(false) {
                    <span class="font-semibold text-red-700" title="CAS 체크섬이 맞지 않습니다(원문 표기)">
                        { cas }
                    </span>
                    <span class="ml-2 rounded bg-red-100 px-1.5 py-0.5 font-sans text-xs text-red-800">
                        { "체크섬 오류" }
                    </span>
                } else {
                    { cas }
                }
            </td>
            <td class="px-4 py-2">{ c.percent.clone().unwrap_or_default() }</td>
        </tr>
    }
}
