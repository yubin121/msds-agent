// 순수 함수
pub mod cas;
pub mod classification;
pub mod ghs;
pub mod headings;
pub mod text;

mod common;

// 툴
pub mod doc_info;
pub mod find_headings;
pub mod get_section;
pub mod save_components;
pub mod save_hazards;
pub mod save_sections;
pub mod save_translation;

use std::sync::Arc;

use pekko_agent_core::Tool;
use sqlx::PgPool;

pub use doc_info::DocInfoTool;
pub use find_headings::FindHeadingsTool;
pub use get_section::GetSectionTool;
pub use save_components::SaveComponentsTool;
pub use save_hazards::SaveHazardsTool;
pub use save_sections::SaveSectionsTool;
pub use save_translation::SaveTranslationTool;

/// 모든 툴 (tool-test용). 에이전트는 자기 툴만 골라 등록한다.
pub fn all_tools(pool: &PgPool) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(DocInfoTool::new(pool.clone())),
        Arc::new(FindHeadingsTool::new(pool.clone())),
        Arc::new(SaveSectionsTool::new(pool.clone())),
        Arc::new(GetSectionTool::new(pool.clone())),
        Arc::new(SaveComponentsTool::new(pool.clone())),
        Arc::new(SaveHazardsTool::new(pool.clone())),
        Arc::new(SaveTranslationTool::new(pool.clone())),
    ]
}
