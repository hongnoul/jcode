//! Per-session UI status files under `~/.jcode/session_status`.
//!
//! Each active session gets one small JSON file describing the state a
//! presence UI should show for it (running / waiting / finished / fresh) plus
//! the display title jcode puts in the terminal window chrome. External
//! consumers (e.g. the niri workspace sorter / waybar ticker) read these files
//! instead of reverse-engineering session state from streaming markers, todo
//! files, and session JSON, which kept breaking whenever jcode's internals
//! changed.
//!
//! This module is the low-level file I/O only: deciding *which* state a
//! session is in requires todo/title knowledge and lives in
//! `jcode-base::session_status`. Files are cleaned up alongside the
//! active-pid registry entry in [`crate::unregister_active_pid`].

use crate::jcode_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// UI-facing session state. Mirrors what presence UIs (menu bar, waybar
/// ticker, workspace sorter) want to display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionUiState {
    /// Actively streaming a model response.
    Running,
    /// Idle with incomplete (or no) work: waiting on the user.
    Waiting,
    /// Idle and every todo in the plan is completed.
    Finished,
    /// Just spawned: no real user input yet.
    Fresh,
}

impl SessionUiState {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionUiState::Running => "running",
            SessionUiState::Waiting => "waiting",
            SessionUiState::Finished => "finished",
            SessionUiState::Fresh => "fresh",
        }
    }
}

/// On-disk schema for one session status file. Versioned so external readers
/// can evolve alongside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUiStatus {
    /// Schema version, currently 1.
    pub v: u32,
    pub state: SessionUiState,
    /// Display title as shown in the terminal window chrome (untruncated),
    /// when one exists. External readers should mirror jcode's window-title
    /// truncation if they match against window titles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// PID of the process that wrote this status (server or local client).
    /// Not necessarily inside the terminal window that displays the session.
    pub pid: u32,
    /// Milliseconds since the Unix epoch at write time.
    pub updated_at_ms: u64,
}

/// Directory holding one status file per session
/// (`~/.jcode/session_status/<session_id>.json`).
pub fn session_status_dir() -> Option<PathBuf> {
    jcode_dir().ok().map(|d| d.join("session_status"))
}

fn status_path(session_id: &str) -> Option<PathBuf> {
    session_status_dir().map(|d| d.join(format!("{session_id}.json")))
}

/// Write (or overwrite) the status file for `session_id`. Best-effort: status
/// files are advisory presence state, never load-bearing.
pub fn write_session_ui_status(session_id: &str, state: SessionUiState, title: Option<String>) {
    let Some(path) = status_path(session_id) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let status = SessionUiStatus {
        v: 1,
        state,
        title,
        pid: std::process::id(),
        updated_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };
    let _ = crate::write_json_fast(&path, &status);
}

/// Read the status file for `session_id`, if present and parseable.
pub fn read_session_ui_status(session_id: &str) -> Option<SessionUiStatus> {
    crate::read_json(&status_path(session_id)?).ok()
}

/// Remove the status file for `session_id`, if present, along with the
/// `.bak` sibling the atomic writer leaves behind.
pub fn clear_session_ui_status(session_id: &str) {
    if let Some(path) = status_path(session_id) {
        let _ = std::fs::remove_file(path.with_extension("bak"));
        let _ = std::fs::remove_file(path);
    }
}

/// Prune status files whose session is no longer live (no active-pid entry,
/// or the recorded owner process is gone). Sessions killed without a clean
/// shutdown (SIGKILL, power loss) never reach `unregister_active_pid`, so
/// their status files would otherwise accumulate forever. Called on session
/// activation; the directory only ever holds a handful of small files.
pub fn prune_stale_session_ui_status() {
    let Some(dir) = session_status_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let active_dir = crate::active_pids_dir();
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let Some(session_id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            // `.bak` siblings are removed together with their primary below;
            // orphaned ones (primary already gone) are stale by definition.
            if let Some(session_id) = name.to_str().and_then(|n| n.strip_suffix(".bak")) {
                if !dir.join(format!("{session_id}.json")).exists() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            continue;
        };
        let live = active_dir
            .as_ref()
            .and_then(|d| std::fs::read_to_string(d.join(session_id)).ok())
            .and_then(|raw| raw.trim().parse::<u32>().ok())
            .is_some_and(crate::active_pids::process_is_running_crate);
        if !live {
            clear_session_ui_status(session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that mutate `JCODE_HOME` (crate-wide lock).
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        crate::lock_test_env_crate()
    }

    #[test]
    fn write_read_clear_roundtrip() {
        let _guard = lock_env();
        let temp = tempfile::tempdir().expect("tempdir");
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        assert!(read_session_ui_status("session_x").is_none());
        write_session_ui_status(
            "session_x",
            SessionUiState::Running,
            Some("~/git/demo".to_string()),
        );
        let status = read_session_ui_status("session_x").expect("status present");
        assert_eq!(status.v, 1);
        assert_eq!(status.state, SessionUiState::Running);
        assert_eq!(status.title.as_deref(), Some("~/git/demo"));
        assert_eq!(status.pid, std::process::id());
        assert!(status.updated_at_ms > 0);

        write_session_ui_status("session_x", SessionUiState::Finished, None);
        let status = read_session_ui_status("session_x").expect("status present");
        assert_eq!(status.state, SessionUiState::Finished);
        assert_eq!(status.title, None);

        clear_session_ui_status("session_x");
        assert!(read_session_ui_status("session_x").is_none());

        jcode_core::env::remove_var("JCODE_HOME");
    }

    #[test]
    fn prune_drops_dead_sessions_keeps_live_ones() {
        let _guard = lock_env();
        let temp = tempfile::tempdir().expect("tempdir");
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        // Live session: active-pid entry pointing at this process.
        crate::register_active_pid("session_live", std::process::id());
        write_session_ui_status("session_live", SessionUiState::Waiting, None);

        // Dead session: active-pid entry pointing at a dead pid.
        crate::register_active_pid("session_dead", 999_999_999);
        write_session_ui_status("session_dead", SessionUiState::Running, None);

        // Orphan: status file with no active-pid entry at all, plus a stray
        // .bak with no primary.
        write_session_ui_status("session_orphan", SessionUiState::Finished, None);
        let dir = session_status_dir().expect("dir");
        std::fs::write(dir.join("session_ghost.bak"), b"{}").expect("write bak");

        prune_stale_session_ui_status();

        assert!(read_session_ui_status("session_live").is_some());
        assert!(read_session_ui_status("session_dead").is_none());
        assert!(read_session_ui_status("session_orphan").is_none());
        assert!(!dir.join("session_ghost.bak").exists());

        crate::unregister_active_pid("session_live");
        crate::unregister_active_pid("session_dead");
        jcode_core::env::remove_var("JCODE_HOME");
    }
}
