// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The alacritty [`EventListener`] both editions attach to their `Term`.
//!
//! Alacritty calls `send_event(Event::PtyWrite(text))` whenever the VT parser
//! produces a reply that has to travel back into the PTY's stdin — Device
//! Status Report (`ESC[6n`), primary device attributes, window-size queries,
//! colour queries, and so on. The default trait impl is a no-op, which
//! silently drops those replies and breaks anything that waits on them
//! (reedline times out on its cursor-position probe, for instance). This proxy
//! holds a slot for the `EventLoopSender` that the caller fills in once
//! `EventLoop::spawn` has handed it back; until then events are buffered into
//! the void, which is fine because no PTY exists to read them yet.
//!
//! The GUI edition additionally answers OSC colour queries (from the tab's
//! live theme) and flips an `exited` flag on `ChildExit`; those fields and the
//! arms that use them are `gui`-gated, so the headless daemon compiles to the
//! exact PtyWrite-only proxy it had before this was shared.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener};
use alacritty_terminal::event_loop::{EventLoopSender, Msg};

#[cfg(feature = "gui")]
use crate::theme::ThemeName;

#[derive(Clone, Default)]
pub struct EventProxy {
    notifier: Arc<Mutex<Option<EventLoopSender>>>,
    /// Active theme, so OSC colour queries (see `send_event`) answer with the
    /// palette the tab is actually painted in. Kept in sync by
    /// `TerminalView::set_theme`. `Arc<Mutex<_>>` because the proxy is cloned
    /// into the parser thread.
    #[cfg(feature = "gui")]
    theme: Arc<Mutex<ThemeName>>,
    /// Flipped by `ChildExit` — alacritty's event loop already watches the PTY
    /// child, so the shell's death arrives as an event instead of a `/proc`
    /// poll. Shared with `TerminalView::exited`.
    #[cfg(feature = "gui")]
    pub exited: Arc<std::sync::atomic::AtomicBool>,
}

impl EventProxy {
    pub fn set_notifier(&self, sender: EventLoopSender) {
        if let Ok(mut slot) = self.notifier.lock() {
            *slot = Some(sender);
        }
    }

    #[cfg(feature = "gui")]
    pub fn set_theme(&self, theme: ThemeName) {
        if let Ok(mut t) = self.theme.lock() {
            *t = theme;
        }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        let bytes: Vec<u8> = match event {
            AlacrittyEvent::PtyWrite(text) => text.into_bytes(),
            #[cfg(feature = "gui")]
            AlacrittyEvent::ChildExit(_) => {
                self.exited.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            // Answer OSC colour queries (OSC 4 palette / 10 fg / 11 bg /
            // 12 cursor). Without a reply the query times out and the app
            // assumes a default (near-black) background — Claude Code then
            // computes its diff highlight colours for that imagined bg, and
            // those clash with our real navy theme (added lines render a
            // blue that nearly matches the background). Replying with the
            // actual palette lets the app blend against the right bg.
            #[cfg(feature = "gui")]
            AlacrittyEvent::ColorRequest(index, formatter) => {
                let theme = self.theme.lock().map_or_else(|_| ThemeName::default(), |t| *t);
                formatter(crate::theme::theme(theme).color_index_to_rgb(index)).into_bytes()
            }
            _ => return,
        };
        if let Ok(slot) = self.notifier.lock()
            && let Some(sender) = slot.as_ref()
        {
            let _ = sender.send(Msg::Input(bytes.into()));
        }
    }
}
