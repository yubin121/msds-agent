mod api;
mod components;
mod labels;
mod route;

fn main() {
    yew::Renderer::<components::app::App>::new().render();
}
