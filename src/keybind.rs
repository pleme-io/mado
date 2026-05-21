//! Keybinding system — configurable key -> action mapping via awase.
//!
//! Uses `awase::Hotkey` and `awase::Binding` for key binding definitions,
//! providing a consistent hotkey representation across pleme-io apps.
//! Default keybindings use macOS-style Cmd as the primary modifier.

use serde::{Deserialize, Serialize};

/// High-level terminal actions triggered by keybindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Copy,
    Paste,
    PasteFromSelection,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    JumpToPrompt,
    JumpToPromptPrev,
    JumpToPromptNext,
    SearchOpen,
    SearchClose,
    SearchNext,
    SearchPrev,
    FontIncrease,
    FontDecrease,
    FontReset,
    // NewTab / CloseTab / NextTab / PrevTab / SplitHorizontal /
    // SplitVertical / FocusNext / FocusPrev / ClosePane removed
    // at Phase 4 — multiplexing lives in tear, not mado. See
    // theory/MADO-TEAR-M5.md. Users who want splits / tabs run
    // tear inside mado (or use `mado tear-attach --gpu`).
    ResetTerminal,
    ClearScreen,
    ToggleFullscreen,
    SelectAll,
    CopyUrlToClipboard,
    ToggleMouseReporting,
}

/// A keybinding mapping an awase hotkey to a mado action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybinding {
    pub hotkey: awase::Hotkey,
    pub action: Action,
}

/// Keybinding manager with lookup.
pub struct KeybindManager {
    bindings: Vec<Keybinding>,
}

#[allow(dead_code)]
impl KeybindManager {
    /// Create with platform-appropriate default bindings.
    /// Construct an EMPTY KeybindManager — zero bindings, every
    /// keystroke falls through to the PTY by default. Operator
    /// principle: nothing is bound until explicitly opted in.
    ///
    /// For mado's curated baseline (Cmd-+/Cmd--/Cmd-0/Cmd-C/...)
    /// call [`KeybindManager::with_mado_defaults`] instead. That
    /// constructor IS the documented "default mado experience" —
    /// `new()` is for headless paths, tests, and operators who
    /// want to start from scratch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bindings: Vec::new(),
        }
    }

    /// Construct a KeybindManager pre-populated with mado's curated
    /// baseline (`default_bindings()`). This is what `main.rs` +
    /// `gui_tear_attach.rs` call so operators get a useful first-
    /// launch experience without writing a yaml file.
    ///
    /// Each binding can be removed via [`KeybindManager::unbind`]
    /// or replaced via [`KeybindManager::bind`]; the operator-
    /// visible set is `manager.bindings()`.
    #[must_use]
    pub fn with_mado_defaults() -> Self {
        Self {
            bindings: default_bindings(),
        }
    }

    /// Look up an action for the given awase hotkey.
    #[must_use]
    pub fn lookup(&self, hotkey: &awase::Hotkey) -> Option<Action> {
        self.bindings
            .iter()
            .find(|b| b.hotkey == *hotkey)
            .map(|b| b.action)
    }

    /// Look up an action using awase key + modifier components.
    #[must_use]
    pub fn lookup_key(&self, key: awase::Key, modifiers: awase::Modifiers) -> Option<Action> {
        let hotkey = awase::Hotkey::new(modifiers, key);
        self.lookup(&hotkey)
    }

    /// Add or replace a keybinding using an awase hotkey.
    pub fn bind(&mut self, hotkey: awase::Hotkey, action: Action) {
        self.bindings.retain(|b| b.hotkey != hotkey);
        self.bindings.push(Keybinding { hotkey, action });
    }

    /// Add or replace a keybinding parsed from a string (e.g., "cmd+c").
    pub fn bind_str(&mut self, hotkey_str: &str, action: Action) -> Result<(), awase::AwaseError> {
        let hotkey = awase::Hotkey::parse(hotkey_str)?;
        self.bind(hotkey, action);
        Ok(())
    }

    /// Remove a keybinding for the given hotkey.
    pub fn unbind(&mut self, hotkey: &awase::Hotkey) {
        self.bindings.retain(|b| b.hotkey != *hotkey);
    }

    /// All current bindings.
    #[must_use]
    pub fn bindings(&self) -> &[Keybinding] {
        &self.bindings
    }

    /// Convenience: look up an action directly from a madori
    /// KeyEvent. Encapsulates the modifier → awase + key → awase
    /// conversion so the tear-attach event loop (which doesn't
    /// pull main.rs's helpers) can dispatch keybindings the same
    /// way the local-PTY path does.
    ///
    /// Returns `None` when the key + modifiers don't match a
    /// bound action — caller falls back to forwarding the text
    /// to the PTY (or dropping it if a non-text modifier is held).
    #[must_use]
    pub fn lookup_madori(&self, event: &madori::event::KeyEvent) -> Option<Action> {
        let key = madori_key_to_awase(&event.key, &event.text)?;
        let mods = madori_modifiers_to_awase(event.modifiers);
        self.lookup(&awase::Hotkey::new(mods, key))
    }
}

