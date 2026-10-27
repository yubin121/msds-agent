use yew::prelude::*;
use yew_router::prelude::*;

use super::doc_detail::DocDetail;
use super::home_page::HomePage;
use crate::route::Route;

fn switch(route: Route) -> Html {
    match route {
        Route::Home => html! { <HomePage /> },
        Route::Doc { id } => html! { <DocDetail {id} /> },
        Route::NotFound => html! { <p class="p-6 text-slate-500">{ "페이지를 찾을 수 없습니다" }</p> },
    }
}

#[function_component]
pub fn App() -> Html {
    html! {
        <BrowserRouter>
            <header class="border-b border-slate-200 bg-white">
                <div class="mx-auto max-w-6xl px-4 py-3">
                    <Link<Route> to={Route::Home} classes="text-lg font-bold text-slate-900">
                        { "MSDS 분석" }
                    </Link<Route>>
                </div>
            </header>
            <main class="mx-auto max-w-6xl px-4 py-6">
                <Switch<Route> render={switch} />
            </main>
        </BrowserRouter>
    }
}
