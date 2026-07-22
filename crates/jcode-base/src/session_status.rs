//! Session UI status publishing.
//!
//! Computes the UI-facing state (running / waiting / finished / fresh) for a
//! session and writes it to the per-session status file in `jcode-storage`
//! (`~/.jcode/session_status/<id>.json`). External presence UIs (niri
//! workspace sorter, waybar ticker) consume these files instead of
//! re-deriving state from todos, streaming markers, and session JSON.
//!
//! Write points:
//! - Session registration ([`crate::session::Session::mark_active`]):
//!   fresh/waiting/finished snapshot.
//! - Turn lifetime ([`crate::session::StreamingGuard`]): `running` while a
//!   turn streams, recomputed idle state when it ends (on every exit path).
//! - Cleanup happens with `unregister_active_pid` in `jcode-storage`.

pub use crate::storage::{SessionUiState, SessionUiStatus, read_session_ui_status};

/// Display title for a session as shown in terminal window chrome, resolved
/// from disk (rename > working dir > todo/goal title > generated title).
fn status_title(session_id: &str) -> Option<String> {
    crate::process_title::terminal_window_display_title_for_id(session_id)
}

/// Idle state derived from the session's on-disk todo list: waiting only when
/// there is an actual incomplete plan, finished otherwise (including sessions
/// with no todos at all, e.g. Q&A chats, so idle sessions default to green
/// rather than red). Never returns `Fresh`: callers use this after a turn
/// ran, which implies user input.
pub fn idle_state_from_todos(session_id: &str) -> SessionUiState {
    let todos = crate::todo::load_todos(session_id).unwrap_or_default();
    if todos.iter().all(|todo| todo.status == "completed") {
        SessionUiState::Finished
    } else {
        SessionUiState::Waiting
    }
}

/// Write the status file for `session_id` with a freshly resolved title.
pub fn publish(session_id: &str, state: SessionUiState) {
    crate::storage::write_session_ui_status(session_id, state, status_title(session_id));
}

/// RAII guard pairing a turn with its UI status: `running` while alive, the
/// recomputed idle state on drop. Dropping on every exit path (return, `?`,
/// interrupt, panic) keeps the status file from sticking at `running`.
pub struct TurnStatusGuard {
    session_id: String,
}

impl TurnStatusGuard {
    pub fn new(session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        publish(&session_id, SessionUiState::Running);
        Self { session_id }
    }
}

impl Drop for TurnStatusGuard {
    fn drop(&mut self) {
        publish(&self.session_id, idle_state_from_todos(&self.session_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::lock_test_env;

    #[test]
    fn turn_guard_publishes_running_then_idle() {
        let _guard = lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        let id = "session_status_guard_1_aa";
        {
            let _turn = TurnStatusGuard::new(id);
            assert_eq!(
                read_session_ui_status(id).map(|s| s.state),
                Some(SessionUiState::Running)
            );
        }
        // No todos: idle resolves to finished (nothing pending, not red).
        assert_eq!(
            read_session_ui_status(id).map(|s| s.state),
            Some(SessionUiState::Finished)
        );

        // Incomplete plan: idle resolves to waiting.
        let todos: Vec<jcode_task_types::TodoItem> = serde_json::from_str(
            r#"[{"content":"x","status":"in_progress","priority":"high","id":"1"}]"#,
        )
        .expect("parse todos");
        crate::todo::save_todos(id, &todos).expect("save todos");
        {
            let _turn = TurnStatusGuard::new(id);
        }
        assert_eq!(
            read_session_ui_status(id).map(|s| s.state),
            Some(SessionUiState::Waiting)
        );

        // Completed plan: idle resolves to finished.
        let todos: Vec<jcode_task_types::TodoItem> = serde_json::from_str(
            r#"[{"content":"x","status":"completed","priority":"high","id":"1"}]"#,
        )
        .expect("parse todos");
        crate::todo::save_todos(id, &todos).expect("save todos");
        {
            let _turn = TurnStatusGuard::new(id);
        }
        assert_eq!(
            read_session_ui_status(id).map(|s| s.state),
            Some(SessionUiState::Finished)
        );

        jcode_core::env::remove_var("JCODE_HOME");
    }
}
