//! Terminal UI crate for gn.

mod app;
mod helpers;
mod input;
mod style;
mod types;
mod view;
mod workflow;

pub use app::AppModel;
pub(crate) use app::runtime_config_modified_at;
pub(crate) use workflow::{PendingSearch, WorkflowUiEvent};

pub(crate) use helpers::{
    center_target_top, format_int_with_commas, normalize_view_lines, open_path_in_editor,
    open_path_in_editor_and_wait, open_url_in_browser, resolve_article_url_for_open,
    truncate_footer_text, truncate_with_ellipsis, window_view_lines,
};
pub(crate) use types::{BOTTOM_BLOCK_ROWS, BrowserOpener, ConfigEditor, LEFT_PAD, UiPalette};
