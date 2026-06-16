use crate::ui::*;

impl AppModel {
    pub(crate) fn set_status_message(&mut self, content: &str) {
        self.status_message = Some(content.to_string());
    }

    pub(crate) fn active_model_requires_provider(&self, provider: &str) -> bool {
        if self.active_model >= self.runtime.models.len() {
            return false;
        }
        self.runtime.models[self.active_model]
            .provider
            .trim()
            .eq_ignore_ascii_case(provider)
    }

    pub(crate) fn check_provider_auth(&mut self, reason: &str) {
        if self.active_model_requires_provider("codex") {
            self.maybe_start_implicit_codex_login(reason);
        }
    }

    pub(crate) fn maybe_start_implicit_codex_login(&mut self, reason: &str) {
        match crate::llm::codex::codex_login_status() {
            Ok(status) if status.logged_in => {
                self.codex_auth_in_flight = false;
                return;
            }
            Err(err) => {
                let msg = err.to_string();
                if !msg.contains("not logged in") {
                    self.set_status_message(&format!("Codex auth check failed: {err}"));
                    return;
                }
            }
            _ => {}
        }
        if self.codex_auth_in_flight {
            return;
        }
        let msg = match reason {
            "startup" => {
                "Codex is the active model, but no Codex auth was found. Run `codex login` in another terminal so `~/.codex/auth.json` is populated."
            }
            "model switch" => {
                "Switched to Codex, but no Codex auth was found. Run `codex login` in another terminal so `~/.codex/auth.json` is populated."
            }
            "workflow" => {
                "Codex needs an existing login before running this workflow. Run `codex login` in another terminal so `~/.codex/auth.json` is populated."
            }
            _ => {
                "Codex auth was not found. Run `codex login` in another terminal so `~/.codex/auth.json` is populated."
            }
        };
        self.set_status_message(msg);
        self.codex_auth_in_flight = true;
    }

    pub(crate) fn ensure_provider_auth_for_workflow(&mut self) -> bool {
        if self.active_model_requires_provider("codex") {
            match crate::llm::codex::codex_login_status() {
                Ok(status) if status.logged_in => return true,
                _ => {
                    self.maybe_start_implicit_codex_login("workflow");
                    return false;
                }
            }
        }
        true
    }
}
