use gloo_timers::callback::Interval;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_router::prelude::*;

use super::status_badge::StatusBadge;
use crate::api::{self, DocSummary};
use crate::labels::{format_time, REFRESH_MS};
use crate::route::Route;

#[derive(Properties, PartialEq)]
pub struct DocListProps {
    /// 값이 바뀌면 바로 다시 불러온다(업로드 직후)
    pub refresh: u32,
}

#[function_component]
pub fn DocList(DocListProps { refresh }: &DocListProps) -> Html {
    let docs = use_state(|| None::<Vec<DocSummary>>);
    let error = use_state(|| None::<String>);
    {
        let (docs, error) = (docs.clone(), error.clone());
        use_effect_with(*refresh, move |_| {
            let load = move || {
                let (docs, error) = (docs.clone(), error.clone());
                spawn_local(async move {
                    match api::list_docs().await {
                        Ok(d) => {
                            docs.set(Some(d));
                            error.set(None);
                        }
                        Err(e) => error.set(Some(e)),
                    }
                });
            };
            load();
            let interval = Interval::new(REFRESH_MS, load);
            move || drop(interval)
        });
    }

    html! {
        <section class="rounded-lg border border-slate-200 bg-white shadow-sm">
            <div class="flex items-center justify-between border-b border-slate-200 px-4 py-3">
                <h2 class="text-base font-semibold">{ "문서 목록" }</h2>
                <span class="text-xs text-slate-500">{ "3초마다 갱신" }</span>
            </div>
            if let Some(e) = &*error {
                <p class="px-4 py-2 text-sm text-red-700">{ e }</p>
            }
            {
                match &*docs {
                    None => html! { <p class="px-4 py-6 text-sm text-slate-500">{ "불러오는 중…" }</p> },
                    Some(d) if d.is_empty() => html! {
                        <p class="px-4 py-6 text-sm text-slate-500">{ "올린 문서가 없습니다" }</p>
                    },
                    Some(d) => html! {
                        <div class="overflow-x-auto">
                            <table class="w-full text-left text-sm">
                                <thead class="bg-slate-50 text-xs text-slate-500">
                                    <tr>
                                        <th class="px-4 py-2 font-medium">{ "파일명" }</th>
                                        <th class="px-4 py-2 font-medium">{ "상태" }</th>
                                        <th class="px-4 py-2 font-medium">{ "언어" }</th>
                                        <th class="px-4 py-2 font-medium">{ "페이지" }</th>
                                        <th class="px-4 py-2 font-medium">{ "업로드 시각" }</th>
                                        <th class="px-4 py-2 font-medium">{ "오류" }</th>
                                    </tr>
                                </thead>
                                <tbody class="divide-y divide-slate-100">
                                    { for d.iter().map(row) }
                                </tbody>
                            </table>
                        </div>
                    },
                }
            }
        </section>
    }
}

fn row(doc: &DocSummary) -> Html {
    let name = doc.file_name.clone().unwrap_or_else(|| doc.id.clone());
    html! {
        <tr key={doc.id.clone()} class="hover:bg-slate-50">
            <td class="px-4 py-2">
                <Link<Route>
                    to={Route::Doc { id: doc.id.clone() }}
                    classes="font-medium text-blue-700 hover:underline"
                >
                    { name }
                </Link<Route>>
            </td>
            <td class="px-4 py-2"><StatusBadge status={doc.status.clone()} /></td>
            <td class="px-4 py-2">{ doc.language.clone().unwrap_or_default() }</td>
            <td class="px-4 py-2">{ doc.page_count.map(|p| p.to_string()).unwrap_or_default() }</td>
            <td class="whitespace-nowrap px-4 py-2 text-slate-600">{ format_time(&doc.created_at) }</td>
            <td class="max-w-xs truncate px-4 py-2 text-red-700" title={doc.error.clone()}>
                { doc.error.clone().unwrap_or_default() }
            </td>
        </tr>
    }
}