/// Convert a madori::Modifiers to awase::Modifiers. Shared by
/// main.rs (local-PTY path) and gui_tear_attach.rs (tear path)
/// so both use the same modifier rules. `meta` (winit's term for
/// macOS Cmd) maps to `awase::Modifiers::CMD`.
#[must_use]
pub fn madori_modifiers_to_awase(m: madori::event::Modifiers) -> awase::Modifiers {
    let mut out = awase::Modifiers::NONE;
    if m.ctrl {
        out = out | awase::Modifiers::CTRL;
    }
    if m.alt {
        out = out | awase::Modifiers::ALT;
    }
    if m.shift {
        out = out | awase::Modifiers::SHIFT;
    }
    if m.meta {
        out = out | awase::Modifiers::CMD;
    }
    out
}

/// Convert a madori::KeyCode + optional text into an awase::Key.
/// Returns None for keys with no awase equivalent (e.g. unknown
/// scancode, multi-char text).
#[must_use]
pub fn madori_key_to_awase(
    key: &madori::event::KeyCode,
    text: &Option<String>,
) -> Option<awase::Key> {
    match key {
        madori::event::KeyCode::Enter => Some(awase::Key::Return),
        madori::event::KeyCode::Escape => Some(awase::Key::Escape),
        madori::event::KeyCode::Tab => Some(awase::Key::Tab),
        madori::event::KeyCode::Backspace => Some(awase::Key::Backspace),
        madori::event::KeyCode::Delete => Some(awase::Key::Delete),
        madori::event::KeyCode::Home => Some(awase::Key::Home),
        madori::event::KeyCode::End => Some(awase::Key::End),
        madori::event::KeyCode::PageUp => Some(awase::Key::PageUp),
        madori::event::KeyCode::PageDown => Some(awase::Key::PageDown),
        madori::event::KeyCode::Up => Some(awase::Key::Up),
        madori::event::KeyCode::Down => Some(awase::Key::Down),
        madori::event::KeyCode::Left => Some(awase::Key::Left),
        madori::event::KeyCode::Right => Some(awase::Key::Right),
        madori::event::KeyCode::F(n) => match n {
            1 => Some(awase::Key::F1),
            2 => Some(awase::Key::F2),
            3 => Some(awase::Key::F3),
            4 => Some(awase::Key::F4),
            5 => Some(awase::Key::F5),
            6 => Some(awase::Key::F6),
            7 => Some(awase::Key::F7),
            8 => Some(awase::Key::F8),
            9 => Some(awase::Key::F9),
            10 => Some(awase::Key::F10),
            11 => Some(awase::Key::F11),
            12 => Some(awase::Key::F12),
            _ => None,
        },
        madori::event::KeyCode::Char(ch) => char_to_awase_key(*ch),
        madori::event::KeyCode::Space => Some(awase::Key::Space),
        _ => {
            // Try to extract from text (e.g. shifted symbols).
            if let Some(t) = text {
                if let Some(ch) = t.chars().next() {
                    if t.len() == ch.len_utf8() {
                        return char_to_awase_key(ch);
                    }
                }
            }
            None
        }
    }
}

