use gloo_timers::callback::Interval;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_router::prelude::*;

use super::agent_runs::AgentRuns;
use super::product_view::ProductView;
use super::status_badge::StatusBadge;
use super::translation_list::TranslationList;
use crate::api::{self, DocDetail as Detail};
use crate::labels::{format_time, is_active, REFRESH_MS};
use crate::route::Route;

#[derive(Properties, PartialEq)]
pub struct DocDetailProps {
    pub id: AttrValue,
}

#[function_component]
pub fn DocDetail(DocDetailProps { id }: &DocDetailProps) -> Html {
    let detail = use_state(|| None::<Detail>);
    let error = use_state(|| None::<String>);
    let selected = use_state(|| 0usize);
    let active = detail.as_ref().map(|d| is_active(&d.doc.status)).unwrap_or(true);
    {
        let (detail, error) = (detail.clone(), error.clone());
        // 분석 중일 때만 다시 불러온다. 끝나면 active가 바뀌어 타이머가 멈춘다
        use_effect_with((id.clone(), active), move |(id, active)| {
            let id = id.clone();
            let load = move || {
                let (detail, error, id) = (detail.clone(), error.clone(), id.clone());
                spawn_local(async move {
                    match api::get_doc(&id).await {
                        Ok(d) => {
                            detail.set(Some(d));
                            error.set(None);
                        }
                        Err(e) => error.set(Some(e)),
                    }
                });
            };
            load();
            let interval = active.then(|| Interval::new(REFRESH_MS, load));
            move || drop(interval)
        });
    }

    let back = html! {
        <Link<Route> to={Route::Home} classes="text-sm text-blue-700 hover:underline">{ "← 목록" }</Link<Route>>
    };
    let Some(d) = &*detail else {
        return html! {
            <div class="space-y-3">
                { back }
                <p class="text-sm text-slate-500">{ error.as_deref().unwrap_or("불러오는 중…") }</p>
            </div>
        };
    };
    let product = d.products.get(*selected).or_else(|| d.products.first());
    let translations: Vec<_> = product
        .map(|p| d.translations.iter().filter(|t| t.product_no == p.product_no).cloned().collect())
        .unwrap_or_default();

    html! {
        <div class="space-y-6">
            { back }
            <section class="rounded-lg border border-slate-200 bg-white p-4 shadow-sm">
                <div class="flex flex-wrap items-center gap-3">
                    <h1 class="text-xl font-bold">
                        { d.doc.file_name.clone().unwrap_or_else(|| d.doc.id.clone()) }
                    </h1>
                    <StatusBadge status={d.doc.status.clone()} />
                </div>
                <p class="mt-1 text-sm text-slate-500">
                    { format!(
                        "언어 {} · {}쪽 · 업로드 {}",
                        d.doc.language.clone().unwrap_or_else(|| "-".into()),
                        d.doc.page_count.unwrap_or(0),
                        format_time(&d.doc.created_at),
                    ) }
                </p>
                if let Some(err) = &d.doc.error {
                    <div class="mt-3 rounded border border-red-200 bg-red-50 px-3 py-2 text-sm text-red-800">
                        <p class="font-semibold">{ "실패 이유" }</p>
                        { for err.lines().map(|l| html! { <p>{ l }</p> }) }
                    </div>
                }
                if let Some(e) = &*error {
                    <p class="mt-2 text-sm text-red-700">{ e }</p>
                }
            </section>

            if !d.agents.is_empty() {
                <AgentRuns agents={d.agents.clone()} />
            }

            if d.products.len() > 1 {
                <nav class="flex flex-wrap gap-2">
                    { for d.products.iter().enumerate().map(|(i, p)| {
                        let selected = selected.clone();
                        let current = Some(p) == product;
                        html! {
                            <button
                                type="button"
                                onclick={Callback::from(move |_| selected.set(i))}
                                class={classes!(
                                    "rounded-full", "border", "px-3", "py-1", "text-sm",
                                    if current {
                                        "border-slate-900 bg-slate-900 text-white"
                                    } else {
                                        "border-slate-300 bg-white text-slate-700 hover:bg-slate-100"
                                    }
                                )}
                            >
                                { format!(
                                    "제품 {}: {}",
                                    p.product_no,
                                    p.product_name.clone().unwrap_or_else(|| "이름 없음".into())
                                ) }
                            </button>
                        }
                    }) }
                </nav>
            }

            {
                match product {
                    Some(p) => html! { <ProductView product={p.clone()} /> },
                    None if is_active(&d.doc.status) => html! {
                        <p class="text-sm text-slate-500">{ "분석이 끝나면 결과가 여기에 나옵니다" }</p>
                    },
                    None => html! { <p class="text-sm text-slate-500">{ "분석 결과가 없습니다" }</p> },
                }
            }

            if product.is_some() {
                <TranslationList
                    {translations}
                    korean={d.doc.language.as_deref() == Some("ko")}
                />
            }
        </div>
    }
}
