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
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    SearchOpen,
    SearchClose,
    SearchNext,
    SearchPrev,
    FontIncrease,
    FontDecrease,
    FontReset,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SplitHorizontal,
    SplitVertical,
    FocusNext,
    FocusPrev,
    ClosePane,
    ResetTerminal,
    ToggleFullscreen,
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

impl KeybindManager {
    /// Create with platform-appropriate default bindings.
    #[must_use]
    pub fn new() -> Self {
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
}

impl Default for KeybindManager {
    fn default() -> Self {
        Self::new()
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

    let cmd = Modifiers::CMD;
    let cmd_shift = Modifiers::CMD | Modifiers::SHIFT;
    let none = Modifiers::NONE;

    vec![
        // Clipboard
        Keybinding { hotkey: hk(cmd, Key::C), action: Action::Copy },
        Keybinding { hotkey: hk(cmd, Key::V), action: Action::Paste },
        // Search
        Keybinding { hotkey: hk(cmd, Key::F), action: Action::SearchOpen },
        Keybinding { hotkey: hk(none, Key::Escape), action: Action::SearchClose },
        Keybinding { hotkey: hk(cmd, Key::G), action: Action::SearchNext },
        Keybinding { hotkey: hk(cmd_shift, Key::G), action: Action::SearchPrev },
        // Font
        Keybinding { hotkey: hk(cmd, Key::Equal), action: Action::FontIncrease },
        Keybinding { hotkey: hk(cmd, Key::Minus), action: Action::FontDecrease },
        Keybinding { hotkey: hk(cmd, Key::Num0), action: Action::FontReset },
        // Tabs
        Keybinding { hotkey: hk(cmd, Key::T), action: Action::NewTab },
        Keybinding { hotkey: hk(cmd, Key::W), action: Action::CloseTab },
        // Splits
        Keybinding { hotkey: hk(cmd, Key::D), action: Action::SplitVertical },
        Keybinding { hotkey: hk(cmd_shift, Key::D), action: Action::SplitHorizontal },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bindings_exist() {
        let mgr = KeybindManager::new();
        assert!(!mgr.bindings().is_empty());
    }

    #[test]
    fn lookup_copy() {
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::C);
        assert_eq!(mgr.lookup(&hk), Some(Action::Copy));
    }

    #[test]
    fn lookup_paste() {
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::V);
        assert_eq!(mgr.lookup(&hk), Some(Action::Paste));
    }

    #[test]
    fn lookup_no_match() {
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::NONE, awase::Key::X);
        assert_eq!(mgr.lookup(&hk), None);
    }

    #[test]
    fn custom_binding() {
        let mut mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::CTRL, awase::Key::R);
        mgr.bind(hk, Action::ResetTerminal);
        assert_eq!(mgr.lookup(&hk), Some(Action::ResetTerminal));
    }

    #[test]
    fn unbind() {
        let mut mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::C);
        assert!(mgr.lookup(&hk).is_some());
        mgr.unbind(&hk);
        assert!(mgr.lookup(&hk).is_none());
    }

    #[test]
    fn rebind_replaces() {
        let mut mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::C);
        mgr.bind(hk, Action::ResetTerminal);
        assert_eq!(mgr.lookup(&hk), Some(Action::ResetTerminal));
    }

    #[test]
    fn lookup_key_works() {
        let mgr = KeybindManager::new();
        let action = mgr.lookup_key(awase::Key::C, awase::Modifiers::CMD);
        assert_eq!(action, Some(Action::Copy));
    }

    #[test]
    fn search_bindings() {
        let mgr = KeybindManager::new();
        let hk_open = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::F);
        assert_eq!(mgr.lookup(&hk_open), Some(Action::SearchOpen));

        let hk_close = awase::Hotkey::new(awase::Modifiers::NONE, awase::Key::Escape);
        assert_eq!(mgr.lookup(&hk_close), Some(Action::SearchClose));
    }
}
