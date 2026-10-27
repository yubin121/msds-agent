use yew::prelude::*;

use super::component_table::ComponentTable;
use super::hazard_panel::HazardPanel;
use crate::api::Product;

#[derive(Properties, PartialEq)]
pub struct ProductViewProps {
    pub product: Product,
}

#[function_component]
pub fn ProductView(ProductViewProps { product }: &ProductViewProps) -> Html {
    html! {
        <div class="space-y-4">
            <h2 class="text-lg font-semibold">
                { product.product_name.clone().unwrap_or_else(|| format!("제품 {}", product.product_no)) }
            </h2>
            <HazardPanel product={product.clone()} />
            <ComponentTable components={product.components.clone()} />
        </div>
    }
}