/// Lowercase-folded char → awase::Key. Centralizes the
/// printable-to-key map both event paths use.
#[must_use]
pub fn char_to_awase_key(ch: char) -> Option<awase::Key> {
    match ch.to_ascii_lowercase() {
        'a' => Some(awase::Key::A),
        'b' => Some(awase::Key::B),
        'c' => Some(awase::Key::C),
        'd' => Some(awase::Key::D),
        'e' => Some(awase::Key::E),
        'f' => Some(awase::Key::F),
        'g' => Some(awase::Key::G),
        'h' => Some(awase::Key::H),
        'i' => Some(awase::Key::I),
        'j' => Some(awase::Key::J),
        'k' => Some(awase::Key::K),
        'l' => Some(awase::Key::L),
        'm' => Some(awase::Key::M),
        'n' => Some(awase::Key::N),
        'o' => Some(awase::Key::O),
        'p' => Some(awase::Key::P),
        'q' => Some(awase::Key::Q),
        'r' => Some(awase::Key::R),
        's' => Some(awase::Key::S),
        't' => Some(awase::Key::T),
        'u' => Some(awase::Key::U),
        'v' => Some(awase::Key::V),
        'w' => Some(awase::Key::W),
        'x' => Some(awase::Key::X),
        'y' => Some(awase::Key::Y),
        'z' => Some(awase::Key::Z),
        '0' => Some(awase::Key::Num0),
        '1' => Some(awase::Key::Num1),
        '2' => Some(awase::Key::Num2),
        '3' => Some(awase::Key::Num3),
        '4' => Some(awase::Key::Num4),
        '5' => Some(awase::Key::Num5),
        '6' => Some(awase::Key::Num6),
        '7' => Some(awase::Key::Num7),
        '8' => Some(awase::Key::Num8),
        '9' => Some(awase::Key::Num9),
        ' ' => Some(awase::Key::Space),
        '/' => Some(awase::Key::Slash),
        '+' | '=' => Some(awase::Key::Equal),
        '-' => Some(awase::Key::Minus),
        ',' => Some(awase::Key::Comma),
        '.' => Some(awase::Key::Period),
        _ => None,
    }
}

impl Default for KeybindManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an action name from config into an Action enum.
pub fn parse_action(name: &str) -> Option<Action> {
    match name {
        "copy" => Some(Action::Copy),
        "paste" => Some(Action::Paste),
        "paste_from_selection" => Some(Action::PasteFromSelection),
        "scroll_up" => Some(Action::ScrollUp),
        "scroll_down" => Some(Action::ScrollDown),
        "scroll_page_up" => Some(Action::ScrollPageUp),
        "scroll_page_down" => Some(Action::ScrollPageDown),
        "scroll_to_top" => Some(Action::ScrollToTop),
        "scroll_to_bottom" => Some(Action::ScrollToBottom),
        "jump_to_prompt" => Some(Action::JumpToPrompt),
        "jump_to_prompt:prev" | "jump_to_prompt_prev" | "jump_to_previous_prompt" => {
            Some(Action::JumpToPromptPrev)
        }
        "jump_to_prompt:next" | "jump_to_prompt_next" | "jump_to_next_prompt" => {
            Some(Action::JumpToPromptNext)
        }
        "search_open" | "search" => Some(Action::SearchOpen),
        "search_close" => Some(Action::SearchClose),
        "search_next" => Some(Action::SearchNext),
        "search_prev" => Some(Action::SearchPrev),
        "font_increase" | "increase_font_size" => Some(Action::FontIncrease),
        "font_decrease" | "decrease_font_size" => Some(Action::FontDecrease),
        "font_reset" | "reset_font_size" => Some(Action::FontReset),
        // Multiplexing actions removed at Phase 4 — tear's job now.
        "new_tab" | "close_tab" | "next_tab" | "prev_tab"
        | "split_horizontal" | "split_vertical"
        | "focus_next" | "focus_prev"
        | "close_pane" | "close_surface" => None,
        // goto_split:* also resolved to None above (Phase 4).
        "goto_split:next" | "goto_split:previous" => None,
        "reset_terminal" | "reset" => Some(Action::ResetTerminal),
        "clear_screen" => Some(Action::ClearScreen),
        "toggle_fullscreen" => Some(Action::ToggleFullscreen),
        "select_all" => Some(Action::SelectAll),
        "copy_url_to_clipboard" => Some(Action::CopyUrlToClipboard),
        "toggle_mouse_reporting" => Some(Action::ToggleMouseReporting),
        _ => None,
    }
}

