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
        "search_open" | "search" => Some(Action::SearchOpen),
        "search_close" => Some(Action::SearchClose),
        "search_next" => Some(Action::SearchNext),
        "search_prev" => Some(Action::SearchPrev),
        "font_increase" | "increase_font_size" => Some(Action::FontIncrease),
        "font_decrease" | "decrease_font_size" => Some(Action::FontDecrease),
        "font_reset" | "reset_font_size" => Some(Action::FontReset),
        "new_tab" => Some(Action::NewTab),
        "close_tab" => Some(Action::CloseTab),
        "next_tab" => Some(Action::NextTab),
        "prev_tab" => Some(Action::PrevTab),
        "split_horizontal" => Some(Action::SplitHorizontal),
        "split_vertical" => Some(Action::SplitVertical),
        "focus_next" | "goto_split:next" => Some(Action::FocusNext),
        "focus_prev" | "goto_split:previous" => Some(Action::FocusPrev),
        "close_pane" | "close_surface" => Some(Action::ClosePane),
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
        // Scroll
        Keybinding { hotkey: hk(none, Key::PageUp), action: Action::ScrollPageUp },
        Keybinding { hotkey: hk(none, Key::PageDown), action: Action::ScrollPageDown },
        Keybinding { hotkey: hk(cmd, Key::Home), action: Action::ScrollToTop },
        Keybinding { hotkey: hk(cmd, Key::End), action: Action::ScrollToBottom },
        // Pane navigation
        Keybinding { hotkey: hk(cmd, Key::RightBracket), action: Action::FocusNext },
        Keybinding { hotkey: hk(cmd, Key::LeftBracket), action: Action::FocusPrev },
        Keybinding { hotkey: hk(cmd_shift, Key::W), action: Action::ClosePane },
        // Tab navigation
        Keybinding { hotkey: hk(cmd_shift, Key::RightBracket), action: Action::NextTab },
        Keybinding { hotkey: hk(cmd_shift, Key::LeftBracket), action: Action::PrevTab },
        // Terminal
        Keybinding { hotkey: hk(cmd_shift, Key::R), action: Action::ResetTerminal },
        // Fullscreen
        Keybinding { hotkey: hk(cmd | Modifiers::CTRL, Key::F), action: Action::ToggleFullscreen },
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

    #[test]
    fn bind_str_valid() {
        let mut mgr = KeybindManager::new();
        let result = mgr.bind_str("cmd+t", Action::NewTab);
        assert!(result.is_ok());
        let hk = awase::Hotkey::parse("cmd+t").unwrap();
        assert_eq!(mgr.lookup(&hk), Some(Action::NewTab));
    }

    #[test]
    fn bind_str_invalid() {
        let mut mgr = KeybindManager::new();
        let result = mgr.bind_str("not_a_real_hotkey!!!", Action::Copy);
        assert!(result.is_err());
    }

    #[test]
    fn default_bindings_count() {
        let mgr = KeybindManager::new();
        // Default bindings include Copy, Paste, SearchOpen, SearchClose,
        // SearchNext, SearchPrev, FontIncrease, FontDecrease, FontReset,
        // NewTab, CloseTab, SplitVertical, SplitHorizontal, plus scroll,
        // pane, tab, terminal, fullscreen = 24
        assert_eq!(mgr.bindings().len(), 24);
    }

    #[test]
    fn all_actions_serializable() {
        let actions = [
            Action::Copy, Action::Paste, Action::PasteFromSelection,
            Action::ScrollUp, Action::ScrollDown,
            Action::ScrollPageUp, Action::ScrollPageDown, Action::ScrollToTop,
            Action::ScrollToBottom, Action::JumpToPrompt,
            Action::SearchOpen, Action::SearchClose,
            Action::SearchNext, Action::SearchPrev, Action::FontIncrease,
            Action::FontDecrease, Action::FontReset, Action::NewTab,
            Action::CloseTab, Action::NextTab, Action::PrevTab,
            Action::SplitHorizontal, Action::SplitVertical, Action::FocusNext,
            Action::FocusPrev, Action::ClosePane, Action::ResetTerminal,
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
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::NONE, awase::Key::PageUp);
        assert_eq!(mgr.lookup(&hk), Some(Action::ScrollPageUp));
    }

    #[test]
    fn test_scroll_page_down_binding() {
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::NONE, awase::Key::PageDown);
        assert_eq!(mgr.lookup(&hk), Some(Action::ScrollPageDown));
    }

    #[test]
    fn test_focus_next_binding() {
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::RightBracket);
        assert_eq!(mgr.lookup(&hk), Some(Action::FocusNext));
    }

    #[test]
    fn test_focus_prev_binding() {
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(awase::Modifiers::CMD, awase::Key::LeftBracket);
        assert_eq!(mgr.lookup(&hk), Some(Action::FocusPrev));
    }

    #[test]
    fn test_close_pane_binding() {
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(
            awase::Modifiers::CMD | awase::Modifiers::SHIFT,
            awase::Key::W,
        );
        assert_eq!(mgr.lookup(&hk), Some(Action::ClosePane));
    }

    #[test]
    fn test_toggle_fullscreen_binding() {
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(
            awase::Modifiers::CMD | awase::Modifiers::CTRL,
            awase::Key::F,
        );
        assert_eq!(mgr.lookup(&hk), Some(Action::ToggleFullscreen));
    }

    #[test]
    fn test_reset_terminal_binding() {
        let mgr = KeybindManager::new();
        let hk = awase::Hotkey::new(
            awase::Modifiers::CMD | awase::Modifiers::SHIFT,
            awase::Key::R,
        );
        assert_eq!(mgr.lookup(&hk), Some(Action::ResetTerminal));
    }

    #[test]
    fn test_total_default_bindings_count() {
        let mgr = KeybindManager::new();
        assert_eq!(mgr.bindings().len(), 24);
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
        assert_eq!(parse_action("goto_split:next"), Some(Action::FocusNext));
        assert_eq!(parse_action("goto_split:previous"), Some(Action::FocusPrev));
        assert_eq!(parse_action("close_surface"), Some(Action::ClosePane));
        assert_eq!(parse_action("reset"), Some(Action::ResetTerminal));
    }

    #[test]
    fn test_parse_action_unknown() {
        assert_eq!(parse_action("not_a_real_action"), None);
        assert_eq!(parse_action(""), None);
    }
}
