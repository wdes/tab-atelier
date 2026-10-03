// SPDX-License-Identifier: MPL-2.0

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
    /// Per-tab background tint (`#RRGGBB` packed), mirrored from
    /// `TerminalView::set_bg_override`. An app that asks for the background
    /// (OSC 11) must be told the colour we actually paint, or it computes its
    /// own highlights against the theme's colour and they clash — which is
    /// exactly what happens to Claude Code's submitted input line.
    #[cfg(feature = "gui")]
    bg_override: Arc<Mutex<Option<u32>>>,
    /// Flipped by `ChildExit` — alacritty's event loop already watches the PTY
    /// child, so the shell's death arrives as an event instead of a `/proc`
    /// poll. Shared with `TerminalView::exited`.
    #[cfg(feature = "gui")]
    pub exited: Arc<std::sync::atomic::AtomicBool>,
    /// Text a program in the tab asked to put on the clipboard (OSC 52).
    ///
    /// A slot rather than a direct copy, because the copy has to happen on the UI
    /// thread: writing to the clipboard needs gpui's `App`, and this callback runs on
    /// the parser thread with none. So it is filled here and drained by the app's
    /// sweep, the same arrangement as [`Self::exited`].
    ///
    /// `Option` rather than a bool because the *text* is the message. A second write
    /// before the drain replaces the first, which is the right resolution: a clipboard
    /// holds one value, and the later copy is the one the operator just asked for.
    #[cfg(feature = "gui")]
    pub clipboard: Arc<Mutex<Option<String>>>,
}

impl EventProxy {
    pub fn set_notifier(&self, sender: EventLoopSender) {
        if let Ok(mut slot) = self.notifier.lock() {
            *slot = Some(sender);
        }
    }

    #[cfg(feature = "gui")]
    pub fn set_bg_override(&self, rgb: Option<u32>) {
        if let Ok(mut b) = self.bg_override.lock() {
            *b = rgb;
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
            // A clipboard **write** — OSC 52 — from whatever is running in the tab.
            //
            // catbus-agent uses this to put its raw markdown on the clipboard, because it
            // is a TUI and has no clipboard of its own: the clipboard belongs to the
            // terminal it runs inside, and this is the sequence asking for it. The text
            // arrives already decoded by alacritty — it strips the base64 and the
            // terminator — so it is stored as it is.
            //
            // Deliberately *not* routed through the input channel below: that sends
            // bytes **to** the pty as though they had been typed, which would write the
            // clipboard value back into the child.
            #[cfg(feature = "gui")]
            AlacrittyEvent::ClipboardStore(_, text) => {
                if let Ok(mut slot) = self.clipboard.lock() {
                    *slot = Some(text);
                }
                return;
            }
            // Answer OSC colour queries (OSC 4 palette / 10 fg / 11 bg /
            // 12 cursor).
            //
            // Without a reply the query times out and the app
            // assumes a default (near-black) background — Claude Code then
            // computes its diff highlight colours for that imagined bg, and
            // those clash with our real navy theme (added lines render a
            // blue that nearly matches the background). Replying with the
            // actual palette lets the app blend against the right bg.
            #[cfg(feature = "gui")]
            AlacrittyEvent::ColorRequest(index, formatter) => {
                let theme = self.theme.lock().map_or_else(|_| ThemeName::default(), |t| *t);
                let tint = self.bg_override.lock().map_or(None, |b| *b);
                formatter(crate::theme::theme(theme).with_term_bg(tint).color_index_to_rgb(index)).into_bytes()
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
