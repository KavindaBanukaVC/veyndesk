//! Remote input injection + clipboard sync, on a dedicated thread
//! (enigo / arboard handles are not `Send` on every platform).

use crate::{video::Shared, Outgoing};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use std::{
    collections::HashSet,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};
use velodesk_protocol::{HostEvent, HostToServer, ViewerInput};

pub enum InputCmd {
    Event(u32, ViewerInput),
    /// A session ended: release anything it was holding down.
    SessionEnded(u32),
}

pub fn spawn_input(
    shared: Arc<Shared>,
    out: UnboundedSender<Outgoing>,
    active_sessions: Arc<Mutex<HashSet<u32>>>,
) -> mpsc::Sender<InputCmd> {
    let (tx, rx) = mpsc::channel::<InputCmd>();
    std::thread::Builder::new()
        .name("velodesk-input".into())
        .spawn(move || input_loop(rx, shared, out, active_sessions))
        .expect("spawn input thread");
    tx
}

fn input_loop(
    rx: mpsc::Receiver<InputCmd>,
    shared: Arc<Shared>,
    out: UnboundedSender<Outgoing>,
    active_sessions: Arc<Mutex<HashSet<u32>>>,
) {
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => Some(e),
        Err(e) => {
            warn!("input injection unavailable: {e}");
            None
        }
    };
    let mut clipboard = arboard::Clipboard::new().ok();
    let mut last_clip: Option<String> = clipboard.as_mut().and_then(|c| c.get_text().ok());
    let mut last_poll = Instant::now();
    let mut held_keys: HashSet<String> = HashSet::new();
    let mut held_buttons: HashSet<u8> = HashSet::new();

    loop {
        let cmd = rx.recv_timeout(Duration::from_millis(500));

        // Host → viewer clipboard sync (polled; cheap).
        if last_poll.elapsed() > Duration::from_millis(800) {
            last_poll = Instant::now();
            let sessions: Vec<u32> = active_sessions.lock().unwrap().iter().copied().collect();
            if !sessions.is_empty() {
                if let Some(text) = clipboard.as_mut().and_then(|c| c.get_text().ok()) {
                    if Some(&text) != last_clip.as_ref() && text.len() < 1 << 20 {
                        last_clip = Some(text.clone());
                        for sid in sessions {
                            let m = HostToServer::ToViewer { sid, msg: HostEvent::Clipboard { text: text.clone() } };
                            let _ = out.send(Outgoing::Text(serde_json::to_string(&m).unwrap()));
                        }
                    }
                }
            }
        }

        let cmd = match cmd {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let Some(en) = enigo.as_mut() else { continue };

        match cmd {
            InputCmd::SessionEnded(_) => {
                for code in held_keys.drain() {
                    if let Some(k) = map_key(&code, "") {
                        let _ = en.key(k, Direction::Release);
                    }
                }
                for b in held_buttons.drain() {
                    let _ = en.button(map_button(b), Direction::Release);
                }
            }
            InputCmd::Event(_sid, ev) => {
                let r = match ev {
                    ViewerInput::MouseMove { x, y } => {
                        let (ox, oy) = *shared.origin.lock().unwrap();
                        let (w, h) = *shared.size.lock().unwrap();
                        let x = x.clamp(0, w.saturating_sub(1) as i32);
                        let y = y.clamp(0, h.saturating_sub(1) as i32);
                        en.move_mouse(ox + x, oy + y, Coordinate::Abs)
                    }
                    ViewerInput::MouseButton { button, down } => {
                        if down { held_buttons.insert(button); } else { held_buttons.remove(&button); }
                        en.button(map_button(button), dir(down))
                    }
                    ViewerInput::Wheel { dx, dy } => {
                        let a = if dy != 0 { en.scroll(dy, Axis::Vertical) } else { Ok(()) };
                        if dx != 0 { en.scroll(dx, Axis::Horizontal) } else { a }
                    }
                    ViewerInput::Key { code, key, down } => match map_key(&code, &key) {
                        Some(k) => {
                            if down { held_keys.insert(code); } else { held_keys.remove(&code); }
                            en.key(k, dir(down))
                        }
                        None => {
                            debug!("unmapped key {code} / {key}");
                            Ok(())
                        }
                    },
                    ViewerInput::Text { text } => en.text(&text),
                    ViewerInput::Clipboard { text } => {
                        if let Some(c) = clipboard.as_mut() {
                            let _ = c.set_text(text.clone());
                        }
                        last_clip = Some(text);
                        Ok(())
                    }
                    ViewerInput::SpecialKey { name } => special(en, &name),
                    // Video-related messages are routed to the video thread, not here.
                    ViewerInput::Ack { .. } | ViewerInput::Refresh | ViewerInput::SelectMonitor { .. } => Ok(()),
                };
                if let Err(e) = r {
                    debug!("input error: {e}");
                }
            }
        }
    }
}

fn dir(down: bool) -> Direction {
    if down { Direction::Press } else { Direction::Release }
}

fn map_button(b: u8) -> Button {
    match b {
        1 => Button::Middle,
        2 => Button::Right,
        3 => Button::Back,
        4 => Button::Forward,
        _ => Button::Left,
    }
}

