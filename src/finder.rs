//! Files the operating system asks soundcheck to open, and on macOS its
//! requests to quit.
//!
//! macOS never passes files as arguments. Double-click, Open With and
//! dropping onto the Dock icon all send an Apple Event instead, at launch
//! and while running, and winit has no hook for it, so this installs the
//! handler itself. Windows and Linux start the app with the file as an
//! argument, which `main` reads.
//!
//! Quitting on macOS, with ⌘Q, from the Dock or by logging out, would end
//! the app at once, around the window's close request where unsaved changes
//! are asked about. Those requests come here instead, for the app to treat
//! as that close request.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};

use eframe::egui;

type Wake = Arc<OnceLock<egui::Context>>;

pub struct Inbox {
    receiver: mpsc::Receiver<PathBuf>,
    quit: Arc<AtomicBool>,
    wake: Wake,
    #[cfg(target_os = "macos")]
    listener: mac::Listener,
}

impl Inbox {
    /// Must be made before the event loop starts, or the event that
    /// launched the app arrives before anything listens for it.
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let quit = Arc::default();
        let wake = Wake::default();
        #[cfg(target_os = "macos")]
        let listener = mac::Listener::install(sender, Arc::clone(&quit), Arc::clone(&wake));
        #[cfg(not(target_os = "macos"))]
        drop(sender);
        Self {
            receiver,
            quit,
            wake,
            #[cfg(target_os = "macos")]
            listener,
        }
    }

    /// Ties the inbox to the window: `ctx` repaints whenever something
    /// arrives, so it is seen without waiting for the next mouse movement,
    /// and on macOS the app menu's Quit, there by the time the window is,
    /// comes here too.
    pub fn connect(&self, ctx: &egui::Context) {
        let _ = self.wake.set(ctx.clone());
        #[cfg(target_os = "macos")]
        self.listener.take_quit_menu();
    }

    /// The latest file asked for since the last call.
    pub fn latest(&self) -> Option<PathBuf> {
        self.receiver.try_iter().last()
    }

    /// Whether the app was asked to quit since the last call.
    pub fn quit_asked(&self) -> bool {
        self.quit.swap(false, Ordering::Relaxed)
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Sel};
    use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send, sel};
    use objc2_app_kit::{NSApplication, NSApplicationWillFinishLaunchingNotification, NSMenuItem};
    use objc2_core_services::{
        kAEOpenDocuments, kAEQuitApplication, kCoreEventClass, keyDirectObject, typeAEList,
    };
    use objc2_foundation::{
        NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSNotificationCenter, NSObject,
    };

    use super::Wake;

    pub struct Ivars {
        sender: mpsc::Sender<PathBuf>,
        quit: Arc<AtomicBool>,
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
                let manager = NSAppleEventManager::sharedAppleEventManager();
                for (selector, event) in [
                    (sel!(openDocuments:withReply:), kAEOpenDocuments),
                    (sel!(quit:withReply:), kAEQuitApplication),
                ] {
                    // SAFETY: each selector names a method below, whose
                    // signature is the one Apple Event handlers are called
                    // with.
                    unsafe {
                        manager.setEventHandler_andSelector_forEventClass_andEventID(
                            this,
                            selector,
                            kCoreEventClass,
                            event,
                        );
                    }
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
                    self.wake();
                }
            }

            // SAFETY: Apple Event handlers receive the event and its reply.
            #[unsafe(method(quit:withReply:))]
            fn quit_event(&self, _event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
                self.ask_to_quit();
            }

            // SAFETY: a menu item's action receives the item.
            #[unsafe(method(quitFromMenu:))]
            fn quit_from_menu(&self, _item: &AnyObject) {
                self.ask_to_quit();
            }
        }
    );

    impl Handler {
        fn wake(&self) {
            if let Some(ctx) = self.ivars().wake.get() {
                ctx.request_repaint();
            }
        }

        fn ask_to_quit(&self) {
            self.ivars().quit.store(true, Ordering::Relaxed);
            self.wake();
        }
    }

    /// Every item of the app's menus whose action is `action`.
    fn menu_items(mtm: MainThreadMarker, action: Sel) -> Vec<Retained<NSMenuItem>> {
        let Some(menu) = NSApplication::sharedApplication(mtm).mainMenu() else {
            return Vec::new();
        };
        menu.itemArray()
            .to_vec()
            .iter()
            .filter_map(|top| top.submenu())
            .flat_map(|submenu| submenu.itemArray().to_vec())
            .filter(|item| item.action() == Some(action))
            .collect()
    }

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
        pub fn install(sender: mpsc::Sender<PathBuf>, quit: Arc<AtomicBool>, wake: Wake) -> Self {
            let handler = Handler::alloc().set_ivars(Ivars { sender, quit, wake });
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

        /// Points the app menu's Quit, which ends the app on the spot, at
        /// the handler instead.
        pub fn take_quit_menu(&self) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let this: &AnyObject = self.0.as_ref();
            for item in menu_items(mtm, sel!(terminate:)) {
                // SAFETY: the handler has a method for the action, taking the
                // item, and the item lets go of it before it goes, in `drop`.
                unsafe {
                    item.setTarget(Some(this));
                    item.setAction(Some(sel!(quitFromMenu:)));
                }
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let manager = NSAppleEventManager::sharedAppleEventManager();
            for event in [kAEOpenDocuments, kAEQuitApplication] {
                manager.removeEventHandlerForEventClass_andEventID(kCoreEventClass, event);
            }
            let this: &AnyObject = self.0.as_ref();
            if let Some(mtm) = MainThreadMarker::new() {
                for item in menu_items(mtm, sel!(quitFromMenu:)) {
                    if item.target().is_some_and(|t| std::ptr::eq(&*t, this)) {
                        // SAFETY: `terminate:` is NSApplication's own action,
                        // and with no target the item sends it to the app.
                        unsafe {
                            item.setTarget(None);
                            item.setAction(Some(sel!(terminate:)));
                        }
                    }
                }
            }
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

        #[test]
        fn a_request_to_quit_reaches_the_app_instead_of_ending_it() {
            let (sender, _receiver) = mpsc::channel();
            let quit = Arc::new(AtomicBool::new(false));
            let listener = Listener::install(sender, Arc::clone(&quit), Wake::default());
            let request = NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(
                kCoreEventClass,
                kAEQuitApplication,
                None,
                kAutoGenerateReturnID as _,
                kAnyTransactionID as _,
            );
            let reply = NSAppleEventDescriptor::nullDescriptor();
            // SAFETY: the handler implements the selector, with this signature.
            let () = unsafe { msg_send![&*listener.0, quit: &*request, withReply: &*reply] };
            assert!(quit.swap(false, Ordering::Relaxed));
            // SAFETY: as above, for the menu item's action.
            let () = unsafe { msg_send![&*listener.0, quitFromMenu: &*reply] };
            assert!(quit.load(Ordering::Relaxed));
        }
    }
}
