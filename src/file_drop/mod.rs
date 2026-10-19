#[cfg(target_os = "macos")]
pub mod macos;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use crate::clipboard_store::OwnedTemp;

pub const DROP_TEMP: OwnedTemp = OwnedTemp::new("mado-drop-", None);
const DROP_TEMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub const FILE_URL: &str = "public.file-url";
pub const LEGACY_FILENAMES: &str = "NSFilenamesPboardType";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Tiff,
    Jpeg,
}

impl ImageKind {
    pub const ALL: [Self; 3] = [Self::Png, Self::Tiff, Self::Jpeg];

    #[must_use]
    pub const fn uti(self) -> &'static str {
        match self {
            Self::Png => "public.png",
            Self::Tiff => "public.tiff",
            Self::Jpeg => "public.jpeg",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Files,
    Promise,
    Image(ImageKind),
}

#[must_use]
pub fn classify(types: &[String], promise_types: &[String]) -> Option<Offer> {
    let has = |uti: &str| types.iter().any(|t| t == uti);
    if has(FILE_URL) || has(LEGACY_FILENAMES) {
        return Some(Offer::Files);
    }
    if types.iter().any(|t| promise_types.contains(t)) {
        return Some(Offer::Promise);
    }
    ImageKind::ALL
        .into_iter()
        .find(|kind| has(kind.uti()))
        .map(Offer::Image)
}

#[must_use]
pub fn accepted_types(promise_types: &[String]) -> Vec<String> {
    let mut types = vec![FILE_URL.to_owned(), LEGACY_FILENAMES.to_owned()];
    types.extend(promise_types.iter().cloned());
    types.extend(ImageKind::ALL.iter().map(|k| k.uti().to_owned()));
    types
}

#[must_use]
pub fn paste_text(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| crate::clipboard_store::dropped_path_text(&p.to_string_lossy()))
        .collect()
}

pub fn materialize_image(kind: ImageKind, bytes: &[u8], dir: &Path) -> std::io::Result<PathBuf> {
    DROP_TEMP.reap(dir, DROP_TEMP_MAX_AGE);
    let png = match kind {
        ImageKind::Png => Some(bytes.to_vec()),
        ImageKind::Tiff => tiff_to_png(bytes),
        ImageKind::Jpeg => None,
    };
    let (ext, body) = match (kind, png) {
        (_, Some(png)) => (".png", png),
        (ImageKind::Jpeg, None) => (".jpg", bytes.to_vec()),
        (_, None) => (".tiff", bytes.to_vec()),
    };
    let path = dir.join(DROP_TEMP.name(ext));
    std::fs::write(&path, body)?;
    Ok(path)
}

fn tiff_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Tiff).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

