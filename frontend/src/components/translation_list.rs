use yew::prelude::*;

use crate::api::Translation;

#[derive(Properties, PartialEq)]
pub struct TranslationListProps {
    pub translations: Vec<Translation>,
    pub korean: bool,
}

/// 섹션별 번역 (펼쳐서 본다)
#[function_component]
pub fn TranslationList(TranslationListProps { translations, korean }: &TranslationListProps) -> Html {
    html! {
        <section class="rounded-lg border border-slate-200 bg-white shadow-sm">
            <h3 class="border-b border-slate-200 px-4 py-3 text-base font-semibold">
                if translations.is_empty() {
                    { "번역" }
                } else {
                    { format!("번역 {}개 섹션", translations.len()) }
                }
            </h3>
            if translations.is_empty() {
                <p class="px-4 py-4 text-sm text-slate-500">
                    { if *korean { "한글 문서라 번역하지 않았습니다" } else { "번역이 없습니다" } }
                </p>
            } else {
                <div class="divide-y divide-slate-100">
                    { for translations.iter().map(item) }
                </div>
            }
        </section>
    }
}

fn item(t: &Translation) -> Html {
    let title = t.title_ko.clone().unwrap_or_else(|| format!("{}번 섹션", t.section_no));
    html! {
        <details key={format!("{}-{}", t.product_no, t.section_no)} class="group px-4 py-2">
            <summary class="cursor-pointer text-sm font-medium text-slate-800 group-open:text-blue-800">
                { title }
            </summary>
            <pre class="mt-2 whitespace-pre-wrap break-words font-sans text-sm leading-relaxed text-slate-700">
                { t.text_ko.clone() }
            </pre>
        </details>
    }
}
