//! Keybinding system — configurable key → action mapping.
//!
//! Provides a default set of keybindings with user override support
//! via configuration. Actions represent high-level terminal operations
//! that main.rs dispatches.

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

/// Modifier key state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
    };

    pub const META: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        meta: true,
    };

    pub const CTRL: Self = Self {
        ctrl: true,
        alt: false,
        shift: false,
        meta: false,
    };

    pub const META_SHIFT: Self = Self {
        ctrl: false,
        alt: false,
        shift: true,
        meta: true,
    };

    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.ctrl == other.ctrl
            && self.alt == other.alt
            && self.shift == other.shift
            && self.meta == other.meta
    }
}

/// A key identifier for binding purposes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Key {
    Char(char),
    F(u8),
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
}

/// A single keybinding mapping a key + modifiers to an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybinding {
    pub key: Key,
    pub modifiers: Modifiers,
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

    /// Look up an action for the given key + modifiers.
    #[must_use]
    pub fn lookup(&self, key: &Key, modifiers: &Modifiers) -> Option<Action> {
        self.bindings
            .iter()
            .find(|b| b.key == *key && b.modifiers.matches(modifiers))
            .map(|b| b.action)
    }

    /// Add or replace a keybinding.
    pub fn bind(&mut self, key: Key, modifiers: Modifiers, action: Action) {
        // Remove existing binding for this key+modifier combo
        self.bindings
            .retain(|b| !(b.key == key && b.modifiers.matches(&modifiers)));
        self.bindings.push(Keybinding {
            key,
            modifiers,
            action,
        });
    }

    /// Remove a keybinding for the given key + modifiers.
    pub fn unbind(&mut self, key: &Key, modifiers: &Modifiers) {
        self.bindings
            .retain(|b| !(b.key == *key && b.modifiers.matches(modifiers)));
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

/// Default keybindings (macOS-style: Cmd as primary modifier).
fn default_bindings() -> Vec<Keybinding> {
    vec![
        // Clipboard
        Keybinding {
            key: Key::Char('c'),
            modifiers: Modifiers::META,
            action: Action::Copy,
        },
        Keybinding {
            key: Key::Char('v'),
            modifiers: Modifiers::META,
            action: Action::Paste,
        },
        // Search
        Keybinding {
            key: Key::Char('f'),
            modifiers: Modifiers::META,
            action: Action::SearchOpen,
        },
        Keybinding {
            key: Key::Escape,
            modifiers: Modifiers::NONE,
            action: Action::SearchClose,
        },
        Keybinding {
            key: Key::Char('g'),
            modifiers: Modifiers::META,
            action: Action::SearchNext,
        },
        Keybinding {
            key: Key::Char('g'),
            modifiers: Modifiers::META_SHIFT,
            action: Action::SearchPrev,
        },
        // Font
        Keybinding {
            key: Key::Char('+'),
            modifiers: Modifiers::META,
            action: Action::FontIncrease,
        },
        Keybinding {
            key: Key::Char('-'),
            modifiers: Modifiers::META,
            action: Action::FontDecrease,
        },
        Keybinding {
            key: Key::Char('0'),
            modifiers: Modifiers::META,
            action: Action::FontReset,
        },
        // Tabs
        Keybinding {
            key: Key::Char('t'),
            modifiers: Modifiers::META,
            action: Action::NewTab,
        },
        Keybinding {
            key: Key::Char('w'),
            modifiers: Modifiers::META,
            action: Action::CloseTab,
        },
        Keybinding {
            key: Key::Char(']'),
            modifiers: Modifiers::META_SHIFT,
            action: Action::NextTab,
        },
        Keybinding {
            key: Key::Char('['),
            modifiers: Modifiers::META_SHIFT,
            action: Action::PrevTab,
        },
        // Splits
        Keybinding {
            key: Key::Char('d'),
            modifiers: Modifiers::META,
            action: Action::SplitVertical,
        },
        Keybinding {
            key: Key::Char('d'),
            modifiers: Modifiers::META_SHIFT,
            action: Action::SplitHorizontal,
        },
        // Scroll
        Keybinding {
            key: Key::PageUp,
            modifiers: Modifiers::NONE,
            action: Action::ScrollPageUp,
        },
        Keybinding {
            key: Key::PageDown,
            modifiers: Modifiers::NONE,
            action: Action::ScrollPageDown,
        },
        Keybinding {
            key: Key::Home,
            modifiers: Modifiers::META,
            action: Action::ScrollToTop,
        },
        Keybinding {
            key: Key::End,
            modifiers: Modifiers::META,
            action: Action::ScrollToBottom,
        },
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
        let action = mgr.lookup(&Key::Char('c'), &Modifiers::META);
        assert_eq!(action, Some(Action::Copy));
    }

    #[test]
    fn lookup_paste() {
        let mgr = KeybindManager::new();
        let action = mgr.lookup(&Key::Char('v'), &Modifiers::META);
        assert_eq!(action, Some(Action::Paste));
    }

    #[test]
    fn lookup_no_match() {
        let mgr = KeybindManager::new();
        let action = mgr.lookup(&Key::Char('x'), &Modifiers::NONE);
        assert!(action.is_none());
    }

    #[test]
    fn custom_binding() {
        let mut mgr = KeybindManager::new();
        mgr.bind(Key::Char('r'), Modifiers::CTRL, Action::ResetTerminal);
        let action = mgr.lookup(&Key::Char('r'), &Modifiers::CTRL);
        assert_eq!(action, Some(Action::ResetTerminal));
    }

    #[test]
    fn unbind() {
        let mut mgr = KeybindManager::new();
        assert!(mgr.lookup(&Key::Char('c'), &Modifiers::META).is_some());
        mgr.unbind(&Key::Char('c'), &Modifiers::META);
        assert!(mgr.lookup(&Key::Char('c'), &Modifiers::META).is_none());
    }

    #[test]
    fn rebind_replaces() {
        let mut mgr = KeybindManager::new();
        mgr.bind(Key::Char('c'), Modifiers::META, Action::ResetTerminal);
        let action = mgr.lookup(&Key::Char('c'), &Modifiers::META);
        assert_eq!(action, Some(Action::ResetTerminal));
    }

    #[test]
    fn modifiers_match() {
        assert!(Modifiers::META.matches(&Modifiers::META));
        assert!(!Modifiers::META.matches(&Modifiers::CTRL));
        assert!(!Modifiers::META.matches(&Modifiers::NONE));
    }

    #[test]
    fn search_bindings() {
        let mgr = KeybindManager::new();
        assert_eq!(
            mgr.lookup(&Key::Char('f'), &Modifiers::META),
            Some(Action::SearchOpen)
        );
        assert_eq!(
            mgr.lookup(&Key::Escape, &Modifiers::NONE),
            Some(Action::SearchClose)
        );
    }
}
