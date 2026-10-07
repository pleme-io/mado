use std::sync::Arc;

use tear_types::{
    ControlResult, DaemonIdentity, Direction, InputPolicy, LayoutKind, MultiplexerControl, PaneId,
    PaneSnapshot, SessionId, SessionSource, SpawnEnv, TearPane, TearSession, TearWindow, WindowId,
};

use super::{TearCall, TearCalls, UI_TEAR_CALLS, count_tear_call};

pub struct Counted<C: ?Sized> {
    inner: Arc<C>,
    calls: &'static TearCalls,
}

impl<C: ?Sized> Counted<C> {
    #[must_use]
    pub fn new(inner: Arc<C>) -> Self {
        Self::counting_into(inner, &UI_TEAR_CALLS)
    }

    #[must_use]
    pub fn counting_into(inner: Arc<C>, calls: &'static TearCalls) -> Self {
        Self { inner, calls }
    }

    fn count(&self, call: TearCall) {
        count_tear_call(self.calls, call);
    }
}

impl<C: ?Sized> Clone for Counted<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            calls: self.calls,
        }
    }
}

impl<C: MultiplexerControl + 'static> Counted<C> {
    #[must_use]
    pub fn as_dyn(&self) -> Counted<dyn MultiplexerControl> {
        Counted {
            inner: Arc::clone(&self.inner) as Arc<dyn MultiplexerControl>,
            calls: self.calls,
        }
    }
}

impl Counted<tear_core::InProcess> {
    pub fn set_spawn_env(&self, env: SpawnEnv) {
        self.count(TearCall::SetSpawnEnv);
        self.inner.set_spawn_env(env);
    }

    pub fn with_registry<R>(&self, f: impl FnOnce(&tear_core::registry::Registry) -> R) -> R {
        self.count(TearCall::WithRegistry);
        self.inner.with_registry(f)
    }

    #[must_use]
    pub fn producer(&self, pane: PaneId) -> tear_core::engate_producer::PaneProducer {
        tear_core::engate_producer::PaneProducer::new(Arc::clone(&self.inner), pane)
    }
}

impl Counted<tear_client::Client> {
    pub fn set_spawn_env(&self, env: &SpawnEnv) -> ControlResult<()> {
        self.count(TearCall::SetSpawnEnv);
        self.inner.set_spawn_env(env)
    }

    pub fn get_config(&self) -> ControlResult<tear_config::TearConfig> {
        self.count(TearCall::GetConfig);
        self.inner.get_config()
    }

    pub fn set_config(&self, cfg: &tear_config::TearConfig) -> ControlResult<()> {
        self.count(TearCall::SetConfig);
        self.inner.set_config(cfg)
    }

    #[must_use]
    pub fn producer(&self, pane: PaneId) -> tear_client::engate_producer::PaneProducer {
        tear_client::engate_producer::PaneProducer::new(Arc::clone(&self.inner), pane)
    }
}

macro_rules! counted_control {
    ($($call:ident => fn $method:ident(&self $(, $arg:ident: $ty:ty)*) -> $ret:ty;)+) => {
        impl<C: MultiplexerControl + ?Sized> MultiplexerControl for Counted<C> {
            $(
                fn $method(&self $(, $arg: $ty)*) -> $ret {
                    self.count(TearCall::$call);
                    self.inner.$method($($arg),*)
                }
            )+
        }
    };
}

