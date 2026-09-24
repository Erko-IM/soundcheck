//! Files the operating system asks soundcheck to open.
//!
//! macOS never passes them as arguments. Double-click, Open With and
//! dropping onto the Dock icon all send an Apple Event instead, at launch
//! and while running, and winit has no hook for it, so this installs the
//! handler itself. Windows and Linux start the app with the file as an
//! argument, which `main` reads.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, mpsc};

use eframe::egui;

type Wake = Arc<OnceLock<egui::Context>>;

pub struct Inbox {
    receiver: mpsc::Receiver<PathBuf>,
    wake: Wake,
    #[cfg(target_os = "macos")]
    _listener: mac::Listener,
}

impl Inbox {
    /// Must be made before the event loop starts, or the event that
    /// launched the app arrives before anything listens for it.
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let wake = Wake::default();
        #[cfg(target_os = "macos")]
        let _listener = mac::Listener::install(sender, Arc::clone(&wake));
        #[cfg(not(target_os = "macos"))]
        drop(sender);
        Self {
            receiver,
            wake,
            #[cfg(target_os = "macos")]
            _listener,
        }
    }

    /// Repaints `ctx` whenever a file arrives, so it opens without waiting
    /// for the next mouse movement.
    pub fn wake(&self, ctx: &egui::Context) {
        let _ = self.wake.set(ctx.clone());
    }

    /// The latest file asked for since the last call.
    pub fn latest(&self) -> Option<PathBuf> {
        self.receiver.try_iter().last()
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
    use objc2_app_kit::NSApplicationWillFinishLaunchingNotification;
    use objc2_core_services::{kAEOpenDocuments, kCoreEventClass, keyDirectObject, typeAEList};
    use objc2_foundation::{
        NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSNotificationCenter, NSObject,
    };

    use super::Wake;

    pub struct Ivars {
        sender: mpsc::Sender<PathBuf>,
        wake: Wake,
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and `Handler`
        // does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "SoundcheckOpenDocuments"]
        #[ivars = Ivars]
        pub struct Handler;

        impl Handler {
            // SAFETY: notification observers receive the notification.
            #[unsafe(method(willFinishLaunching:))]
            fn will_finish_launching(&self, _notification: &NSNotification) {
                // AppKit installs its own handler as it launches, just before
                // this notification, and delivers the event that launched the
                // app just after it: earlier would be overwritten, later
                // would miss that event.
                let this: &AnyObject = self.as_ref();
                // SAFETY: the selector names the method below, whose
                // signature is the one Apple Event handlers are called with.
                unsafe {
                    NSAppleEventManager::sharedAppleEventManager()
                        .setEventHandler_andSelector_forEventClass_andEventID(
                            this,
                            sel!(openDocuments:withReply:),
                            kCoreEventClass,
                            kAEOpenDocuments,
                        );
                }
            }

            // SAFETY: Apple Event handlers receive the event and its reply.
            #[unsafe(method(openDocuments:withReply:))]
            fn open_documents(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
                // One file at a time: of several, the first opens, and the
                // explorer shows the folder the others are usually in.
                if let Some(path) = files(event).into_iter().next() {
                    // The receiver only goes away with the window.
                    let _ = self.ivars().sender.send(path);
                    if let Some(ctx) = self.ivars().wake.get() {
                        ctx.request_repaint();
                    }
                }
            }
        }
    );

    /// The files an "open documents" event names.
    pub fn files(event: &NSAppleEventDescriptor) -> Vec<PathBuf> {
        let Some(direct) = event.paramDescriptorForKeyword(keyDirectObject) else {
            return Vec::new();
        };
        let items = if direct.descriptorType() == typeAEList {
            (1..=direct.numberOfItems())
                .filter_map(|i| direct.descriptorAtIndex(i))
                .collect()
        } else {
            vec![direct]
        };
        items
            .iter()
            .filter_map(|item| item.fileURLValue()?.to_file_path())
            .collect()
    }

    /// Keeps the handler registered while it lives. AppKit holds on to it
    /// only weakly, so dropping this also takes it out.
    pub struct Listener(Retained<Handler>);

    impl Listener {
        pub fn install(sender: mpsc::Sender<PathBuf>, wake: Wake) -> Self {
            let handler = Handler::alloc().set_ivars(Ivars { sender, wake });
            // SAFETY: NSObject's `init` takes no arguments.
            let handler: Retained<Handler> = unsafe { msg_send![super(handler), init] };
            let this: &AnyObject = handler.as_ref();
            // SAFETY: the selector names the observer method above, and the
            // name is AppKit's own constant.
            unsafe {
                NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                    this,
                    sel!(willFinishLaunching:),
                    Some(NSApplicationWillFinishLaunchingNotification),
                    None,
                );
            }
            Self(handler)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            NSAppleEventManager::sharedAppleEventManager()
                .removeEventHandlerForEventClass_andEventID(kCoreEventClass, kAEOpenDocuments);
            let this: &AnyObject = self.0.as_ref();
            // SAFETY: `this` is the observer registered in `install`.
            unsafe { NSNotificationCenter::defaultCenter().removeObserver(this) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use objc2_core_services::{kAnyTransactionID, kAutoGenerateReturnID};
        use objc2_foundation::NSURL;

        fn event(direct: &NSAppleEventDescriptor) -> Retained<NSAppleEventDescriptor> {
            let event = NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(
                kCoreEventClass,
                kAEOpenDocuments,
                None,
                kAutoGenerateReturnID as _,
                kAnyTransactionID as _,
            );
            event.setParamDescriptor_forKeyword(direct, keyDirectObject);
            event
        }

        fn file(path: &str) -> Retained<NSAppleEventDescriptor> {
            NSAppleEventDescriptor::descriptorWithFileURL(&NSURL::from_file_path(path).unwrap())
        }

        #[test]
        fn a_finder_selection_arrives_as_its_files() {
            let list = NSAppleEventDescriptor::listDescriptor();
            list.insertDescriptor_atIndex(&file("/Volumes/X8/Dawn chorus.wav"), 0);
            list.insertDescriptor_atIndex(&file("/Volumes/X8/Heron.flac"), 0);
            assert_eq!(
                files(&event(&list)),
                [
                    PathBuf::from("/Volumes/X8/Dawn chorus.wav"),
                    PathBuf::from("/Volumes/X8/Heron.flac")
                ]
            );
        }

        #[test]
        fn a_single_file_need_not_come_in_a_list() {
            let single = file("/tmp/take 3.wav");
            assert_eq!(files(&event(&single)), [PathBuf::from("/tmp/take 3.wav")]);
        }
    }
}
