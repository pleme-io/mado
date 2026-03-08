//! Color theme system — named color schemes for terminal rendering.
//!
//! Provides built-in themes (Nord, Dracula, Solarized, etc.) and
//! a configurable theme structure that maps to the terminal's color palette.

use crate::terminal::Color;

/// A complete terminal color theme.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,
    pub background: Color,
    pub foreground: Color,
    pub cursor: Color,
    pub selection_bg: [f32; 4], // RGBA for selection overlay
    /// ANSI colors 0-15 (normal 0-7, bright 8-15).
    pub ansi: [Color; 16],
}

impl Theme {
    /// Look up a theme by name (case-insensitive).
    #[must_use]
    pub fn by_name(name: &str) -> Option<&'static Theme> {
        THEMES.iter().find(|t| t.name.eq_ignore_ascii_case(name))
    }

    /// List all available theme names.
    #[must_use]
    pub fn available() -> &'static [Theme] {
        &THEMES
    }
}

// ---------------------------------------------------------------------------
// Built-in themes
// ---------------------------------------------------------------------------

static THEMES: [Theme; 8] = [
    // Nord
    Theme {
        name: "nord",
        background: Color::new(46, 52, 64),    // #2e3440
        foreground: Color::new(236, 239, 244),  // #eceff4
        cursor: Color::new(236, 239, 244),      // #eceff4
        selection_bg: [0.533, 0.753, 0.816, 0.3], // Nord frost
        ansi: [
            Color::new(59, 66, 82),     // 0  nord1
            Color::new(191, 97, 106),   // 1  nord11 red
            Color::new(163, 190, 140),  // 2  nord14 green
            Color::new(235, 203, 139),  // 3  nord13 yellow
            Color::new(129, 161, 193),  // 4  nord9 blue
            Color::new(180, 142, 173),  // 5  nord15 magenta
            Color::new(136, 192, 208),  // 6  nord8 cyan
            Color::new(229, 233, 240),  // 7  nord5 white
            Color::new(76, 86, 106),    // 8  nord3 bright black
            Color::new(191, 97, 106),   // 9  nord11 bright red
            Color::new(163, 190, 140),  // 10 nord14 bright green
            Color::new(235, 203, 139),  // 11 nord13 bright yellow
            Color::new(129, 161, 193),  // 12 nord9 bright blue
            Color::new(180, 142, 173),  // 13 nord15 bright magenta
            Color::new(143, 188, 187),  // 14 nord7 bright cyan
            Color::new(236, 239, 244),  // 15 nord6 bright white
        ],
    },
    // Dracula
    Theme {
        name: "dracula",
        background: Color::new(40, 42, 54),     // #282a36
        foreground: Color::new(248, 248, 242),   // #f8f8f2
        cursor: Color::new(248, 248, 242),
        selection_bg: [0.275, 0.278, 0.353, 0.5],
        ansi: [
            Color::new(33, 34, 44),     // 0
            Color::new(255, 85, 85),    // 1  red
            Color::new(80, 250, 123),   // 2  green
            Color::new(241, 250, 140),  // 3  yellow
            Color::new(98, 114, 164),   // 4  blue (comment)
            Color::new(255, 121, 198),  // 5  pink
            Color::new(139, 233, 253),  // 6  cyan
            Color::new(248, 248, 242),  // 7  fg
            Color::new(98, 114, 164),   // 8  comment
            Color::new(255, 110, 110),  // 9
            Color::new(105, 255, 148),  // 10
            Color::new(246, 255, 165),  // 11
            Color::new(130, 148, 196),  // 12
            Color::new(255, 146, 213),  // 13
            Color::new(164, 248, 255),  // 14
            Color::new(255, 255, 255),  // 15
        ],
    },
    // Solarized Dark
    Theme {
        name: "solarized-dark",
        background: Color::new(0, 43, 54),       // #002b36
        foreground: Color::new(131, 148, 150),    // #839496
        cursor: Color::new(131, 148, 150),
        selection_bg: [0.027, 0.212, 0.259, 0.5], // #073642
        ansi: [
            Color::new(7, 54, 66),      // 0  base02
            Color::new(220, 50, 47),    // 1  red
            Color::new(133, 153, 0),    // 2  green
            Color::new(181, 137, 0),    // 3  yellow
            Color::new(38, 139, 210),   // 4  blue
            Color::new(211, 54, 130),   // 5  magenta
            Color::new(42, 161, 152),   // 6  cyan
            Color::new(238, 232, 213),  // 7  base2
            Color::new(0, 43, 54),      // 8  base03
            Color::new(203, 75, 22),    // 9  orange
            Color::new(88, 110, 117),   // 10 base01
            Color::new(101, 123, 131),  // 11 base00
            Color::new(131, 148, 150),  // 12 base0
            Color::new(108, 113, 196),  // 13 violet
            Color::new(147, 161, 161),  // 14 base1
            Color::new(253, 246, 227),  // 15 base3
        ],
    },
    // Solarized Light
    Theme {
        name: "solarized-light",
        background: Color::new(253, 246, 227),    // #fdf6e3
        foreground: Color::new(101, 123, 131),     // #657b83
        cursor: Color::new(101, 123, 131),
        selection_bg: [0.933, 0.910, 0.835, 0.5],
        ansi: [
            Color::new(238, 232, 213),  // 0  base2
            Color::new(220, 50, 47),    // 1
            Color::new(133, 153, 0),    // 2
            Color::new(181, 137, 0),    // 3
            Color::new(38, 139, 210),   // 4
            Color::new(211, 54, 130),   // 5
            Color::new(42, 161, 152),   // 6
            Color::new(7, 54, 66),      // 7  base02
            Color::new(253, 246, 227),  // 8  base3
            Color::new(203, 75, 22),    // 9
            Color::new(147, 161, 161),  // 10
            Color::new(131, 148, 150),  // 11
            Color::new(101, 123, 131),  // 12
            Color::new(108, 113, 196),  // 13
            Color::new(88, 110, 117),   // 14
            Color::new(0, 43, 54),      // 15 base03
        ],
    },
    // Gruvbox Dark
    Theme {
        name: "gruvbox-dark",
        background: Color::new(40, 40, 40),       // #282828
        foreground: Color::new(235, 219, 178),     // #ebdbb2
        cursor: Color::new(235, 219, 178),
        selection_bg: [0.290, 0.267, 0.212, 0.5],
        ansi: [
            Color::new(40, 40, 40),     // 0  bg
            Color::new(204, 36, 29),    // 1  red
            Color::new(152, 151, 26),   // 2  green
            Color::new(215, 153, 33),   // 3  yellow
            Color::new(69, 133, 136),   // 4  blue
            Color::new(177, 98, 134),   // 5  purple
            Color::new(104, 157, 106),  // 6  aqua
            Color::new(168, 153, 132),  // 7  fg4
            Color::new(146, 131, 116),  // 8  gray
            Color::new(251, 73, 52),    // 9
            Color::new(184, 187, 38),   // 10
            Color::new(250, 189, 47),   // 11
            Color::new(131, 165, 152),  // 12
            Color::new(211, 134, 155),  // 13
            Color::new(142, 192, 124),  // 14
            Color::new(235, 219, 178),  // 15 fg
        ],
    },
    // One Dark
    Theme {
        name: "one-dark",
        background: Color::new(40, 44, 52),       // #282c34
        foreground: Color::new(171, 178, 191),     // #abb2bf
        cursor: Color::new(82, 139, 255),          // #528bff
        selection_bg: [0.247, 0.282, 0.357, 0.5],
        ansi: [
            Color::new(40, 44, 52),     // 0
            Color::new(224, 108, 117),  // 1  red
            Color::new(152, 195, 121),  // 2  green
            Color::new(229, 192, 123),  // 3  yellow
            Color::new(97, 175, 239),   // 4  blue
            Color::new(198, 120, 221),  // 5  magenta
            Color::new(86, 182, 194),   // 6  cyan
            Color::new(171, 178, 191),  // 7  fg
            Color::new(92, 99, 112),    // 8  comment
            Color::new(224, 108, 117),  // 9
            Color::new(152, 195, 121),  // 10
            Color::new(229, 192, 123),  // 11
            Color::new(97, 175, 239),   // 12
            Color::new(198, 120, 221),  // 13
            Color::new(86, 182, 194),   // 14
            Color::new(255, 255, 255),  // 15
        ],
    },
    // Tokyo Night
    Theme {
        name: "tokyo-night",
        background: Color::new(26, 27, 38),       // #1a1b26
        foreground: Color::new(169, 177, 214),     // #a9b1d6
        cursor: Color::new(169, 177, 214),
        selection_bg: [0.141, 0.173, 0.345, 0.5],
        ansi: [
            Color::new(21, 22, 30),     // 0
            Color::new(247, 118, 142),  // 1  red
            Color::new(158, 206, 106),  // 2  green
            Color::new(224, 175, 104),  // 3  yellow
            Color::new(122, 162, 247),  // 4  blue
            Color::new(187, 154, 247),  // 5  magenta
            Color::new(125, 207, 255),  // 6  cyan
            Color::new(169, 177, 214),  // 7  fg
            Color::new(65, 72, 104),    // 8  comment
            Color::new(247, 118, 142),  // 9
            Color::new(158, 206, 106),  // 10
            Color::new(224, 175, 104),  // 11
            Color::new(122, 162, 247),  // 12
            Color::new(187, 154, 247),  // 13
            Color::new(125, 207, 255),  // 14
            Color::new(200, 211, 245),  // 15
        ],
    },
    // Catppuccin Mocha
    Theme {
        name: "catppuccin-mocha",
        background: Color::new(30, 30, 46),       // #1e1e2e
        foreground: Color::new(205, 214, 244),     // #cdd6f4
        cursor: Color::new(245, 224, 220),         // #f5e0dc
        selection_bg: [0.227, 0.224, 0.333, 0.5],
        ansi: [
            Color::new(69, 71, 90),     // 0  surface1
            Color::new(243, 139, 168),  // 1  red
            Color::new(166, 227, 161),  // 2  green
            Color::new(249, 226, 175),  // 3  yellow
            Color::new(137, 180, 250),  // 4  blue
            Color::new(203, 166, 247),  // 5  mauve
            Color::new(148, 226, 213),  // 6  teal
            Color::new(186, 194, 222),  // 7  subtext1
            Color::new(88, 91, 112),    // 8  overlay0
            Color::new(243, 139, 168),  // 9
            Color::new(166, 227, 161),  // 10
            Color::new(249, 226, 175),  // 11
            Color::new(137, 180, 250),  // 12
            Color::new(203, 166, 247),  // 13
            Color::new(148, 226, 213),  // 14
            Color::new(205, 214, 244),  // 15 text
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_by_name() {
        let theme = Theme::by_name("nord").unwrap();
        assert_eq!(theme.name, "nord");
        assert_eq!(theme.background, Color::new(46, 52, 64));
    }

    #[test]
    fn lookup_case_insensitive() {
        assert!(Theme::by_name("Nord").is_some());
        assert!(Theme::by_name("DRACULA").is_some());
    }

    #[test]
    fn unknown_theme_returns_none() {
        assert!(Theme::by_name("nonexistent").is_none());
    }

    #[test]
    fn all_themes_have_16_ansi_colors() {
        for theme in Theme::available() {
            assert_eq!(theme.ansi.len(), 16, "theme {} missing colors", theme.name);
        }
    }

    #[test]
    fn eight_themes_available() {
        assert_eq!(Theme::available().len(), 8);
    }
}
