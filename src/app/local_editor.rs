use super::*;

use std::path::Path;

/// Split a user-provided editor command without invoking a shell. This handles
/// the quoting normally used in `$VISUAL` / `$EDITOR` while keeping the temp
/// file path a separate argv item.
pub(crate) fn parse_editor_command(command: &str) -> Result<Vec<String>> {
    let mut argv = Vec::new();
    let mut word = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut started = false;

    for ch in command.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            started = true;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                word.push(ch);
            }
            started = true;
            continue;
        }
        if in_double {
            match ch {
                '"' => in_double = false,
                '\\' => escaped = true,
                _ => word.push(ch),
            }
            started = true;
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                started = true;
            }
            '"' => {
                in_double = true;
                started = true;
            }
            '\\' => {
                escaped = true;
                started = true;
            }
            c if c.is_whitespace() => {
                if started {
                    argv.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(ch);
                started = true;
            }
        }
    }

    if escaped || in_single || in_double {
        anyhow::bail!("editor command has an unfinished quote or escape");
    }
    if started {
        argv.push(word);
    }
    if argv.is_empty() {
        anyhow::bail!("editor command is empty");
    }
    Ok(argv)
}

pub(crate) fn local_editor_command() -> String {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "nano".into())
}

pub(crate) fn local_editor_argv(command: &str, path: &Path) -> Result<Vec<String>> {
    let mut argv = parse_editor_command(command)?;
    argv.push(path.to_string_lossy().into_owned());
    Ok(argv)
}

impl App {
    /// Open the local editor for the already downloaded working copy.
    pub(crate) fn start_local_editor(&mut self) -> Result<()> {
        let Some((path, name)) = self.remote_edit.as_ref().and_then(|edit| {
            matches!(
                edit.phase,
                RemoteEditPhase::Editing | RemoteEditPhase::RetryingEditor
            )
            .then(|| {
                (
                    edit.local_path.clone(),
                    edit.local_path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "file".into()),
                )
            })
        }) else {
            return Ok(());
        };

        let argv = match local_editor_argv(&local_editor_command(), &path) {
            Ok(argv) => argv,
            Err(e) => {
                self.remote_edit_editor_failed(format!("could not start local editor: {e:#}"));
                return Ok(());
            }
        };
        let before = self.sessions.len();
        self.spawn_embedded_session(
            argv,
            format!("edit: {name}"),
            crate::session::SessionMeta::default(),
            None,
            "local-editor",
        )?;
        if self.sessions.len() == before {
            let message = self
                .host_notice
                .clone()
                .unwrap_or_else(|| "could not start local editor".into());
            self.remote_edit_editor_failed(message);
            return Ok(());
        }

        if let Some(edit) = self.remote_edit.as_mut() {
            edit.phase = RemoteEditPhase::Editing;
            edit.editor_session = Some(self.sessions.len() - 1);
        }
        Ok(())
    }

    /// Detect the local editor's PTY exit independently of which session tab is
    /// visible. A successful editor exit starts the guarded remote upload.
    pub(crate) fn tick_remote_edit(&mut self) {
        let Some(idx) = self
            .remote_edit
            .as_ref()
            .and_then(|edit| edit.editor_session)
        else {
            return;
        };
        let Some(status) = self
            .sessions
            .get(idx)
            .and_then(|session| match &session.phase {
                crate::session::SessionPhase::Exited { status, .. } => Some(status.clone()),
                _ => None,
            })
        else {
            return;
        };

        let editor_was_visible = self.active_session == Some(idx) && self.session_is_rendered();
        if let Some(edit) = self.remote_edit.as_mut() {
            edit.editor_session = None;
        }
        if self.active_session == Some(idx) {
            self.close_active_session();
        } else if idx < self.sessions.len() {
            self.sessions.remove(idx);
            if let Some(active) = self.active_session {
                if active > idx {
                    self.active_session = Some(active - 1);
                }
            }
        }
        if editor_was_visible {
            self.mode = AppMode::Normal;
            self.active_tab = 1;
        }

        if status == "success" {
            if self
                .remote_edit
                .as_ref()
                .is_some_and(|edit| edit.source == EditSource::Local)
            {
                self.local_edit_finished();
            } else {
                self.remote_edit_start_upload();
            }
        } else {
            self.remote_edit_editor_failed(format!("local editor exited with {status}"));
        }
    }

    /// An in-place local-file edit ended successfully: there is no remote
    /// working copy to upload, so the state is dropped and the panes are
    /// refreshed so size/mtime changes show up.
    pub(crate) fn local_edit_finished(&mut self) {
        self.remote_edit = None;
        if let Some(s) = self.sftp.as_mut() {
            // A queue may have been started while the editor was open: don't
            // clobber its phase, or the next `c` would re-dispatch transfers
            // that are still running.
            if s.running.is_empty() {
                s.phase = crate::sftp::model::Phase::Browsing;
            }
            s.progress = None;
            s.notice = Some("local file edited".into());
        }
        self.sftp_refresh_panes();
    }

    pub(crate) fn remote_edit_editor_failed(&mut self, message: String) {
        if let Some(edit) = self.remote_edit.as_mut() {
            edit.phase = RemoteEditPhase::RetryingEditor;
            edit.editor_session = None;
        }
        if let Some(s) = self.sftp.as_mut() {
            s.phase = crate::sftp::model::Phase::Browsing;
            s.progress = None;
            s.notice = Some(format!("{message} — press e to retry or Esc to discard"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{local_editor_argv, parse_editor_command};

    #[test]
    fn parses_editor_arguments_and_quotes() {
        assert_eq!(
            parse_editor_command("code --wait 'profile one'").unwrap(),
            vec!["code", "--wait", "profile one"]
        );
    }

    #[test]
    fn keeps_the_file_path_as_one_argument() {
        let argv = local_editor_argv("nvim -f", std::path::Path::new("/tmp/a file.toml")).unwrap();
        assert_eq!(argv, vec!["nvim", "-f", "/tmp/a file.toml"]);
    }

    #[test]
    fn rejects_unfinished_editor_quotes() {
        assert!(parse_editor_command("nvim '").is_err());
    }
}