fn special(en: &mut Enigo, name: &str) -> enigo::InputResult<()> {
    let combo: &[&str] = match name {
        // Note: real Ctrl+Alt+Del (the Secure Attention Sequence) can only be raised by a
        // SYSTEM service via SendSAS(); see ROADMAP. This sends the key combo.
        "ctrl_alt_del" => &["ControlLeft", "AltLeft", "Delete"],
        "alt_tab" => &["AltLeft", "Tab"],
        "win" => &["MetaLeft"],
        "win_d" => &["MetaLeft", "KeyD"],
        "win_r" => &["MetaLeft", "KeyR"],
        "ctrl_shift_esc" => &["ControlLeft", "ShiftLeft", "Escape"],
        "print_screen" => &["PrintScreen"],
        _ => &[],
    };
    let keys: Vec<Key> = combo.iter().filter_map(|c| map_key(c, "")).collect();
    for k in &keys {
        en.key(*k, Direction::Press)?;
    }
    for k in keys.iter().rev() {
        en.key(*k, Direction::Release)?;
    }
    Ok(())
}

/// DOM `KeyboardEvent.code` → enigo key.
/// On Windows we inject physical virtual-key codes so layouts / shortcuts behave natively.
#[cfg(windows)]
pub fn map_key(code: &str, _key: &str) -> Option<Key> {
    Some(Key::Other(code_to_vk(code)?))
}

#[cfg(windows)]
pub fn code_to_vk(code: &str) -> Option<u32> {
    if let Some(c) = code.strip_prefix("Key") {
        let ch = c.chars().next()?;
        if c.len() == 1 && ch.is_ascii_uppercase() {
            return Some(ch as u32); // VK_A..VK_Z == 'A'..'Z'
        }
    }
    if let Some(d) = code.strip_prefix("Digit") {
        let ch = d.chars().next()?;
        if d.len() == 1 && ch.is_ascii_digit() {
            return Some(ch as u32); // VK_0..VK_9
        }
    }
    if let Some(n) = code.strip_prefix("Numpad").and_then(|d| d.parse::<u32>().ok()) {
        return Some(0x60 + n);
    }
    if let Some(n) = code.strip_prefix('F').and_then(|d| d.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x6F + n);
        }
    }
    Some(match code {
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Enter" | "NumpadEnter" => 0x0D,
        "ShiftLeft" => 0xA0,
        "ShiftRight" => 0xA1,
        "ControlLeft" => 0xA2,
        "ControlRight" => 0xA3,
        "AltLeft" => 0xA4,
        "AltRight" => 0xA5,
        "Pause" => 0x13,
        "CapsLock" => 0x14,
        "Escape" => 0x1B,
        "Space" => 0x20,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "End" => 0x23,
        "Home" => 0x24,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "PrintScreen" => 0x2C,
        "Insert" => 0x2D,
        "Delete" => 0x2E,
        "MetaLeft" | "OSLeft" => 0x5B,
        "MetaRight" | "OSRight" => 0x5C,
        "ContextMenu" => 0x5D,
        "NumpadMultiply" => 0x6A,
        "NumpadAdd" => 0x6B,
        "NumpadSubtract" => 0x6D,
        "NumpadDecimal" => 0x6E,
        "NumpadDivide" => 0x6F,
        "NumLock" => 0x90,
        "ScrollLock" => 0x91,
        "Semicolon" => 0xBA,
        "Equal" => 0xBB,
        "Comma" => 0xBC,
        "Minus" => 0xBD,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backquote" => 0xC0,
        "BracketLeft" => 0xDB,
        "Backslash" => 0xDC,
        "BracketRight" => 0xDD,
        "Quote" => 0xDE,
        "IntlBackslash" => 0xE2,
        _ => return None,
    })
}

/// Non-Windows: named keys, otherwise the produced character.
#[cfg(not(windows))]
pub fn map_key(code: &str, key: &str) -> Option<Key> {
    Some(match code {
        "Backspace" => Key::Backspace,
        "Tab" => Key::Tab,
        "Enter" | "NumpadEnter" => Key::Return,
        "ShiftLeft" | "ShiftRight" => Key::Shift,
        "ControlLeft" | "ControlRight" => Key::Control,
        "AltLeft" | "AltRight" => Key::Alt,
        "MetaLeft" | "MetaRight" | "OSLeft" | "OSRight" => Key::Meta,
        "CapsLock" => Key::CapsLock,
        "Escape" => Key::Escape,
        "Space" => Key::Space,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "End" => Key::End,
        "Home" => Key::Home,
        "ArrowLeft" => Key::LeftArrow,
        "ArrowUp" => Key::UpArrow,
        "ArrowRight" => Key::RightArrow,
        "ArrowDown" => Key::DownArrow,
        "Delete" => Key::Delete,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        _ => {
            // Physical letter/digit keys: use the unshifted char so Shift state applies naturally.
            if let Some(c) = code.strip_prefix("Key") {
                return c.chars().next().map(|c| Key::Unicode(c.to_ascii_lowercase()));
            }
            if let Some(d) = code.strip_prefix("Digit") {
                return d.chars().next().map(Key::Unicode);
            }
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Unicode(c),
                _ => return None,
            }
        }
    })
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn vk_codes() {
        assert_eq!(super::code_to_vk("KeyA"), Some(0x41));
        assert_eq!(super::code_to_vk("Digit7"), Some(0x37));
        assert_eq!(super::code_to_vk("F12"), Some(0x7B));
        assert_eq!(super::code_to_vk("Numpad5"), Some(0x65));
    }
}
