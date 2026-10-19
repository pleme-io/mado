use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const HOLD: Duration = Duration::from_millis(1200);
pub const FADE: Duration = Duration::from_millis(300);
const NAME_MAX: usize = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size(pub u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.0;
        if bytes < 1000 {
            write!(f, "{bytes} B")
        } else if bytes < 1_000_000 {
            write!(f, "{} KB", (bytes + 500) / 1000)
        } else {
            write!(f, "{:.1} MB", bytes as f64 / 1_000_000.0)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortName<'a>(pub &'a str);

impl fmt::Display for ShortName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let chars: Vec<char> = self.0.chars().collect();
        if chars.len() <= NAME_MAX {
            return f.write_str(self.0);
        }
        let tail = NAME_MAX / 3;
        let head = NAME_MAX - tail - 1;
        let head: String = chars[..head].iter().collect();
        let tail: String = chars[chars.len() - tail..].iter().collect();
        write!(f, "{head}…{tail}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attached {
    File { name: String, size: Option<Size> },
    Files(usize),
    Refused,
}

impl Attached {
    #[must_use]
    pub fn of(paths: &[PathBuf]) -> Self {
        match paths {
            [] => Self::Refused,
            [one] => Self::File {
                name: one
                    .file_name()
                    .map_or_else(|| one.to_string_lossy(), |n| n.to_string_lossy())
                    .into_owned(),
                size: std::fs::metadata(one)
                    .ok()
                    .filter(std::fs::Metadata::is_file)
                    .map(|m| Size(m.len())),
            },
            many => Self::Files(many.len()),
        }
    }
}

impl fmt::Display for Attached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File {
                name,
                size: Some(size),
            } => write!(f, "attached {} · {size}", ShortName(name)),
            Self::File { name, size: None } => write!(f, "attached {}", ShortName(name)),
            Self::Files(n) => write!(f, "attached {n} files"),
            Self::Refused => f.write_str("refused: no file or image"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PillFrame {
    pub text: String,
    pub alpha: f32,
    pub refused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachPill {
    enabled: bool,
    shown: Option<(Attached, Instant)>,
}

impl Default for AttachPill {
    fn default() -> Self {
        Self::new(false)
    }
}

impl AttachPill {
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            shown: None,
        }
    }

    pub fn show(&mut self, what: Attached, now: Instant) {
        if self.enabled {
            self.shown = Some((what, now));
        }
    }

    #[must_use]
    pub fn alpha_at(&self, now: Instant) -> Option<f32> {
        let (_, at) = self.shown.as_ref()?;
        let age = now.saturating_duration_since(*at);
        if age < HOLD {
            return Some(1.0);
        }
        let fading = age - HOLD;
        (fading < FADE).then(|| 1.0 - fading.as_secs_f32() / FADE.as_secs_f32())
    }

    #[must_use]
    pub fn frame(&self, now: Instant) -> Option<PillFrame> {
        let alpha = self.alpha_at(now)?;
        let (what, _) = self.shown.as_ref()?;
        Some(PillFrame {
            text: what.to_string(),
            alpha,
            refused: matches!(what, Attached::Refused),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_finder() {
        assert_eq!(Size(512).to_string(), "512 B");
        assert_eq!(Size(240_123).to_string(), "240 KB");
        assert_eq!(Size(999_499).to_string(), "999 KB");
        assert_eq!(Size(2_450_000).to_string(), "2.5 MB");
    }

    #[test]
    fn a_long_name_keeps_its_head_and_extension() {
        let name = "mado-drop-1729384756123456789-4242.png";
        let short = ShortName(name).to_string();
        assert_eq!(short.chars().count(), NAME_MAX);
        assert!(short.starts_with("mado-drop-"), "{short}");
        assert!(short.ends_with("4242.png"), "{short}");
        assert!(short.contains('…'));
        assert_eq!(ShortName("shot.png").to_string(), "shot.png");
    }

    #[test]
    fn one_file_names_itself_and_its_size() {
        let dir = std::env::temp_dir().join(crate::clipboard_store::DROP_TEMP.name("-pill"));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shot.png");
        std::fs::write(&file, vec![0u8; 240_000]).unwrap();
        assert_eq!(
            Attached::of(std::slice::from_ref(&file)).to_string(),
            "attached shot.png · 240 KB"
        );
        assert_eq!(
            Attached::of(&[dir.join("gone.png")]).to_string(),
            "attached gone.png"
        );
        assert_eq!(
            Attached::of(std::slice::from_ref(&dir)).to_string(),
            format!(
                "attached {}",
                ShortName(&dir.file_name().unwrap().to_string_lossy())
            ),
            "a promised directory has no single size"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn several_files_count_and_nothing_is_a_refusal() {
        let paths = [PathBuf::from("/a"), PathBuf::from("/b")];
        assert_eq!(Attached::of(&paths).to_string(), "attached 2 files");
        assert_eq!(Attached::of(&[]), Attached::Refused);
        assert_eq!(Attached::Refused.to_string(), "refused: no file or image");
    }

    #[test]
    fn the_pill_holds_then_fades_then_is_gone() {
        let t0 = Instant::now();
        let mut pill = AttachPill::new(true);
        assert_eq!(pill.frame(t0), None);
        pill.show(Attached::Files(2), t0);
        let held = pill.frame(t0 + HOLD - Duration::from_millis(1)).unwrap();
        assert_eq!(held.text, "attached 2 files");
        assert!((held.alpha - 1.0).abs() < f32::EPSILON);
        assert!(!held.refused);
        let mid = pill.alpha_at(t0 + HOLD + FADE / 2).unwrap();
        assert!((mid - 0.5).abs() < 0.01, "{mid}");
        assert_eq!(pill.frame(t0 + HOLD + FADE), None);
        assert!(HOLD + FADE <= Duration::from_millis(1500));
    }

    #[test]
    fn a_new_attachment_restarts_the_pill() {
        let t0 = Instant::now();
        let mut pill = AttachPill::new(true);
        pill.show(Attached::Files(2), t0);
        let later = t0 + HOLD + FADE / 2;
        pill.show(Attached::Refused, later);
        let frame = pill.frame(later).unwrap();
        assert!(frame.refused);
        assert!((frame.alpha - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_disabled_pill_never_shows() {
        let t0 = Instant::now();
        let mut pill = AttachPill::new(false);
        pill.show(Attached::Files(3), t0);
        assert_eq!(pill.frame(t0), None);
    }
}
