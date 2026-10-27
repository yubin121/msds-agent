use web_sys::HtmlInputElement;
use yew::platform::spawn_local;
use yew::prelude::*;

use crate::api;

#[derive(Properties, PartialEq)]
pub struct UploadFormProps {
    pub on_uploaded: Callback<()>,
}

/// 업로드 결과 안내 (성공 / 서버 오류 메시지)
#[derive(Clone, PartialEq)]
enum Notice {
    Ok(String),
    Error(String),
}

#[function_component]
pub fn UploadForm(UploadFormProps { on_uploaded }: &UploadFormProps) -> Html {
    let input = use_node_ref();
    let busy = use_state(|| false);
    let notice = use_state(|| None::<Notice>);

    let onsubmit = {
        let (input, busy, notice, on_uploaded) = (input.clone(), busy.clone(), notice.clone(), on_uploaded.clone());
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            let Some(el) = input.cast::<HtmlInputElement>() else { return };
            let Some(file) = el.files().and_then(|f| f.get(0)) else {
                notice.set(Some(Notice::Error("파일을 선택하세요".into())));
                return;
            };
            busy.set(true);
            notice.set(None);
            let (busy, notice, on_uploaded) = (busy.clone(), notice.clone(), on_uploaded.clone());
            spawn_local(async move {
                match api::upload(&file).await {
                    Ok(()) => {
                        notice.set(Some(Notice::Ok(format!("{} 업로드 완료. 분석을 시작합니다.", file.name()))));
                        el.set_value("");
                        on_uploaded.emit(());
                    }
                    Err(msg) => notice.set(Some(Notice::Error(msg))),
                }
                busy.set(false);
            });
        })
    };

    html! {
        <form {onsubmit} class="rounded-lg border border-slate-200 bg-white p-4 shadow-sm">
            <h2 class="mb-3 text-base font-semibold">{ "MSDS 업로드" }</h2>
            <div class="flex flex-wrap items-center gap-3">
                <input
                    ref={input}
                    type="file"
                    accept="application/pdf,.pdf"
                    class={classes!(
                        "text-sm", "file:mr-3", "file:rounded", "file:border-0",
                        "file:bg-slate-100", "file:px-3", "file:py-1.5", "file:text-sm"
                    )}
                />
                <button
                    type="submit"
                    disabled={*busy}
                    class={classes!(
                        "rounded", "bg-slate-900", "px-4", "py-1.5", "text-sm", "font-medium",
                        "text-white", "hover:bg-slate-700", "disabled:opacity-50"
                    )}
                >
                    { if *busy { "올리는 중…" } else { "업로드" } }
                </button>
            </div>
            {
                match &*notice {
                    Some(Notice::Ok(m)) => html! { <p class="mt-3 text-sm text-emerald-700">{ m }</p> },
                    Some(Notice::Error(m)) => html! { <p class="mt-3 text-sm text-red-700">{ m }</p> },
                    None => html! {},
                }
            }
        </form>
    }
}
