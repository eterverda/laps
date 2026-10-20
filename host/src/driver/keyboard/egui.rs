//! Main-клавиатура: адаптер фокусной клавиатуры GUI (egui) как
//! источника `KeyPressed` общего канала — рядом с HID-устройствами.
//! Собственность — у экрана (Idle/Live), как у канала клавиатур: клон
//! отправителя берётся при его рождении, шлёт в него из потока UI.

use super::KeyPressed;
use crossbeam_channel as crossbeam;
use std::time::Instant;

/// Отправитель общего канала клавиатур: `pump` превращает события
/// egui-ввода в `KeyEvent` (keyboard_id "main").
pub struct MainKeyboard {
    tx: crossbeam::Sender<KeyPressed>,
}

impl MainKeyboard {
    pub fn new(events: &crossbeam::Sender<KeyPressed>) -> Self {
        Self { tx: events.clone() }
    }

    /// События egui-ввода → `KeyPressed` (keyboard_id "main") в общий
    /// канал. Физические сканкоды — layout не влияет на управляющие
    /// клавиши гонки.
    pub fn pump(&self, ui: &egui::Ui) {
        for event in ui.input(|i| i.events.clone()) {
            if let egui::Event::Key {
                key,
                physical_key,
                pressed,
                repeat,
                modifiers,
                ..
            } = event
            {
                // Только фронт нажатия: autorepeat и релизы мимо.
                if repeat || !pressed {
                    continue;
                }
                // Физический сканкод без layout'а; его нет (web/
                // старые бэкенды) — логический.
                if let Some(usage) = egui_key_usage(physical_key.unwrap_or(key)) {
                    let _ = self.tx.send(KeyPressed {
                        keyboard_id: "main".to_owned(),
                        modifiers: hid_modifiers(modifiers),
                        keycode: usage,
                        at: Instant::now(),
                    });
                }
            }
        }
    }
}

/// egui-сканкод → HID usage (page 0x07): буквы, цифры, F1-F24,
/// навигация — управляющие клавиши гонки. Пунктуация у egui
/// layout-зависима (имя варианта = производимый символ), надёжного
/// физического маппинга нет — None.
fn egui_key_usage(key: egui::Key) -> Option<u32> {
    use egui::Key as K;
    Some(match key {
        K::A => 0x04,
        K::B => 0x05,
        K::C => 0x06,
        K::D => 0x07,
        K::E => 0x08,
        K::F => 0x09,
        K::G => 0x0A,
        K::H => 0x0B,
        K::I => 0x0C,
        K::J => 0x0D,
        K::K => 0x0E,
        K::L => 0x0F,
        K::M => 0x10,
        K::N => 0x11,
        K::O => 0x12,
        K::P => 0x13,
        K::Q => 0x14,
        K::R => 0x15,
        K::S => 0x16,
        K::T => 0x17,
        K::U => 0x18,
        K::V => 0x19,
        K::W => 0x1A,
        K::X => 0x1B,
        K::Y => 0x1C,
        K::Z => 0x1D,
        K::Num1 => 0x1E,
        K::Num2 => 0x1F,
        K::Num3 => 0x20,
        K::Num4 => 0x21,
        K::Num5 => 0x22,
        K::Num6 => 0x23,
        K::Num7 => 0x24,
        K::Num8 => 0x25,
        K::Num9 => 0x26,
        K::Num0 => 0x27,
        K::Enter => 0x28,
        K::Escape => 0x29,
        K::Backspace => 0x2A,
        K::Tab => 0x2B,
        K::Space => 0x2C,
        K::F1 => 0x3A,
        K::F2 => 0x3B,
        K::F3 => 0x3C,
        K::F4 => 0x3D,
        K::F5 => 0x3E,
        K::F6 => 0x3F,
        K::F7 => 0x40,
        K::F8 => 0x41,
        K::F9 => 0x42,
        K::F10 => 0x43,
        K::F11 => 0x44,
        K::F12 => 0x45,
        K::F13 => 0x68,
        K::F14 => 0x69,
        K::F15 => 0x6A,
        K::F16 => 0x6B,
        K::F17 => 0x6C,
        K::F18 => 0x6D,
        K::F19 => 0x6E,
        K::F20 => 0x6F,
        K::F21 => 0x70,
        K::F22 => 0x71,
        K::F23 => 0x72,
        K::F24 => 0x73,
        K::Insert => 0x49,
        K::Home => 0x4A,
        K::PageUp => 0x4B,
        K::Delete => 0x4C,
        K::End => 0x4D,
        K::PageDown => 0x4E,
        K::ArrowRight => 0x4F,
        K::ArrowLeft => 0x50,
        K::ArrowDown => 0x51,
        K::ArrowUp => 0x52,
        _ => return None,
    })
}

/// egui::Modifiers → HID boot-маска: egui лево/право не различает —
/// кладём в левые биты (LCtrl/LShift/LAlt/LGui).
fn hid_modifiers(m: egui::Modifiers) -> u32 {
    (m.ctrl as u32) | (m.shift as u32) << 1 | (m.alt as u32) << 2 | (m.mac_cmd as u32) << 3
}