pub fn promise_dir(dir: &Path) -> std::io::Result<PathBuf> {
    DROP_TEMP.reap(dir, DROP_TEMP_MAX_AGE);
    let path = dir.join(DROP_TEMP.name(""));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

#[derive(Debug, Default)]
pub struct Inbox {
    batches: Mutex<Vec<Vec<PathBuf>>>,
}

impl Inbox {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            batches: Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, batch: Vec<PathBuf>) -> bool {
        if batch.is_empty() {
            return false;
        }
        match self.batches.lock() {
            Ok(mut batches) => {
                batches.push(batch);
                true
            }
            Err(poisoned) => {
                poisoned.into_inner().push(batch);
                true
            }
        }
    }

    pub fn post(&self, batch: Vec<PathBuf>) {
        if self.push(batch) {
            crate::ring::WINDOW.ring();
        }
    }

    #[must_use]
    pub fn take(&self) -> Vec<Vec<PathBuf>> {
        match self.batches.lock() {
            Ok(mut batches) => std::mem::take(&mut *batches),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

pub static INBOX: Inbox = Inbox::new();

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    const PROMISE: &[&str] = &[
        "com.apple.NSFilePromiseItemMetaData",
        "com.apple.pasteboard.promised-file-url",
    ];

    #[test]
    fn a_finder_drag_is_a_file_offer_by_either_type_name() {
        let promise = s(PROMISE);
        assert_eq!(classify(&s(&[FILE_URL]), &promise), Some(Offer::Files));
        assert_eq!(
            classify(&s(&[LEGACY_FILENAMES, FILE_URL]), &promise),
            Some(Offer::Files)
        );
    }

    #[test]
    fn a_screenshot_thumbnail_promise_with_no_file_url_is_accepted() {
        let promise = s(PROMISE);
        assert_eq!(
            classify(
                &s(&["com.apple.pasteboard.promised-file-url", "public.png"]),
                &promise
            ),
            Some(Offer::Promise)
        );
    }

    #[test]
    fn image_data_with_no_file_or_promise_is_accepted_by_kind() {
        let promise = s(PROMISE);
        for kind in ImageKind::ALL {
            assert_eq!(
                classify(&s(&[kind.uti()]), &promise),
                Some(Offer::Image(kind)),
                "{kind:?}"
            );
        }
        assert_eq!(
            classify(&s(&["public.tiff", "public.png"]), &promise),
            Some(Offer::Image(ImageKind::Png)),
            "png wins over tiff when both are offered"
        );
    }

    #[test]
    fn plain_text_drags_are_refused() {
        assert_eq!(classify(&s(&["public.utf8-plain-text"]), &s(PROMISE)), None);
    }

    #[test]
    fn the_window_registers_every_type_it_can_accept() {
        let promise = s(PROMISE);
        let accepted = accepted_types(&promise);
        for uti in [FILE_URL, LEGACY_FILENAMES]
            .into_iter()
            .chain(PROMISE.iter().copied())
            .chain(ImageKind::ALL.iter().map(|k| k.uti()))
        {
            assert!(accepted.iter().any(|t| t == uti), "{uti} not registered");
        }
        for uti in &accepted {
            assert!(
                classify(std::slice::from_ref(uti), &promise).is_some(),
                "{uti} registered but refused on entry"
            );
        }
    }

    #[test]
    fn several_files_become_one_space_separated_escaped_text() {
        let paths = [
            PathBuf::from("/Users/op/Screen Shot 1.png"),
            PathBuf::from("/Users/op/it's.png"),
        ];
        assert_eq!(
            paste_text(&paths),
            "/Users/op/Screen\\ Shot\\ 1.png /Users/op/it\\'s.png "
        );
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mado-file-drop-test-{}-{}",
            std::process::id(),
            DROP_TEMP.name("")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn png_data_materializes_to_an_owned_temp_png() {
        let dir = scratch();
        let path = materialize_image(ImageKind::Png, &png_bytes(), &dir).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(
            name.starts_with("mado-drop-") && name.ends_with(".png"),
            "{name}"
        );
        let img = image::open(&path).unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tiff_data_is_converted_to_png_so_a_tui_can_load_it() {
        let dir = scratch();
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 8, 7, 255]));
        let mut tiff = std::io::Cursor::new(Vec::new());
        img.write_to(&mut tiff, image::ImageFormat::Tiff).unwrap();
        let path = materialize_image(ImageKind::Tiff, &tiff.into_inner(), &dir).unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        let back = image::load_from_memory_with_format(
            &std::fs::read(&path).unwrap(),
            image::ImageFormat::Png,
        )
        .unwrap();
        assert_eq!((back.width(), back.height()), (4, 4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_promise_lands_in_its_own_owned_directory() {
        let dir = scratch();
        let promised = promise_dir(&dir).unwrap();
        assert!(promised.is_dir());
        assert!(
            promised
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("mado-drop-")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_inbox_keeps_one_batch_per_drop_and_ignores_empty_ones() {
        let inbox = Inbox::new();
        assert!(!inbox.push(Vec::new()));
        assert!(inbox.push(vec![PathBuf::from("/a"), PathBuf::from("/b")]));
        assert!(inbox.push(vec![PathBuf::from("/c")]));
        assert_eq!(
            inbox.take(),
            vec![
                vec![PathBuf::from("/a"), PathBuf::from("/b")],
                vec![PathBuf::from("/c")]
            ]
        );
        assert!(inbox.take().is_empty());
    }
}
