#![deny(unsafe_code)]

use std::path::PathBuf;
use std::sync::Once;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSDraggingInfo, NSFilePromiseReceiver, NSPasteboard, NSWindow};
use objc2_foundation::{NSArray, NSString, NSURL};

use super::{Offer, Payload};

pub enum Received {
    Payload(Payload),
    Promise(Vec<Retained<NSFilePromiseReceiver>>),
}

fn strings(array: &NSArray<NSString>) -> Vec<String> {
    array.iter().map(|s| s.to_string()).collect()
}

fn promise_types() -> Vec<String> {
    strings(&NSFilePromiseReceiver::readableDraggedTypes())
}

fn pasteboard_types(pb: &NSPasteboard) -> Vec<String> {
    pb.types().map(|t| strings(&t)).unwrap_or_default()
}

#[must_use]
pub fn offer(pb: &NSPasteboard) -> Option<Offer> {
    super::classify(&pasteboard_types(pb), &promise_types())
}

fn file_urls(pb: &NSPasteboard) -> Vec<PathBuf> {
    let file_url = NSString::from_str(super::FILE_URL);
    let from_items: Vec<PathBuf> = pb
        .pasteboardItems()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.stringForType(&file_url))
                .filter_map(|s| NSURL::URLWithString(&s))
                .filter_map(|url| url.filePathURL().unwrap_or(url).path())
                .map(|p| PathBuf::from(p.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !from_items.is_empty() {
        return from_items;
    }
    seam::legacy_filenames(pb)
}

#[must_use]
pub fn receive(pb: &NSPasteboard) -> Option<Received> {
    match offer(pb)? {
        Offer::Files => {
            let files = file_urls(pb);
            (!files.is_empty()).then_some(Received::Payload(Payload::Files(files)))
        }
        Offer::Promise => {
            let receivers = seam::promise_receivers(pb);
            (!receivers.is_empty()).then_some(Received::Promise(receivers))
        }
        Offer::Image(kind) => pb
            .dataForType(&NSString::from_str(kind.uti()))
            .map(|data| Received::Payload(Payload::Image(kind, data.to_vec()))),
    }
}

#[must_use]
pub fn general_payload() -> Option<Payload> {
    pasteboard_payload(&NSPasteboard::generalPasteboard())
}

#[must_use]
pub fn pasteboard_payload(pb: &NSPasteboard) -> Option<Payload> {
    match offer(pb)? {
        Offer::Promise => None,
        Offer::Files | Offer::Image(_) => match receive(pb)? {
            Received::Payload(payload) => Some(payload),
            Received::Promise(_) => None,
        },
    }
}

fn perform(pb: &NSPasteboard) -> bool {
    let dir = std::env::temp_dir();
    match receive(pb) {
        Some(Received::Payload(payload)) => match super::land(payload, &dir) {
            Ok(paths) => {
                super::INBOX.post(paths);
                true
            }
            Err(error) => {
                tracing::warn!(%error, "dropped image could not be written");
                false
            }
        },
        Some(Received::Promise(receivers)) => match super::promise_dir(&dir) {
            Ok(dest) => {
                seam::receive_promises(&receivers, &dest);
                true
            }
            Err(error) => {
                tracing::warn!(%error, "no directory for the promised drop");
                false
            }
        },
        None => false,
    }
}

fn entered(sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    offer(&sender.draggingPasteboard()).is_some()
}

fn performed(sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    perform(&sender.draggingPasteboard())
}

static TAKEOVER: Once = Once::new();

pub fn install(window: &NSWindow) {
    TAKEOVER.call_once(|| {
        if !seam::take_over_winit_drops() {
            tracing::warn!(
                "winit window delegate not found; drops stay on winit's filename-only path"
            );
        }
    });
    if !seam::taken_over() {
        return;
    }
    let types: Vec<Retained<NSString>> = super::accepted_types(&promise_types())
        .iter()
        .map(|t| NSString::from_str(t))
        .collect();
    window.registerForDraggedTypes(&NSArray::from_retained_slice(&types));
}

#[allow(unsafe_code)]
mod seam {
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, Ordering};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, ProtocolObject, Sel};
    use objc2::{ClassType, sel};
    #[allow(deprecated)]
    use objc2_app_kit::NSFilenamesPboardType;
    use objc2_app_kit::{NSDragOperation, NSDraggingInfo, NSFilePromiseReceiver, NSPasteboard};
    use objc2_foundation::{NSArray, NSDictionary, NSError, NSOperationQueue, NSString, NSURL};

    static TAKEN: AtomicBool = AtomicBool::new(false);

    pub(super) fn taken_over() -> bool {
        TAKEN.load(Ordering::Acquire)
    }

    extern "C-unwind" fn dragging_entered(
        _this: &AnyObject,
        _cmd: Sel,
        sender: &ProtocolObject<dyn NSDraggingInfo>,
    ) -> NSDragOperation {
        if super::entered(sender) {
            NSDragOperation::Copy
        } else {
            NSDragOperation::None
        }
    }

    extern "C-unwind" fn perform_drag_operation(
        _this: &AnyObject,
        _cmd: Sel,
        sender: &ProtocolObject<dyn NSDraggingInfo>,
    ) -> Bool {
        Bool::new(super::performed(sender))
    }

    type EnteredFn = extern "C-unwind" fn(
        &AnyObject,
        Sel,
        &ProtocolObject<dyn NSDraggingInfo>,
    ) -> NSDragOperation;
    type PerformFn =
        extern "C-unwind" fn(&AnyObject, Sel, &ProtocolObject<dyn NSDraggingInfo>) -> Bool;

    pub(super) fn take_over_winit_drops() -> bool {
        take_over(c"WinitWindowDelegate")
    }

    fn take_over(class_name: &std::ffi::CStr) -> bool {
        let Some(class) = AnyClass::get(class_name) else {
            return false;
        };
        let (Some(entered), Some(perform)) = (
            class.instance_method(sel!(draggingEntered:)),
            class.instance_method(sel!(performDragOperation:)),
        ) else {
            return false;
        };
        let entered_imp: EnteredFn = dragging_entered;
        let perform_imp: PerformFn = perform_drag_operation;
        unsafe {
            entered.set_implementation(std::mem::transmute::<EnteredFn, Imp>(entered_imp));
            perform.set_implementation(std::mem::transmute::<PerformFn, Imp>(perform_imp));
        }
        TAKEN.store(true, Ordering::Release);
        true
    }

    #[allow(deprecated)]
    pub(super) fn legacy_filenames(pb: &NSPasteboard) -> Vec<PathBuf> {
        let Some(list) = pb.propertyListForType(unsafe { NSFilenamesPboardType }) else {
            return Vec::new();
        };
        let Ok(array) = list.downcast::<NSArray>() else {
            return Vec::new();
        };
        array
            .iter()
            .filter_map(|obj| obj.downcast::<NSString>().ok())
            .map(|s| PathBuf::from(s.to_string()))
            .collect()
    }

    pub(super) fn promise_receivers(pb: &NSPasteboard) -> Vec<Retained<NSFilePromiseReceiver>> {
        let classes = NSArray::from_slice(&[NSFilePromiseReceiver::class()]);
        let Some(found) = (unsafe { pb.readObjectsForClasses_options(&classes, None) }) else {
            return Vec::new();
        };
        found
            .iter()
            .filter_map(|obj| obj.downcast::<NSFilePromiseReceiver>().ok())
            .collect()
    }

    pub(super) fn receive_promises(receivers: &[Retained<NSFilePromiseReceiver>], dest: &Path) {
        let dest_url = NSURL::fileURLWithPath(&NSString::from_str(&dest.to_string_lossy()));
        let options = NSDictionary::new();
        let queue = NSOperationQueue::mainQueue();
        let reader = RcBlock::new(|url: NonNull<NSURL>, error: *mut NSError| {
            if !error.is_null() {
                let error = unsafe { &*error };
                tracing::warn!(error = %error.localizedDescription(), "promised drop failed");
                return;
            }
            let url = unsafe { url.as_ref() };
            if let Some(path) = url.path() {
                super::super::INBOX.post(vec![PathBuf::from(path.to_string())]);
            }
        });
        for receiver in receivers {
            unsafe {
                receiver.receivePromisedFilesAtDestination_options_operationQueue_reader(
                    &dest_url, &options, &queue, &reader,
                );
            }
        }
    }

    #[cfg(test)]
    #[allow(deprecated)]
    pub(super) fn write_legacy_filenames(pb: &NSPasteboard, paths: &[&str]) -> bool {
        let names: Vec<Retained<NSString>> = paths.iter().map(|p| NSString::from_str(p)).collect();
        let array = NSArray::from_retained_slice(&names);
        unsafe { pb.setPropertyList_forType(&array, NSFilenamesPboardType) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use objc2::msg_send;
        use objc2::runtime::{ClassBuilder, NSObject};
        use objc2_foundation::NSData;

        extern "C-unwind" fn winit_entered(
            _this: &AnyObject,
            _cmd: Sel,
            _sender: &AnyObject,
        ) -> NSDragOperation {
            NSDragOperation::None
        }

        extern "C-unwind" fn winit_perform(
            _this: &AnyObject,
            _cmd: Sel,
            _sender: &AnyObject,
        ) -> Bool {
            Bool::NO
        }

        thread_local! {
            static OFFERED: std::cell::RefCell<Option<Retained<NSPasteboard>>> =
                const { std::cell::RefCell::new(None) };
        }

        extern "C-unwind" fn offered_pasteboard(_this: &AnyObject, _cmd: Sel) -> *mut NSPasteboard {
            OFFERED.with(|pb| {
                pb.borrow()
                    .clone()
                    .map_or(std::ptr::null_mut(), Retained::autorelease_return)
            })
        }

        fn stand_in_delegate() -> Retained<AnyObject> {
            static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
            let name = c"MadoDropStandInDelegate";
            let class = *CLASS.get_or_init(|| {
                let mut builder = ClassBuilder::new(name, NSObject::class()).unwrap();
                unsafe {
                    builder.add_method(
                        sel!(draggingEntered:),
                        winit_entered as extern "C-unwind" fn(_, _, _) -> _,
                    );
                    builder.add_method(
                        sel!(performDragOperation:),
                        winit_perform as extern "C-unwind" fn(_, _, _) -> _,
                    );
                }
                builder.register()
            });
            assert!(take_over(name));
            unsafe { msg_send![class, new] }
        }

        fn dragging_info(pb: Retained<NSPasteboard>) -> Retained<AnyObject> {
            OFFERED.with(|slot| *slot.borrow_mut() = Some(pb));
            static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
            let name = c"MadoDropStandInInfo";
            let class = *CLASS.get_or_init(|| {
                let mut builder = ClassBuilder::new(name, NSObject::class()).unwrap();
                unsafe {
                    builder.add_method(
                        sel!(draggingPasteboard),
                        offered_pasteboard as extern "C-unwind" fn(_, _) -> _,
                    );
                }
                builder.register()
            });
            unsafe { msg_send![class, new] }
        }

        #[test]
        fn the_taken_over_delegate_accepts_and_lands_a_png_only_drag() {
            let delegate = stand_in_delegate();
            let pb = NSPasteboard::pasteboardWithUniqueName();
            pb.clearContents();
            let png = NSString::from_str(crate::file_drop::ImageKind::Png.uti());
            assert!(pb.setData_forType(Some(&NSData::with_bytes(b"\x89PNG-probe")), &png));
            let info = dragging_info(pb);
            let entered: NSDragOperation =
                unsafe { msg_send![&*delegate, draggingEntered: &*info] };
            assert_eq!(
                entered,
                NSDragOperation::Copy,
                "winit's filename-only answer still in place"
            );
            let _ = crate::file_drop::INBOX.take();
            let performed: Bool = unsafe { msg_send![&*delegate, performDragOperation: &*info] };
            assert!(performed.as_bool());
            let batches = crate::file_drop::INBOX.take();
            assert_eq!(batches.len(), 1);
            let path = &batches[0][0];
            assert!(
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("mado-drop-"),
                "{path:?}"
            );
            assert_eq!(std::fs::read(path).unwrap(), b"\x89PNG-probe");
            let _ = std::fs::remove_file(path);
        }

        #[test]
        fn the_taken_over_delegate_refuses_plain_text() {
            let delegate = stand_in_delegate();
            let pb = NSPasteboard::pasteboardWithUniqueName();
            pb.clearContents();
            assert!(pb.setString_forType(
                &NSString::from_str("hi"),
                &NSString::from_str("public.utf8-plain-text")
            ));
            let info = dragging_info(pb);
            let entered: NSDragOperation =
                unsafe { msg_send![&*delegate, draggingEntered: &*info] };
            assert_eq!(entered, NSDragOperation::None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_drop::ImageKind;
    use objc2_foundation::NSData;

    fn private_pasteboard() -> Retained<NSPasteboard> {
        let pb = NSPasteboard::pasteboardWithUniqueName();
        pb.clearContents();
        pb
    }

    #[test]
    fn a_pasteboard_holding_only_png_data_is_received_as_an_image() {
        let pb = private_pasteboard();
        let png = NSString::from_str(ImageKind::Png.uti());
        assert!(pb.setData_forType(Some(&NSData::with_bytes(b"\x89PNG")), &png));
        match receive(&pb) {
            Some(Received::Payload(Payload::Image(ImageKind::Png, bytes))) => {
                assert_eq!(bytes, b"\x89PNG");
            }
            _ => panic!("png-only drag refused"),
        }
        pb.clearContents();
    }

    #[test]
    fn a_pasteboard_of_file_urls_is_received_as_every_path() {
        let pb = private_pasteboard();
        let dir = std::env::temp_dir();
        let a = dir.join(super::super::DROP_TEMP.name(" a b.png"));
        let b = dir.join(super::super::DROP_TEMP.name("-画像.png"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        let urls: Vec<Retained<NSURL>> = [&a, &b]
            .iter()
            .map(|p| NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy())))
            .collect();
        let objects: Vec<&ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>> = urls
            .iter()
            .map(|u| ProtocolObject::from_ref(&**u))
            .collect();
        assert!(pb.writeObjects(&NSArray::from_slice(&objects)));
        match receive(&pb) {
            Some(Received::Payload(Payload::Files(files))) => assert_eq!(
                files
                    .iter()
                    .map(|f| std::fs::canonicalize(f).unwrap())
                    .collect::<Vec<_>>(),
                vec![
                    std::fs::canonicalize(&a).unwrap(),
                    std::fs::canonicalize(&b).unwrap()
                ]
            ),
            _ => panic!("file-url drag refused"),
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        pb.clearContents();
    }

    #[test]
    fn the_legacy_filenames_type_alone_still_reaches_the_paths() {
        let pb = private_pasteboard();
        assert!(seam::write_legacy_filenames(
            &pb,
            &["/tmp/x y.png", "/tmp/z.png"]
        ));
        match receive(&pb) {
            Some(Received::Payload(Payload::Files(files))) => assert_eq!(
                files,
                vec![PathBuf::from("/tmp/x y.png"), PathBuf::from("/tmp/z.png")]
            ),
            _ => panic!("legacy filenames refused"),
        }
        pb.clearContents();
    }

    #[test]
    fn a_finder_copy_reads_as_files_even_beside_its_filename_text() {
        let pb = private_pasteboard();
        let url = NSURL::fileURLWithPath(&NSString::from_str("/tmp/copied shot.png"));
        let objects: Vec<&ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>> =
            vec![ProtocolObject::from_ref(&*url)];
        assert!(pb.writeObjects(&NSArray::from_slice(&objects)));
        pb.setString_forType(
            &NSString::from_str("copied shot.png"),
            &NSString::from_str("public.utf8-plain-text"),
        );
        assert_eq!(
            pasteboard_payload(&pb),
            Some(Payload::Files(vec![PathBuf::from("/tmp/copied shot.png")]))
        );
        pb.clearContents();
    }

    #[test]
    fn a_screenshot_on_the_clipboard_reads_as_its_image_bytes() {
        let pb = private_pasteboard();
        let tiff = NSString::from_str(ImageKind::Tiff.uti());
        assert!(pb.setData_forType(Some(&NSData::with_bytes(b"II*\0")), &tiff));
        assert_eq!(
            pasteboard_payload(&pb),
            Some(Payload::Image(ImageKind::Tiff, b"II*\0".to_vec()))
        );
        pb.clearContents();
    }

    #[test]
    fn a_text_only_clipboard_has_no_attachment_payload() {
        let pb = private_pasteboard();
        assert!(pb.setString_forType(
            &NSString::from_str("words"),
            &NSString::from_str("public.utf8-plain-text")
        ));
        assert_eq!(pasteboard_payload(&pb), None);
        pb.clearContents();
    }

    #[test]
    fn plain_text_is_not_a_drop() {
        let pb = private_pasteboard();
        assert!(pb.setString_forType(
            &NSString::from_str("hello"),
            &NSString::from_str("public.utf8-plain-text")
        ));
        assert!(receive(&pb).is_none());
        pb.clearContents();
    }
}