counted_control! {
    Capabilities => fn capabilities(&self) -> DaemonIdentity;
    ListSessions => fn list_sessions(&self) -> ControlResult<Vec<TearSession>>;
    GetSession => fn get_session(&self, id: SessionId) -> ControlResult<TearSession>;
    GetWindow => fn get_window(&self, id: WindowId) -> ControlResult<(SessionId, TearWindow)>;
    GetPane => fn get_pane(&self, id: PaneId) -> ControlResult<TearPane>;
    NewSession => fn new_session(&self, name: &str, shell: &str) -> ControlResult<SessionId>;
    NewSessionWithSource => fn new_session_with_source(
        &self,
        name: &str,
        shell: &str,
        source: SessionSource
    ) -> ControlResult<SessionId>;
    NewSessionWithSourceAndSize => fn new_session_with_source_and_size(
        &self,
        name: &str,
        shell: &str,
        args: &[String],
        source: SessionSource,
        size_cells: (u16, u16)
    ) -> ControlResult<SessionId>;
    NewSessionIn => fn new_session_in(
        &self,
        name: &str,
        shell: &str,
        args: &[String],
        source: SessionSource,
        size_cells: (u16, u16),
        env: &SpawnEnv
    ) -> ControlResult<SessionId>;
    RenameSession => fn rename_session(&self, id: SessionId, new_name: &str) -> ControlResult<()>;
    KillSession => fn kill_session(&self, id: SessionId) -> ControlResult<()>;
    NewWindow => fn new_window(
        &self,
        session: SessionId,
        name: &str,
        shell: &str,
        args: &[String]
    ) -> ControlResult<WindowId>;
    KillWindow => fn kill_window(&self, id: WindowId) -> ControlResult<()>;
    SelectWindow => fn select_window(&self, id: WindowId) -> ControlResult<()>;
    SplitPane => fn split_pane(
        &self,
        origin: PaneId,
        direction: Direction,
        shell: &str,
        args: &[String]
    ) -> ControlResult<PaneId>;
    KillPane => fn kill_pane(&self, id: PaneId) -> ControlResult<()>;
    SelectPane => fn select_pane(&self, id: PaneId) -> ControlResult<()>;
    ResizePane => fn resize_pane(
        &self,
        id: PaneId,
        direction: Direction,
        delta_cells: i16
    ) -> ControlResult<()>;
    ApplyLayout => fn apply_layout(&self, window: WindowId, kind: LayoutKind) -> ControlResult<()>;
    PaneResizeAbsolute => fn pane_resize_absolute(
        &self,
        id: PaneId,
        cols: u16,
        rows: u16
    ) -> ControlResult<()>;
    SendKeys => fn send_keys(&self, id: PaneId, bytes: &[u8]) -> ControlResult<()>;
    PaneSubscriberCount => fn pane_subscriber_count(&self, id: PaneId) -> ControlResult<u32>;
    SetInputPolicy => fn set_input_policy(&self, id: PaneId, policy: InputPolicy) -> ControlResult<()>;
    PaneSnapshot => fn pane_snapshot(&self, id: PaneId) -> ControlResult<PaneSnapshot>;
    PaneCursorKeysMode => fn pane_cursor_keys_mode(&self, id: PaneId) -> ControlResult<bool>;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use kanshou::metrics::{Family, Label};
    use tear_types::ControlError;

    use super::*;
    use crate::perf::UiThread;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<&'static str>>);

    impl Recorder {
        fn saw<T>(&self, method: &'static str) -> ControlResult<T> {
            self.0.lock().unwrap().push(method);
            Err(ControlError::Rejected(method.into()))
        }

        fn take(&self) -> Vec<&'static str> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    impl MultiplexerControl for Recorder {
        fn capabilities(&self) -> DaemonIdentity {
            let _: ControlResult<()> = self.saw("capabilities");
            DaemonIdentity::pre_capability()
        }
        fn list_sessions(&self) -> ControlResult<Vec<TearSession>> {
            self.saw("list_sessions")
        }
        fn get_session(&self, _: SessionId) -> ControlResult<TearSession> {
            self.saw("get_session")
        }
        fn get_window(&self, _: WindowId) -> ControlResult<(SessionId, TearWindow)> {
            self.saw("get_window")
        }
        fn get_pane(&self, _: PaneId) -> ControlResult<TearPane> {
            self.saw("get_pane")
        }
        fn new_session(&self, _: &str, _: &str) -> ControlResult<SessionId> {
            self.saw("new_session")
        }
        fn new_session_with_source(
            &self,
            _: &str,
            _: &str,
            _: SessionSource,
        ) -> ControlResult<SessionId> {
            self.saw("new_session_with_source")
        }
        fn new_session_with_source_and_size(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: SessionSource,
            _: (u16, u16),
        ) -> ControlResult<SessionId> {
            self.saw("new_session_with_source_and_size")
        }
        fn new_session_in(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: SessionSource,
            _: (u16, u16),
            _: &SpawnEnv,
        ) -> ControlResult<SessionId> {
            self.saw("new_session_in")
        }
        fn rename_session(&self, _: SessionId, _: &str) -> ControlResult<()> {
            self.saw("rename_session")
        }
        fn kill_session(&self, _: SessionId) -> ControlResult<()> {
            self.saw("kill_session")
        }
        fn new_window(
            &self,
            _: SessionId,
            _: &str,
            _: &str,
            _: &[String],
        ) -> ControlResult<WindowId> {
            self.saw("new_window")
        }
        fn kill_window(&self, _: WindowId) -> ControlResult<()> {
            self.saw("kill_window")
        }
        fn select_window(&self, _: WindowId) -> ControlResult<()> {
            self.saw("select_window")
        }
        fn split_pane(
            &self,
            _: PaneId,
            _: Direction,
            _: &str,
            _: &[String],
        ) -> ControlResult<PaneId> {
            self.saw("split_pane")
        }
        fn kill_pane(&self, _: PaneId) -> ControlResult<()> {
            self.saw("kill_pane")
        }
        fn select_pane(&self, _: PaneId) -> ControlResult<()> {
            self.saw("select_pane")
        }
        fn resize_pane(&self, _: PaneId, _: Direction, _: i16) -> ControlResult<()> {
            self.saw("resize_pane")
        }
        fn apply_layout(&self, _: WindowId, _: LayoutKind) -> ControlResult<()> {
            self.saw("apply_layout")
        }
        fn pane_resize_absolute(&self, _: PaneId, _: u16, _: u16) -> ControlResult<()> {
            self.saw("pane_resize_absolute")
        }
        fn send_keys(&self, _: PaneId, _: &[u8]) -> ControlResult<()> {
            self.saw("send_keys")
        }
        fn pane_subscriber_count(&self, _: PaneId) -> ControlResult<u32> {
            self.saw("pane_subscriber_count")
        }
        fn set_input_policy(&self, _: PaneId, _: InputPolicy) -> ControlResult<()> {
            self.saw("set_input_policy")
        }
        fn pane_snapshot(&self, _: PaneId) -> ControlResult<PaneSnapshot> {
            self.saw("pane_snapshot")
        }
        fn pane_cursor_keys_mode(&self, _: PaneId) -> ControlResult<bool> {
            self.saw("pane_cursor_keys_mode")
        }
    }

    fn fresh_calls() -> &'static TearCalls {
        Box::leak(Box::new(Family::new()))
    }

    struct Daemon {
        client: Arc<tear_client::Client>,
        _dir: tempfile::TempDir,
    }

    fn isolated_daemon() -> Daemon {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("tear.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let inproc = Arc::new(tear_core::InProcess::new());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let inproc = Arc::clone(&inproc);
                std::thread::spawn(move || {
                    let _ = tear_daemon::serve_connection_full(
                        stream,
                        inproc,
                        Arc::new(tear_config::LiveConfig::default()),
                        None,
                        None,
                        None,
                    );
                });
            }
        });
        Daemon {
            client: Arc::new(tear_client::Client::connect(&socket).expect("connect")),
            _dir: dir,
        }
    }

    enum Through {
        Control(&'static str),
        Inherent,
        Producer,
    }

    fn call(
        label: TearCall,
        control: &dyn MultiplexerControl,
        inproc: &Counted<tear_core::InProcess>,
        client: &Counted<tear_client::Client>,
    ) -> Through {
        let p = PaneId(1);
        let s = SessionId(1);
        let w = WindowId(1);
        let src = || SessionSource::Named("t".into());
        let args: &[String] = &[];
        let env = SpawnEnv::none();
        match label {
            TearCall::Capabilities => drop(control.capabilities()),
            TearCall::ListSessions => drop(control.list_sessions()),
            TearCall::GetSession => drop(control.get_session(s)),
            TearCall::GetWindow => drop(control.get_window(w)),
            TearCall::GetPane => drop(control.get_pane(p)),
            TearCall::NewSession => drop(control.new_session("n", "sh")),
            TearCall::NewSessionWithSource => {
                drop(control.new_session_with_source("n", "sh", src()));
            }
            TearCall::NewSessionWithSourceAndSize => {
                drop(control.new_session_with_source_and_size("n", "sh", args, src(), (1, 1)));
            }
            TearCall::NewSessionIn => {
                drop(control.new_session_in("n", "sh", args, src(), (1, 1), &env));
            }
            TearCall::RenameSession => drop(control.rename_session(s, "m")),
            TearCall::KillSession => drop(control.kill_session(s)),
            TearCall::NewWindow => drop(control.new_window(s, "n", "sh", args)),
            TearCall::KillWindow => drop(control.kill_window(w)),
            TearCall::SelectWindow => drop(control.select_window(w)),
            TearCall::SplitPane => drop(control.split_pane(p, Direction::Right, "sh", args)),
            TearCall::KillPane => drop(control.kill_pane(p)),
            TearCall::SelectPane => drop(control.select_pane(p)),
            TearCall::ResizePane => drop(control.resize_pane(p, Direction::Right, 1)),
            TearCall::ApplyLayout => drop(control.apply_layout(w, LayoutKind::Tiled)),
            TearCall::PaneResizeAbsolute => drop(control.pane_resize_absolute(p, 1, 1)),
            TearCall::SendKeys => drop(control.send_keys(p, b"x")),
            TearCall::PaneSubscriberCount => drop(control.pane_subscriber_count(p)),
            TearCall::SetInputPolicy => drop(control.set_input_policy(p, InputPolicy::Free)),
            TearCall::PaneSnapshot => drop(control.pane_snapshot(p)),
            TearCall::PaneCursorKeysMode => drop(control.pane_cursor_keys_mode(p)),
            TearCall::SetSpawnEnv => {
                inproc.set_spawn_env(SpawnEnv::none());
                client
                    .set_spawn_env(&env)
                    .expect("the daemon takes a spawn env");
                return Through::Inherent;
            }
            TearCall::WithRegistry => {
                inproc.with_registry(|r| r.sessions.len());
                return Through::Inherent;
            }
            TearCall::GetConfig => {
                client.get_config().expect("the daemon answers its config");
                return Through::Inherent;
            }
            TearCall::SetConfig => {
                client
                    .set_config(&tear_config::TearConfig::default())
                    .expect("the daemon takes a config");
                return Through::Inherent;
            }
            TearCall::ProducerSnapshot | TearCall::ProducerSubscribe => return Through::Producer,
        }
        Through::Control(label.name())
    }

    #[test]
    fn every_tear_method_counts_once_under_its_own_label_and_reaches_the_backend() {
        let _ui = UiThread::mark();
        let calls = fresh_calls();
        let recorder = Arc::new(Recorder::default());
        let control = Counted::counting_into(Arc::clone(&recorder), calls);
        let inproc = Counted::counting_into(Arc::new(tear_core::InProcess::new()), calls);
        let daemon = isolated_daemon();
        let client = Counted::counting_into(Arc::clone(&daemon.client), calls);
        for &label in TearCall::ALL {
            let before: Vec<u64> = TearCall::ALL.iter().map(|&l| calls.get(l)).collect();
            let expected = match call(label, &control, &inproc, &client) {
                Through::Control(method) => {
                    assert_eq!(
                        recorder.take(),
                        vec![method],
                        "{method} through Counted reached a different backend method"
                    );
                    1
                }
                Through::Inherent if label == TearCall::SetSpawnEnv => 2,
                Through::Inherent => 1,
                Through::Producer => 0,
            };
            for (i, &other) in TearCall::ALL.iter().enumerate() {
                let moved = calls.get(other) - before[i];
                let want = if other == label { expected } else { 0 };
                assert_eq!(
                    moved,
                    want,
                    "calling {} moved {} by {moved}",
                    label.name(),
                    other.name()
                );
            }
        }
    }

    #[test]
    fn a_counted_handle_counts_nothing_off_the_ui_thread() {
        let calls = fresh_calls();
        let control = Counted::counting_into(Arc::new(Recorder::default()), calls);
        std::thread::spawn(move || {
            let _ = control.list_sessions();
            let _ = control.send_keys(PaneId(1), b"x");
        })
        .join()
        .unwrap();
        assert_eq!(calls.total(), 0);
    }

    #[test]
    fn a_dyn_view_shares_the_backend_and_counts_into_the_same_family() {
        let _ui = UiThread::mark();
        let calls = fresh_calls();
        let recorder = Arc::new(Recorder::default());
        let typed = Counted::counting_into(Arc::clone(&recorder), calls);
        let view: Arc<dyn MultiplexerControl> = Arc::new(typed.as_dyn());
        let _ = view.get_pane(PaneId(1));
        assert_eq!(recorder.take(), vec!["get_pane"]);
        assert_eq!(calls.get(TearCall::GetPane), 1);
        assert_eq!(
            calls.total(),
            1,
            "a dyn view counts once, not once per layer"
        );
    }
}
