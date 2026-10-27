use yew::prelude::*;

use crate::api::{Pictogram, Product};
use crate::labels::source_label;

#[derive(Properties, PartialEq)]
pub struct HazardPanelProps {
    pub product: Product,
}

/// 그림문자, 신호어, H코드. H코드도 분류도 없는 문서(no_h_codes)는 안내와 이유를 보여 준다
#[function_component]
pub fn HazardPanel(HazardPanelProps { product }: &HazardPanelProps) -> Html {
    let signal = product.signal_word.as_deref();
    let no_classification = product.pictograms.is_empty() && product.h_codes_without_pictogram.is_empty();
    html! {
        <section class="rounded-lg border border-slate-200 bg-white p-4 shadow-sm">
            <div class="mb-3 flex items-center gap-3">
                <h3 class="text-base font-semibold">{ "유해성·그림문자" }</h3>
                if let Some(s) = signal {
                    <span
                        class={classes!(
                            "rounded", "px-2", "py-0.5", "text-sm", "font-bold",
                            if s == "위험" { "bg-red-600 text-white" } else { "bg-amber-400 text-slate-900" }
                        )}
                    >
                        { format!("신호어: {s}") }
                    </span>
                }
            </div>
            if no_classification {
                <div class="rounded border border-amber-200 bg-amber-50 px-3 py-2 text-sm text-amber-900">
                    <p class="font-semibold">{ "GHS 분류 정보 없음" }</p>
                    <p>{ product.note.clone().unwrap_or_else(|| "문서에 H코드와 GHS 분류가 없습니다".into()) }</p>
                </div>
            } else if product.pictograms.is_empty() {
                <p class="text-sm text-slate-500">{ "그림문자가 없습니다" }</p>
            } else {
                <ul class="flex flex-wrap gap-4">
                    { for product.pictograms.iter().map(card) }
                </ul>
            }
            if !product.h_codes_without_pictogram.is_empty() {
                <p class="mt-3 text-sm text-slate-600">
                    { format!("그림문자 없는 H코드: {}", product.h_codes_without_pictogram.join(", ")) }
                </p>
            }
        </section>
    }
}

fn card(p: &Pictogram) -> Html {
    let sources: Vec<&str> = p.sources.iter().map(|s| source_label(s)).collect();
    html! {
        <li key={p.ghs_code.clone()} class="w-36 text-center">
            <img
                src={format!("/ghs/{}.png", p.ghs_code)}
                alt={format!("{} {}", p.ghs_code, p.symbol_name)}
                class="mx-auto h-24 w-24"
            />
            <p class="mt-1 text-sm font-semibold">{ format!("{} {}", p.ghs_code, p.symbol_name) }</p>
            <p class="text-xs text-slate-600">{ p.h_codes.join(", ") }</p>
            <p class="text-xs text-slate-400">{ format!("근거: {}", sources.join(", ")) }</p>
        </li>
    }
}
