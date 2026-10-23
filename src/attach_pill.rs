use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

pub const HOLD: Duration = Duration::from_millis(1200);
pub const FADE: Duration = Duration::from_millis(300);
pub const NOTICE_HOLD: Duration = Duration::from_millis(6000);
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
    Notice(String),
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
            Self::Notice(text) => f.write_str(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PillFrame {
    pub text: String,
    pub alpha: f32,
    pub refused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stamp {
    Pending,
    At(f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PillDemand {
    Idle,
    Now,
    At(f32),
    Continuous,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttachPill {
    enabled: bool,
    shown: Option<(Attached, Stamp)>,
}

impl Default for AttachPill {
    fn default() -> Self {
        Self::new(false)
    }
}

fn hold_of(what: &Attached) -> f32 {
    match what {
        Attached::Notice(_) => NOTICE_HOLD,
        Attached::File { .. } | Attached::Files(_) | Attached::Refused => HOLD,
    }
    .as_secs_f32()
}

impl AttachPill {
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            shown: None,
        }
    }

    pub fn show(&mut self, what: Attached) {
        if self.enabled {
            self.shown = Some((what, Stamp::Pending));
        }
    }

    pub fn notify(&mut self, text: String) {
        self.shown = Some((Attached::Notice(text), Stamp::Pending));
    }

    fn adopt(&mut self, elapsed: f32) {
        if let Some((_, stamp @ Stamp::Pending)) = self.shown.as_mut() {
            *stamp = Stamp::At(elapsed);
        }
    }

    #[must_use]
    pub fn alpha_at(&self, elapsed: f32) -> Option<f32> {
        let (what, stamp) = self.shown.as_ref()?;
        let Stamp::At(at) = *stamp else {
            return Some(1.0);
        };
        let age = (elapsed - at).max(0.0);
        let hold = hold_of(what);
        if age < hold {
            return Some(1.0);
        }
        let fade = FADE.as_secs_f32();
        let fading = age - hold;
        (fading < fade).then(|| 1.0 - fading / fade)
    }

    pub fn frame(&mut self, elapsed: f32) -> Option<PillFrame> {
        self.adopt(elapsed);
        let alpha = self.alpha_at(elapsed)?;
        let (what, _) = self.shown.as_ref()?;
        Some(PillFrame {
            text: what.to_string(),
            alpha,
            refused: matches!(what, Attached::Refused),
        })
    }

    #[must_use]
    pub fn demand(&self, elapsed: f32) -> PillDemand {
        let Some((what, stamp)) = self.shown.as_ref() else {
            return PillDemand::Idle;
        };
        let Stamp::At(at) = *stamp else {
            return PillDemand::Now;
        };
        let age = (elapsed - at).max(0.0);
        let hold = hold_of(what);
        if age < hold {
            PillDemand::At(at + hold)
        } else if age - hold < FADE.as_secs_f32() {
            PillDemand::Continuous
        } else {
            PillDemand::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_shows_even_with_the_attachment_pill_off_and_holds_longer() {
        let mut pill = AttachPill::new(false);
        pill.show(Attached::Refused);
        assert_eq!(pill.frame(0.0), None);
        pill.notify("pane p: 9 oldest history rows were too large to load".into());
        let frame = pill.frame(0.0).expect("the notice shows");
        assert!(frame.text.contains("history rows"));
        assert!(!frame.refused);
        let settle = (HOLD + FADE).as_secs_f32();
        assert_eq!(pill.alpha_at(settle), Some(1.0));
        assert_eq!(pill.alpha_at((NOTICE_HOLD + FADE).as_secs_f32()), None);
    }

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
        let mut pill = AttachPill::new(true);
        assert_eq!(pill.frame(0.0), None);
        pill.show(Attached::Files(2));
        let first = pill.frame(10.0).unwrap();
        assert_eq!(first.text, "attached 2 files");
        let hold = HOLD.as_secs_f32();
        let fade = FADE.as_secs_f32();
        let held = pill.frame(10.0 + hold - 0.001).unwrap();
        assert!((held.alpha - 1.0).abs() < f32::EPSILON);
        assert!(!held.refused);
        let mid = pill.alpha_at(10.0 + hold + fade / 2.0).unwrap();
        assert!((mid - 0.5).abs() < 0.01, "{mid}");
        assert_eq!(pill.frame(10.0 + hold + fade + 0.001), None);
        assert!(HOLD + FADE <= Duration::from_millis(1500));
    }

    #[test]
    fn the_pill_asks_for_frames_only_while_it_animates() {
        let mut pill = AttachPill::new(true);
        let hold = HOLD.as_secs_f32();
        let fade = FADE.as_secs_f32();
        assert_eq!(pill.demand(0.0), PillDemand::Idle);
        pill.show(Attached::Files(2));
        assert_eq!(pill.demand(5.0), PillDemand::Now);
        pill.frame(5.0);
        assert_eq!(pill.demand(5.1), PillDemand::At(5.0 + hold));
        assert_eq!(pill.demand(5.0 + hold + 0.01), PillDemand::Continuous);
        assert_eq!(pill.demand(5.0 + hold + fade + 0.001), PillDemand::Idle);
    }

    #[test]
    fn a_new_attachment_restarts_the_pill() {
        let mut pill = AttachPill::new(true);
        pill.show(Attached::Files(2));
        pill.frame(0.0);
        let later = HOLD.as_secs_f32() + FADE.as_secs_f32() / 2.0;
        pill.show(Attached::Refused);
        let frame = pill.frame(later).unwrap();
        assert!(frame.refused);
        assert!((frame.alpha - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_disabled_pill_never_shows() {
        let mut pill = AttachPill::new(false);
        pill.show(Attached::Files(3));
        assert_eq!(pill.frame(0.0), None);
    }
}
