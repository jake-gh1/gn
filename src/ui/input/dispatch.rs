use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{debug_log, ensure_runtime_config_file};
use crate::ui::*;

#[derive(Clone, Copy)]
enum FocusDirection {
    Up,
    Down,
}

impl FocusDirection {
    fn delta(self) -> isize {
        match self {
            Self::Up => -1,
            Self::Down => 1,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

impl AppModel {
    pub(crate) fn open_config_editor_from_ui(&mut self) -> Result<()> {
        let path = self.runtime_config_path();
        ensure_runtime_config_file(&path)?;
        self.runtime_config_last_modified = runtime_config_modified_at(&path);
        self.runtime_config_last_path = Some(path.clone());
        (self.config_editor)(&path).map_err(anyhow::Error::msg)?;
        Ok(())
    }

    fn handle_empty_select_root(&mut self) {
        self.handle_empty_story_action(
            "empty_select_root.before",
            "empty_select_root.after_story_select",
            Self::select_story_root,
        );
    }

    fn handle_empty_story_action(
        &mut self,
        before_label: &str,
        after_label: &str,
        select: fn(&mut Self, usize),
    ) {
        self.log_ui_state(before_label);
        let row_count = self.news_article_row_count();
        if self.story_menu_focused && row_count > 0 {
            let highlight = self.story_menu_highlight.min(row_count.saturating_sub(1));
            select(self, highlight);
            self.log_ui_state(after_label);
        }
    }

    pub(crate) fn move_focus_up(&mut self) {
        self.move_focus(FocusDirection::Up);
    }

    pub(crate) fn move_focus_down(&mut self) {
        self.move_focus(FocusDirection::Down);
    }

    pub(crate) fn move_story_menu_highlight(&mut self, delta: isize) -> bool {
        let row_count = self.news_article_row_count();
        if row_count == 0 {
            return false;
        }
        let current = self.story_menu_highlight.min(row_count - 1);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            (current + delta as usize).min(row_count - 1)
        };
        let changed = next != self.story_menu_highlight;
        self.story_menu_highlight = next;
        changed
    }

    fn move_focus(&mut self, direction: FocusDirection) {
        self.log_focus(direction, "before");
        if self.move_focus_within_story_navigation(direction) {
            return;
        }
        self.log_focus(direction, "plain");
    }

    fn move_focus_within_story_navigation(&mut self, direction: FocusDirection) -> bool {
        let row_count = self.news_article_row_count();
        if row_count == 0 {
            return false;
        }

        let entering_story_menu = !self.story_menu_focused;
        if self.story_menu_focused {
            self.move_story_menu_highlight(direction.delta());
        } else {
            self.story_menu_focused = true;
            self.story_menu_highlight = self.story_menu_highlight.min(row_count.saturating_sub(1));
        }

        if !(entering_story_menu && matches!(direction, FocusDirection::Down)) {
            self.log_focus(direction, "story");
        }
        true
    }

    fn log_focus(&self, direction: FocusDirection, stage: &str) {
        debug_log(
            "ui",
            format!(
                "focus.{} {} {}",
                direction.name(),
                stage,
                self.focus_debug_state()
            ),
        );
    }

    pub(crate) fn is_quit_shortcut(key: &KeyEvent) -> bool {
        matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
    }

    pub(crate) fn is_delete_word_shortcut(key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Backspace | KeyCode::Delete => key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL),
            KeyCode::Char('w') | KeyCode::Char('W') => {
                key.modifiers.contains(KeyModifiers::CONTROL)
            }
            _ => false,
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if Self::is_quit_shortcut(&key) {
            return Ok(true);
        }

        if key.code == KeyCode::Esc {
            return Ok(false);
        }

        match key.code {
            KeyCode::Up => {
                self.move_focus_up();
            }
            KeyCode::Down => {
                self.move_focus_down();
            }
            KeyCode::Left | KeyCode::Right => {
                let row_count = self.news_article_row_count();
                if row_count > 0 {
                    self.story_menu_focused = true;
                    self.story_menu_highlight = if key.code == KeyCode::Left {
                        0
                    } else {
                        row_count - 1
                    };
                    self.log_ui_state("jump_story_edge");
                }
            }
            KeyCode::Tab => {
                self.handle_empty_select_root();
                return Ok(false);
            }
            KeyCode::Char(c) => {
                if Self::is_delete_word_shortcut(&key) {
                    return Ok(false);
                }
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
                {
                    return Ok(false);
                }
                if self.story_menu_focused
                    && let Some(idx) = c.to_digit(10).and_then(|digit| digit.checked_sub(1))
                {
                    let idx = idx as usize;
                    if idx < self.news_article_row_count() {
                        self.select_story_root(idx);
                        return Ok(false);
                    }
                }
            }
            KeyCode::Backspace | KeyCode::Delete => {
                if Self::is_delete_word_shortcut(&key) {
                    return Ok(false);
                }
            }
            KeyCode::Enter => {
                self.handle_empty_select_root();
                return Ok(false);
            }
            _ => {}
        }

        Ok(false)
    }
}
