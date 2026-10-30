//! Матчер клавиатурных биндингов: на вход нажатие (`KeyPressed`), на
//! выход — действие. Сетап не матчер: при создании ему отдаётся
//! `Setup`, матчинг живёт здесь. Бинды падов берутся из сетапа,
//! глобальные — из таблицы в коде (не в setup.yaml): цифры 1–8
//! активируют/деактивируют пад, Space — lap активного пада, W/E/R —
//! warn/fault/correct активного.
//! Матчер отслеживает активный пад: действия вида `lap-active-pad`
//! превращаются в сигнал над конкретным падом, `toggle-active-pad-N`
//! глобальны — к падам не привязаны, хоть пады и переключают.

use crate::config::keyboard::KeyBind;
use crate::config::setup::Setup;
use crate::driver::keyboard::KeyPressed;
use crate::model::flight::LapEvent;

/// Глобальные биндинги: (клавиши, действие). В коде, не в setup.yaml.
const GLOBAL_KEYBINDS: &[(&str, &str)] = &[
    ("1", "toggle-active-pad-1"),
    ("2", "toggle-active-pad-2"),
    ("3", "toggle-active-pad-3"),
    ("4", "toggle-active-pad-4"),
    ("5", "toggle-active-pad-5"),
    ("6", "toggle-active-pad-6"),
    ("7", "toggle-active-pad-7"),
    ("8", "toggle-active-pad-8"),
    ("Spacebar", "lap-active-pad"),
    ("W", "warn-active-pad"),
    ("E", "fault-active-pad"),
    ("R", "correct-active-pad"),
];

/// Результат матчинга нажатия.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyMatch {
    /// Сигнал судьи над конкретным падом.
    Signal { pad: String, event: LapEvent },
    /// Глобальное переключение активного пада (клавиши 1–8, нумерация
    /// с 1); повтор того же номера деактивирует. Номер без пада —
    /// нажатие гаснет (no-op).
    ToggleActivePad(usize),
}

/// Глобальную таблицу собираем через serde — тот же парсер ключей,
/// что и у биндов сетапа (exclusive_keyboard_id не касается: бинды
/// глобальные, без id).
fn global_keybinds() -> Vec<KeyBind> {
    GLOBAL_KEYBINDS
        .iter()
        .map(|(keys, action)| {
            serde_yaml::from_str(&format!("keys: {keys}\naction: {action}\n"))
                .expect("глобальный биндинг парсится")
        })
        .collect()
}

pub struct KeyMatcher {
    /// Порядок падов из сетапа (нумерация с 1 — индекс + 1).
    pads: Vec<String>,
    /// Бинды падов сетапа, плоским списком с адресом пада.
    pad_binds: Vec<(String, KeyBind)>,
    /// Глобальная таблица (в коде).
    global_binds: Vec<KeyBind>,
    /// Активный пад — индекс в `pads` (0-based).
    active: Option<usize>,
}

impl KeyMatcher {
    pub fn new(setup: &Setup) -> Self {
        let pads: Vec<String> = setup.pads.keys().cloned().collect();
        let pad_binds = setup
            .pads
            .iter()
            .flat_map(|(pad_id, pad)| {
                pad.keybinds
                    .iter()
                    .cloned()
                    .map(|bind| (pad_id.clone(), bind))
            })
            .collect();
        Self {
            pads,
            pad_binds,
            global_binds: global_keybinds(),
            active: None,
        }
    }

    /// Нажатие → действие. Сматчившийся падовый биндинг имеет
    /// приоритет над глобальным (неизвестное действие нажатие съедает,
    /// глобальные не проверяем). Глобальные действия разбираются на
    /// месте: `toggle-active-pad-N` переключает активный пад,
    /// `<signal>-active-pad` — сигнал над активным (без него — None).
    pub fn match_key(&mut self, event: &KeyPressed) -> Option<KeyMatch> {
        if let Some((pad, bind)) = self.pad_binds.iter().find(|(_, b)| b.matches(event)) {
            let event = LapEvent::from_action(&bind.action)?;
            return Some(KeyMatch::Signal {
                pad: pad.clone(),
                event,
            });
        }
        let action = self
            .global_binds
            .iter()
            .find(|bind| bind.matches(event))?
            .action
            .clone();
        if let Some(n) = action.strip_prefix("toggle-active-pad-") {
            let n: usize = n.parse().ok()?;
            let i = n - 1;
            if !(1..=8).contains(&n) || i >= self.pads.len() {
                return None; // пада с таким номером нет: нажатие гаснет
            }
            self.active = if self.active == Some(i) {
                None
            } else {
                Some(i)
            };
            return Some(KeyMatch::ToggleActivePad(n));
        }
        if let Some(signal) = action.strip_suffix("-active-pad") {
            let event = LapEvent::from_action(signal)?;
            let pad = self.pads.get(self.active?)?.clone();
            return Some(KeyMatch::Signal { pad, event });
        }
        None
    }

