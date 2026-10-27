use yew::prelude::*;

use crate::labels::{status_classes, status_label};

#[derive(Properties, PartialEq)]
pub struct StatusBadgeProps {
    pub status: AttrValue,
}

#[function_component]
pub fn StatusBadge(StatusBadgeProps { status }: &StatusBadgeProps) -> Html {
    html! {
        <span
            class={classes!(
                "inline-block", "whitespace-nowrap", "rounded-full",
                "px-2.5", "py-0.5", "text-xs", "font-semibold",
                status_classes(status)
            )}
        >
            { status_label(status) }
        </span>
    }
}
