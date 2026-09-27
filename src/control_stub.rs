//! Test-only `MultiplexerControl` stubs shared across modules.
//!
//! [`Unreachable`] was born inside `picker::reconcile`'s tests; the session
//! picker's delete and the window's displayed-pane decision need the same
//! "daemon went away" backend, so it lives here once rather than three times.

use tear_types::{MultiplexerControl, SessionId, SessionSource};

/// A backend that answers every call with a transport error — a daemon that
/// has gone away mid-session. The shape every BLIND-is-not-EMPTY test needs.
pub(crate) struct Unreachable;

macro_rules! unreachable_backend {
    ($( fn $name:ident(&self $(, $arg:ident : $ty:ty)* ) -> $ret:ty; )*) => {
        impl MultiplexerControl for Unreachable {
            $( fn $name(&self $(, $arg: $ty)*) -> $ret {
                $( let _ = $arg; )*
                Err(tear_types::ControlError::Transport("daemon unreachable".into()))
            } )*
        }
    };
}

unreachable_backend! {
    fn list_sessions(&self) -> tear_types::ControlResult<Vec<tear_types::TearSession>>;
    fn get_session(&self, id: SessionId) -> tear_types::ControlResult<tear_types::TearSession>;
    fn get_window(&self, id: tear_types::WindowId) -> tear_types::ControlResult<(SessionId, tear_types::TearWindow)>;
    fn get_pane(&self, id: tear_types::PaneId) -> tear_types::ControlResult<tear_types::TearPane>;
    fn new_session_with_source_and_size(&self, name: &str, shell: &str, args: &[String], source: SessionSource, size: (u16, u16)) -> tear_types::ControlResult<SessionId>;
    fn rename_session(&self, id: SessionId, new_name: &str) -> tear_types::ControlResult<()>;
    fn kill_session(&self, id: SessionId) -> tear_types::ControlResult<()>;
    fn new_window(&self, session: SessionId, name: &str, shell: &str, args: &[String]) -> tear_types::ControlResult<tear_types::WindowId>;
    fn kill_window(&self, id: tear_types::WindowId) -> tear_types::ControlResult<()>;
    fn select_window(&self, id: tear_types::WindowId) -> tear_types::ControlResult<()>;
    fn split_pane(&self, origin: tear_types::PaneId, direction: tear_types::Direction, shell: &str, args: &[String]) -> tear_types::ControlResult<tear_types::PaneId>;
    fn kill_pane(&self, id: tear_types::PaneId) -> tear_types::ControlResult<()>;
    fn select_pane(&self, id: tear_types::PaneId) -> tear_types::ControlResult<()>;
    fn resize_pane(&self, id: tear_types::PaneId, direction: tear_types::Direction, delta: i16) -> tear_types::ControlResult<()>;
    fn apply_layout(&self, window: tear_types::WindowId, kind: tear_types::LayoutKind) -> tear_types::ControlResult<()>;
    fn send_keys(&self, id: tear_types::PaneId, bytes: &[u8]) -> tear_types::ControlResult<()>;
    fn pane_subscriber_count(&self, id: tear_types::PaneId) -> tear_types::ControlResult<u32>;
    fn set_input_policy(&self, id: tear_types::PaneId, policy: tear_types::InputPolicy) -> tear_types::ControlResult<()>;
}