/// Helper to create a binding from an awase hotkey.
fn hk(modifiers: awase::Modifiers, key: awase::Key) -> awase::Hotkey {
    awase::Hotkey::new(modifiers, key)
}

/// Default keybindings (macOS-style: Cmd as primary modifier).
fn default_bindings() -> Vec<Keybinding> {
    use awase::Key;
    use awase::Modifiers;

    // Atlas-resolved chords — every cross-GUI-terminal binding pulls
    // its chord from `ishou_tokens::FleetKeybinds::prescribed()` and
    // routes through `awase::Hotkey::parse_atlas_chord`. Drift in
    // either layer surfaces as a build-time `expect()` panic here,
    // and as a Guard test failure in `tests::mado_default_bindings_
    // converge_with_fleet_atlas`. The expect() messages name the
    // exact intent so an atlas change that breaks mado's parse is
    // immediately attributable.
    let kb = ishou_tokens::FleetKeybinds::prescribed();
    let atlas = |chord: &str, intent: &'static str| -> awase::Hotkey {
        awase::Hotkey::parse_atlas_chord(chord)
            .unwrap_or_else(|e| panic!("atlas chord {intent} = {chord:?} failed to parse: {e}"))
    };

    // App-specific chords NOT in the atlas (mado-only ergonomics +
    // OSC-133 prompt navigation + scroll keys). These stay
    // hand-wired until the atlas earns a third consumer for each.
    let cmd = Modifiers::CMD;
    let cmd_shift = Modifiers::CMD | Modifiers::SHIFT;
    let none = Modifiers::NONE;

    vec![
        // Clipboard — atlas-sourced.
        Keybinding { hotkey: atlas(kb.copy,  "copy"),  action: Action::Copy },
        Keybinding { hotkey: atlas(kb.paste, "paste"), action: Action::Paste },
        // Search — atlas-sourced.
        Keybinding { hotkey: atlas(kb.search_open,  "search_open"),  action: Action::SearchOpen },
        Keybinding { hotkey: atlas(kb.search_close, "search_close"), action: Action::SearchClose },
        Keybinding { hotkey: atlas(kb.search_next,  "search_next"),  action: Action::SearchNext },
        Keybinding { hotkey: atlas(kb.search_prev,  "search_prev"),  action: Action::SearchPrev },
        // Font — atlas-sourced.
        Keybinding { hotkey: atlas(kb.font_increase, "font_increase"), action: Action::FontIncrease },
        Keybinding { hotkey: atlas(kb.font_decrease, "font_decrease"), action: Action::FontDecrease },
        Keybinding { hotkey: atlas(kb.font_reset,    "font_reset"),    action: Action::FontReset },
        // Fullscreen — atlas-sourced.
        Keybinding { hotkey: atlas(kb.toggle_fullscreen, "toggle_fullscreen"), action: Action::ToggleFullscreen },
        // Scroll — mado-specific (no atlas intent yet).
        Keybinding { hotkey: hk(none, Key::PageUp),   action: Action::ScrollPageUp },
        Keybinding { hotkey: hk(none, Key::PageDown), action: Action::ScrollPageDown },
        Keybinding { hotkey: hk(cmd,  Key::Home),     action: Action::ScrollToTop },
        Keybinding { hotkey: hk(cmd,  Key::End),      action: Action::ScrollToBottom },
        // Prompt navigation — ghostty-canonical Cmd+Up / Cmd+Down on
        // OSC 133 prompt marks. Requires the shell integration scripts
        // (see shell-integration/mado.*) to be sourced. mado-specific.
        Keybinding { hotkey: hk(cmd, Key::Up),   action: Action::JumpToPromptPrev },
        Keybinding { hotkey: hk(cmd, Key::Down), action: Action::JumpToPromptNext },
        // Terminal — mado-specific (no atlas reset intent).
        Keybinding { hotkey: hk(cmd_shift, Key::R), action: Action::ResetTerminal },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        // Operator principle: zero bindings until explicitly opted in.
        let mgr = KeybindManager::new();
        assert!(mgr.bindings().is_empty());
    }

    #[test]
    fn with_mado_defaults_populates_baseline() {
        let mgr = KeybindManager::with_mado_defaults();
        assert!(!mgr.bindings().is_empty());
        // Sanity-check the canonical font-zoom triplet is present.
        let cmd = awase::Modifiers::CMD;
        assert_eq!(
            mgr.lookup(&awase::Hotkey::new(cmd, awase::Key::Equal)),
            Some(Action::FontIncrease)
        );
        assert_eq!(
            mgr.lookup(&awase::Hotkey::new(cmd, awase::Key::Minus)),
            Some(Action::FontDecrease)
        );
        assert_eq!(
            mgr.lookup(&awase::Hotkey::new(cmd, awase::Key::Num0)),
            Some(Action::FontReset)
        );
    }

    #[test]
    fn mado_default_bindings_converge_with_fleet_atlas() {
        // The 10 atlas-sourced chords in `default_bindings()` must
        // round-trip through `awase::Hotkey::parse_atlas_chord` and
        // match the chord declared in `ishou_tokens::FleetKeybinds::
        // prescribed()`. Drift in either layer (atlas changes a
        // chord, or mado's default_bindings starts hand-coding again)
        // fails this Guard chain loudly rather than at operator-press
        // time.
        let mgr = KeybindManager::with_mado_defaults();
        let kb = ishou_tokens::FleetKeybinds::prescribed();

        // For each (intent, expected action), the binding manager
        // must return that action when looked up with the
        // atlas-parsed hotkey.
        let atlas_actions: &[(&str, Action)] = &[
            (kb.copy,              Action::Copy),
            (kb.paste,             Action::Paste),
            (kb.search_open,       Action::SearchOpen),
            (kb.search_close,      Action::SearchClose),
            (kb.search_next,       Action::SearchNext),
            (kb.search_prev,       Action::SearchPrev),
            (kb.font_increase,     Action::FontIncrease),
            (kb.font_decrease,     Action::FontDecrease),
            (kb.font_reset,        Action::FontReset),
            (kb.toggle_fullscreen, Action::ToggleFullscreen),
        ];
        for (chord, expected) in atlas_actions {
            let hk = awase::Hotkey::parse_atlas_chord(chord)
                .unwrap_or_else(|e| panic!("atlas chord {chord:?} parse failed: {e}"));
            assert_eq!(
                mgr.lookup(&hk),
                Some(*expected),
                "mado default for atlas chord {chord:?} does not bind {expected:?}",
            );
        }

        // Symmetric Guard assertion via ishou's typed convergence
        // helper. Catches the same drift class but with the named-
        // intent error message ("font_increase chord drift") rather
        // than the action-lookup mismatch above.
        ishou_tokens::convergence::Guard::for_app("mado")
            .expect_copy(kb.copy)
            .expect_paste(kb.paste)
            .expect_search_open(kb.search_open)
            .expect_search_close(kb.search_close)
            .expect_search_next(kb.search_next)
            .expect_search_prev(kb.search_prev)
            .expect_font_increase(kb.font_increase)
            .expect_font_decrease(kb.font_decrease)
            .expect_font_reset(kb.font_reset)
            .expect_toggle_fullscreen(kb.toggle_fullscreen)
            .run();
    }

    #[test]
    fn lookup_copy() {
        let mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::C);
        assert_eq!(mgr.lookup(&hk), Some(Action::Copy));
    }

    #[test]
    fn lookup_paste() {
        let mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::V);
        assert_eq!(mgr.lookup(&hk), Some(Action::Paste));
    }

    #[test]
    fn lookup_no_match() {
        let mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(awase::Modifiers::NONE, awase::Key::X);
        assert_eq!(mgr.lookup(&hk), None);
    }

    #[test]
    fn custom_binding() {
        let mut mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(awase::Modifiers::CTRL, awase::Key::R);
        mgr.bind(hk, Action::ResetTerminal);
        assert_eq!(mgr.lookup(&hk), Some(Action::ResetTerminal));
    }

    #[test]
    fn unbind() {
        let mut mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::C);
        assert!(mgr.lookup(&hk).is_some());
        mgr.unbind(&hk);
        assert!(mgr.lookup(&hk).is_none());
    }

    #[test]
    fn rebind_replaces() {
        let mut mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::C);
        mgr.bind(hk, Action::ResetTerminal);
        assert_eq!(mgr.lookup(&hk), Some(Action::ResetTerminal));
    }

    #[test]
    fn lookup_key_works() {
        let mgr = KeybindManager::with_mado_defaults();
        let action = mgr.lookup_key(awase::Key::C, awase::Modifiers::CMD);
        assert_eq!(action, Some(Action::Copy));
    }

    #[test]
    fn search_bindings() {
        let mgr = KeybindManager::with_mado_defaults();
        let hk_open = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::F);
        assert_eq!(mgr.lookup(&hk_open), Some(Action::SearchOpen));

        let hk_close = awase::Hotkey::new(awase::Modifiers::NONE, awase::Key::Escape);
        assert_eq!(mgr.lookup(&hk_close), Some(Action::SearchClose));
    }

    #[test]
    fn bind_str_valid() {
        let mut mgr = KeybindManager::with_mado_defaults();
        let result = mgr.bind_str("cmd+t", Action::Copy);
        assert!(result.is_ok());
        let hk = awase::Hotkey::parse("cmd+t").unwrap();
        assert_eq!(mgr.lookup(&hk), Some(Action::Copy));
    }

    #[test]
    fn bind_str_invalid() {
        let mut mgr = KeybindManager::with_mado_defaults();
        let result = mgr.bind_str("not_a_real_hotkey!!!", Action::Copy);
        assert!(result.is_err());
    }

    #[test]
    fn default_bindings_count() {
        let mgr = KeybindManager::with_mado_defaults();
        // Default bindings after Phase 4 (multiplexing actions
        // removed): clipboard (2) + search (4) + font (3) +
        // scroll (4) + prompt jump (2) + terminal (1) + fullscreen
        // (1) = 17.
        assert_eq!(mgr.bindings().len(), 17);
    }

    #[test]
    fn all_actions_serializable() {
        let actions = [
            Action::Copy, Action::Paste, Action::PasteFromSelection,
            Action::ScrollUp, Action::ScrollDown,
            Action::ScrollPageUp, Action::ScrollPageDown, Action::ScrollToTop,
            Action::ScrollToBottom, Action::JumpToPrompt,
            Action::JumpToPromptPrev, Action::JumpToPromptNext,
            Action::SearchOpen, Action::SearchClose,
            Action::SearchNext, Action::SearchPrev, Action::FontIncrease,
            Action::FontDecrease, Action::FontReset,
            Action::ResetTerminal,
            Action::ClearScreen, Action::ToggleFullscreen, Action::SelectAll,
            Action::CopyUrlToClipboard, Action::ToggleMouseReporting,
        ];
        for action in &actions {
            let json = serde_json::to_string(action);
            assert!(json.is_ok(), "Failed to serialize {:?}", action);
            let json_str = json.unwrap();
            let parsed: Result<Action, _> = serde_json::from_str(&json_str);
            assert!(parsed.is_ok(), "Failed to deserialize {:?}", action);
            assert_eq!(*action, parsed.unwrap());
        }
    }

    #[test]
    fn test_scroll_page_up_binding() {
        let mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(awase::Modifiers::NONE, awase::Key::PageUp);
        assert_eq!(mgr.lookup(&hk), Some(Action::ScrollPageUp));
    }

    #[test]
    fn test_scroll_page_down_binding() {
        let mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(awase::Modifiers::NONE, awase::Key::PageDown);
        assert_eq!(mgr.lookup(&hk), Some(Action::ScrollPageDown));
    }

    // test_focus_next_binding / test_focus_prev_binding /
    // test_close_pane_binding removed at Phase 4 — the actions
    // they exercised no longer exist; mado is single-pane now.

    #[test]
    fn test_toggle_fullscreen_binding() {
        let mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(
            awase::Modifiers::CMD | awase::Modifiers::CTRL,
            awase::Key::F,
        );
        assert_eq!(mgr.lookup(&hk), Some(Action::ToggleFullscreen));
    }

    #[test]
    fn test_reset_terminal_binding() {
        let mgr = KeybindManager::with_mado_defaults();
        let hk = awase::Hotkey::new(
            awase::Modifiers::CMD | awase::Modifiers::SHIFT,
            awase::Key::R,
        );
        assert_eq!(mgr.lookup(&hk), Some(Action::ResetTerminal));
    }

    #[test]
    fn test_total_default_bindings_count() {
        let mgr = KeybindManager::with_mado_defaults();
        assert_eq!(mgr.bindings().len(), 17);
    }

    #[test]
    fn test_parse_action_known() {
        assert_eq!(parse_action("copy"), Some(Action::Copy));
        assert_eq!(parse_action("paste"), Some(Action::Paste));
        assert_eq!(parse_action("paste_from_selection"), Some(Action::PasteFromSelection));
        assert_eq!(parse_action("scroll_to_top"), Some(Action::ScrollToTop));
        assert_eq!(parse_action("jump_to_prompt"), Some(Action::JumpToPrompt));
        assert_eq!(parse_action("clear_screen"), Some(Action::ClearScreen));
        assert_eq!(parse_action("select_all"), Some(Action::SelectAll));
        assert_eq!(parse_action("copy_url_to_clipboard"), Some(Action::CopyUrlToClipboard));
        assert_eq!(parse_action("toggle_mouse_reporting"), Some(Action::ToggleMouseReporting));
    }

    #[test]
    fn test_parse_action_aliases() {
        assert_eq!(parse_action("search"), Some(Action::SearchOpen));
        assert_eq!(parse_action("increase_font_size"), Some(Action::FontIncrease));
        assert_eq!(parse_action("decrease_font_size"), Some(Action::FontDecrease));
        assert_eq!(parse_action("reset_font_size"), Some(Action::FontReset));
        // Phase 4 — goto_split:* / close_surface resolve to None
        // (multiplexing belongs in tear).
        assert_eq!(parse_action("goto_split:next"), None);
        assert_eq!(parse_action("goto_split:previous"), None);
        assert_eq!(parse_action("close_surface"), None);
        assert_eq!(parse_action("reset"), Some(Action::ResetTerminal));
    }

    #[test]
    fn test_parse_action_unknown() {
        assert_eq!(parse_action("not_a_real_action"), None);
        assert_eq!(parse_action(""), None);
    }
}
