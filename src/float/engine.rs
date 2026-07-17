//! The browser-engine seam — the ONE injectable trait ([`BrowserBackend`]) that
//! keeps the whole floating-browser control + geometry stack pure and
//! engine-agnostic (TESTING-SUBSTRATE §IX: "the seam is the whole game").
//!
//! The concrete engine (destination: nami-core's `BrowserEngine` +
//! `NamiNativeEngine`, wired at L1/M1; servo/CDP later) lives *behind* this
//! trait. The control logic — nav state, redundant-load coalescing — is pure
//! over the trait ([`NavControl`]) and is proven against [`MockBrowserBackend`]
//! with zero real I/O.
//!
//! Tier-honesty is baked into the type: an operation an engine cannot perform
//! returns a typed [`BackendError::Unsupported`] — **never a silent `Ok`, no
//! `todo!()`/`panic!()`** (the TYPED-SPEC triplet stub rule). nami-native has no
//! JS engine, so `eval` on it will be a *visible* `Unsupported`, not a lie.
//!
//! `render_to_view` (the zero-copy GPU composite into a mado-owned
//! `wgpu::TextureView`) is deliberately **absent** from this M0 trait — it
//! arrives at L2/M1 with the real GPU wiring, so the pure core takes no wgpu
//! dependency. M0 exposes only the engine-agnostic control surface.

#[cfg(test)]
use std::sync::Mutex;

use url::Url;

use pleme_allvariants_derive::AllVariants;
use pleme_kindstr_derive::KindStr;

/// The page load state a [`BrowserBackend`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, KindStr, AllVariants)]
pub enum LoadState {
    #[kind(name = "idle")]
    Idle,
    #[kind(name = "loading")]
    Loading,
    #[kind(name = "loaded")]
    Loaded,
    #[kind(name = "failed")]
    Failed,
}

/// A page's rendered pixels + geometry — the readback / golden-snapshot frame
/// (the live GPU path composites zero-copy at L2 instead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Tightly-packed RGBA8 (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
}

impl RenderedFrame {
    /// Whether `rgba` has exactly `width * height * 4` bytes.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.rgba.len() == self.width as usize * self.height as usize * 4
    }
}

/// A typed backend failure — the ABSENCE of a silent path is what this enum
/// buys. Every unsupported / failed op is *visible*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// The engine cannot perform this op at all (e.g. nami-native has no JS).
    Unsupported(&'static str),
    /// A navigation or load failed.
    Load(String),
    /// The op needs a live GPU surface/view not present in this context.
    NoSurface,
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Unsupported(op) => write!(f, "backend does not support {op}"),
            BackendError::Load(msg) => write!(f, "load failed: {msg}"),
            BackendError::NoSurface => write!(f, "no render surface available"),
        }
    }
}

impl std::error::Error for BackendError {}

/// The injectable browser engine. Everything above it (geometry, focus, control)
/// is pure; everything below it (a real engine's I/O) is mocked in tests.
///
/// M0 is the engine-agnostic control surface. `render_to_view` (the GPU
/// composite) lands at L2/M1 with the wgpu wiring.
pub trait BrowserBackend {
    /// Navigate to a *parsed, valid* URL (a malformed link is parse-time
    /// rejected by [`url::Url`] before it reaches here).
    fn navigate(&mut self, url: &Url) -> Result<(), BackendError>;

    /// Resize the page viewport (CSS px).
    fn resize(&mut self, width: u32, height: u32);

    /// The current page load state.
    fn load_state(&self) -> LoadState;

    /// Evaluate JavaScript. Engines without a JS runtime (nami-native) return
    /// [`BackendError::Unsupported`] — a visible gap, never a silent `Ok`.
    fn eval(&mut self, script: &str) -> Result<String, BackendError>;

    /// Render the current page to an RGBA frame (the readback / golden path).
    fn screenshot(&mut self) -> Result<RenderedFrame, BackendError>;

    /// Monotonic seconds from the render clock (never `Instant::now`) — the
    /// determinism contract every animation obeys.
    fn now(&self) -> f64;
}

/// Pure browser-control logic over a [`BrowserBackend`]: owns the nav URL + load
/// state and coalesces a redundant navigation to the current URL. Proven against
/// [`MockBrowserBackend`] with zero real I/O.
#[derive(Debug, Default, Clone)]
pub struct NavControl {
    current: Option<Url>,
    state: LoadState,
}

impl Default for LoadState {
    fn default() -> Self {
        LoadState::Idle
    }
}

impl NavControl {
    /// A fresh idle controller.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: None,
            state: LoadState::Idle,
        }
    }

    /// The current URL, if any.
    #[must_use]
    pub fn current(&self) -> Option<&Url> {
        self.current.as_ref()
    }

    /// The last observed load state.
    #[must_use]
    pub fn state(&self) -> LoadState {
        self.state
    }

    /// Navigate `backend` to `url`, coalescing a redundant navigation to the
    /// same URL (unless the last load failed). Returns `Ok(true)` if a real
    /// navigation happened, `Ok(false)` if it was coalesced.
    pub fn navigate<B: BrowserBackend>(
        &mut self,
        backend: &mut B,
        url: Url,
    ) -> Result<bool, BackendError> {
        if self.current.as_ref() == Some(&url) && self.state != LoadState::Failed {
            return Ok(false);
        }
        backend.navigate(&url)?;
        self.current = Some(url);
        self.state = backend.load_state();
        Ok(true)
    }

    /// Reload the current URL. Errors if there is no current URL.
    pub fn refresh<B: BrowserBackend>(&mut self, backend: &mut B) -> Result<(), BackendError> {
        let Some(url) = self.current.clone() else {
            return Err(BackendError::Load("no current url to refresh".to_owned()));
        };
        backend.navigate(&url)?;
        self.state = backend.load_state();
        Ok(())
    }
}