    /// Активный пад (id) или None.
    pub fn active_pad(&self) -> Option<&str> {
        self.active
            .and_then(|i| self.pads.get(i))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::keyboard::key_set;
    use serde_yaml::from_str as from_yaml_string;
    use std::time::Instant;

    /// Два пада: у pad-1 биндинг F13 → fault.
    fn setup() -> Setup {
        from_yaml_string(
            "cameras: {}
pads:
  pad-1:
    label: A1
    color: red
    fpv: { camera-id: camera-1 }
    keybinds:
      - keys: F13
        action: fault
  pad-2:
    label: A2
    color: blue
    fpv: { camera-id: camera-1 }
",
        )
        .unwrap()
    }

    fn press(keys: &[u32]) -> KeyPressed {
        KeyPressed {
            #[cfg(target_os = "linux")]
            exclusive_keyboard_id: None,
            keys: key_set(keys),
            at: Instant::now(),
        }
    }

    #[test]
    fn pad_bind_resolves_to_signal() {
        let mut m = KeyMatcher::new(&setup());
        assert_eq!(
            m.match_key(&press(&[0x68])), // F13
            Some(KeyMatch::Signal {
                pad: "pad-1".to_owned(),
                event: LapEvent::Fault,
            })
        );
    }

    #[test]
    fn toggle_switches_and_repeats_deactivate() {
        let mut m = KeyMatcher::new(&setup());
        // 1 — usage 0x1E.
        assert_eq!(
            m.match_key(&press(&[0x1E])),
            Some(KeyMatch::ToggleActivePad(1))
        );
        assert_eq!(m.active_pad(), Some("pad-1"));
        // Тот же номер — деактивация.
        assert_eq!(
            m.match_key(&press(&[0x1E])),
            Some(KeyMatch::ToggleActivePad(1))
        );
        assert_eq!(m.active_pad(), None);
        // Другой номер — переключение (2 = 0x1F).
        m.match_key(&press(&[0x1E]));
        assert_eq!(
            m.match_key(&press(&[0x1F])),
            Some(KeyMatch::ToggleActivePad(2))
        );
        assert_eq!(m.active_pad(), Some("pad-2"));
    }

    #[test]
    fn lap_on_active_pad_requires_active() {
        let mut m = KeyMatcher::new(&setup());
        // Space/W/E/R (0x2C/0x1A/0x08/0x15) без активного пада — ничего.
        for usage in [0x2C, 0x1A, 0x08, 0x15] {
            assert_eq!(m.match_key(&press(&[usage])), None);
        }
        m.match_key(&press(&[0x1F])); // активен pad-2
        assert_eq!(
            m.match_key(&press(&[0x2C])),
            Some(KeyMatch::Signal {
                pad: "pad-2".to_owned(),
                event: LapEvent::Lap,
            })
        );
        // W/E/R — warn/fault/correct активного пада.
        for (usage, event) in [
            (0x1A, LapEvent::Warn),
            (0x08, LapEvent::Fault),
            (0x15, LapEvent::Correction),
        ] {
            assert_eq!(
                m.match_key(&press(&[usage])),
                Some(KeyMatch::Signal {
                    pad: "pad-2".to_owned(),
                    event,
                })
            );
        }
    }

    #[test]
    fn toggle_beyond_pads_is_nothing() {
        let mut m = KeyMatcher::new(&setup()); // два пада
        for usage in [0x20, 0x22, 0x25] {
            // 3, 5, 8 — пада с таким номером нет: нажатие гаснет.
            assert_eq!(m.match_key(&press(&[usage])), None);
        }
        assert_eq!(m.active_pad(), None);
    }

    #[test]
    fn unmatched_key_is_nothing() {
        let mut m = KeyMatcher::new(&setup());
        assert_eq!(m.match_key(&press(&[0x04])), None); // A
    }
}
