use yew_router::prelude::*;

#[derive(Clone, PartialEq, Routable)]
pub enum Route {
    #[at("/")]
    Home,
    #[at("/docs/:id")]
    Doc { id: String },
    #[not_found]
    #[at("/404")]
    NotFound,
}