/// A recording, canned-response [`BrowserBackend`] for tests — the
/// `MockJanitorEnv` shape (calls recorded in a `Mutex`, responses canned). Gated
/// `#[cfg(test)]` per mado's inline-mock convention (no `mock` feature needed).
#[cfg(test)]
pub(crate) struct MockBrowserBackend {
    pub nav_calls: Mutex<Vec<String>>,
    pub eval_calls: Mutex<Vec<String>>,
    pub resizes: Mutex<Vec<(u32, u32)>>,
    pub load_state: LoadState,
    pub next_frame: RenderedFrame,
    pub fail_navigate: bool,
    pub supports_js: bool,
    pub clock: f64,
}

#[cfg(test)]
impl MockBrowserBackend {
    pub fn new() -> Self {
        Self {
            nav_calls: Mutex::new(Vec::new()),
            eval_calls: Mutex::new(Vec::new()),
            resizes: Mutex::new(Vec::new()),
            load_state: LoadState::Loaded,
            next_frame: RenderedFrame {
                width: 2,
                height: 1,
                rgba: vec![0, 0, 0, 255, 255, 255, 255, 255],
            },
            fail_navigate: false,
            supports_js: false,
            clock: 0.0,
        }
    }
}

#[cfg(test)]
impl BrowserBackend for MockBrowserBackend {
    fn navigate(&mut self, url: &Url) -> Result<(), BackendError> {
        self.nav_calls.lock().unwrap().push(url.to_string());
        if self.fail_navigate {
            self.load_state = LoadState::Failed;
            return Err(BackendError::Load(url.to_string()));
        }
        self.load_state = LoadState::Loaded;
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.resizes.lock().unwrap().push((width, height));
    }

    fn load_state(&self) -> LoadState {
        self.load_state
    }

    fn eval(&mut self, script: &str) -> Result<String, BackendError> {
        self.eval_calls.lock().unwrap().push(script.to_owned());
        if self.supports_js {
            Ok(String::from("null"))
        } else {
            Err(BackendError::Unsupported("javascript eval"))
        }
    }

    fn screenshot(&mut self) -> Result<RenderedFrame, BackendError> {
        Ok(self.next_frame.clone())
    }

    fn now(&self) -> f64 {
        self.clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_state_slugs_round_trip_and_are_complete() {
        assert_eq!(LoadState::ALL.len(), 4);
        for s in LoadState::ALL {
            assert_eq!(LoadState::from_str_kind(s.as_str()), Some(*s));
        }
    }

    #[test]
    fn navigate_records_the_url_and_advances_state() {
        let mut backend = MockBrowserBackend::new();
        let mut nav = NavControl::new();
        let url = Url::parse("https://example.com/").unwrap();
        assert_eq!(nav.navigate(&mut backend, url.clone()), Ok(true));
        assert_eq!(nav.current().map(Url::as_str), Some("https://example.com/"));
        assert_eq!(nav.state(), LoadState::Loaded);
        assert_eq!(backend.nav_calls.lock().unwrap().as_slice(), &["https://example.com/".to_owned()]);
    }

    #[test]
    fn a_redundant_navigation_is_coalesced() {
        let mut backend = MockBrowserBackend::new();
        let mut nav = NavControl::new();
        let url = Url::parse("https://example.com/").unwrap();
        assert_eq!(nav.navigate(&mut backend, url.clone()), Ok(true));
        // Same URL again → coalesced (no second backend call).
        assert_eq!(nav.navigate(&mut backend, url), Ok(false));
        assert_eq!(backend.nav_calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_navigation_is_a_typed_error_not_a_panic() {
        let mut backend = MockBrowserBackend::new();
        backend.fail_navigate = true;
        let mut nav = NavControl::new();
        let url = Url::parse("https://nope.example/").unwrap();
        let err = nav.navigate(&mut backend, url).unwrap_err();
        assert!(matches!(err, BackendError::Load(_)));
        assert_eq!(nav.state(), LoadState::Idle); // control state not advanced on failure
    }

    #[test]
    fn eval_on_a_js_less_engine_is_unsupported_never_silent() {
        let mut backend = MockBrowserBackend::new(); // supports_js = false
        let err = backend.eval("1 + 1").unwrap_err();
        assert_eq!(err, BackendError::Unsupported("javascript eval"));
        // The call was still recorded (visible), not swallowed.
        assert_eq!(backend.eval_calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn screenshot_returns_a_well_formed_canned_frame() {
        let mut backend = MockBrowserBackend::new();
        let frame = backend.screenshot().unwrap();
        assert!(frame.is_well_formed());
        assert_eq!((frame.width, frame.height), (2, 1));
    }

    #[test]
    fn refresh_without_a_current_url_is_a_typed_error() {
        let mut backend = MockBrowserBackend::new();
        let mut nav = NavControl::new();
        assert!(matches!(nav.refresh(&mut backend), Err(BackendError::Load(_))));
    }

    #[test]
    fn backend_error_display_is_stable() {
        assert_eq!(
            BackendError::Unsupported("javascript eval").to_string(),
            "backend does not support javascript eval",
        );
        assert_eq!(BackendError::NoSurface.to_string(), "no render surface available");
    }
}
