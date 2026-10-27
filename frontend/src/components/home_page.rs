use yew::prelude::*;

use super::doc_list::DocList;
use super::upload_form::UploadForm;

#[function_component]
pub fn HomePage() -> Html {
    // 업로드가 끝나면 값을 올려 목록을 바로 다시 불러온다
    let refresh = use_state(|| 0u32);
    let on_uploaded = {
        let refresh = refresh.clone();
        Callback::from(move |_| refresh.set(*refresh + 1))
    };
    html! {
        <div class="space-y-6">
            <UploadForm {on_uploaded} />
            <DocList refresh={*refresh} />
        </div>
    }
}
