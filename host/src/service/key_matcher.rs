//! Матчер клавиатурных биндингов: на вход нажатие (`KeyPressed`), на
//! выход — действие. Сетап не матчер: при создании ему отдаётся
//! `Setup`, матчинг живёт здесь. Бинды падов берутся из сетапа,
//! глобальные — из таблицы в коде (не в setup.yaml): цифры 1–8 —
//! тогл активного пада (повтор снимает), 0 снимает явно, Space — lap
//! активного пада, W/E/R — warn/fault/correct активного.
//! Матчер отслеживает активный пад: действия вида `lap-active-pad`
//! превращаются в сигнал над конкретным падом, тогл цифрой наружу
//! выходит уже разрешённым: Set (чужой номер) или Reset (свой
//! повтором). Глобальные действия к падам не привязаны, хоть пады и
//! переключают.

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
    ("0", "reset-active-pad"),
    ("Spacebar", "lap-active-pad"),
    ("W", "warn-active-pad"),
    ("E", "fault-active-pad"),
    ("R", "correct-active-pad"),
];

/// Результат матчинга нажатия. Активный пад — всегда по имени.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyMatch {
    /// Сигнал судьи над конкретным падом.
    Signal { pad: String, event: LapEvent },
    /// Активный пад включён (тогл невыбранного номера).
    SetActivePad(String),
    /// Активный пад снят (тогл своего номера повтором или «0»).
    ResetActivePad,
}

impl std::fmt::Display for KeyMatch {
    /// Канонические строки действий (как в таблицах биндингов):
    /// `fault -> pad-1`, `set-active-pad -> pad-2`, `reset-active-pad`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyMatch::Signal { pad, event } => write!(f, "{} -> {pad}", event.action()),
            KeyMatch::SetActivePad(pad) => write!(f, "set-active-pad -> {pad}"),
            KeyMatch::ResetActivePad => write!(f, "reset-active-pad"),
        }
    }
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
    /// месте: тогл цифрой разрешается по состоянию в Set (чужой
    /// номер) или Reset (свой повтором), `reset-active-pad` снимает
    /// явно, `<signal>-active-pad` — сигнал над активным (без него —
    /// None).
    pub fn match_key(&mut self, event: &KeyPressed) -> Option<KeyMatch> {
        if let Some((pad, bind)) = self.pad_binds.iter().find(|(_, b)| b.matches(event)) {
            let event = LapEvent::from_action(&bind.action)?;
            return Some(KeyMatch::Signal {
                pad: pad.clone(),
                event,
            });
        }
        let action = &self
            .global_binds
            .iter()
            .find(|bind| bind.matches(event))?
            .action;
        if let Some(n) = action.strip_prefix("toggle-active-pad-") {
            let n: usize = n.parse().ok()?;
            if !(1..=8).contains(&n) || n > self.pads.len() {
                return None; // пада с таким номером нет: нажатие гаснет
            }
            let i = n - 1;
            return Some(if self.active == Some(i) {
                self.active = None;
                KeyMatch::ResetActivePad
            } else {
                self.active = Some(i);
                KeyMatch::SetActivePad(self.pads[i].clone())
            });
        }
        if action == "reset-active-pad" {
            self.active = None;
            return Some(KeyMatch::ResetActivePad);
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

    /// Начальное состояние активного пада (id) — передача выбора
    /// через границу фаз: GUI-матчер отдаёт его матчеру гонки при
    /// входе в Live, обратно приходит через Notice::ActivePad.
    pub fn set_active_pad(&mut self, pad: Option<&str>) {
        self.active = pad.and_then(|p| self.pads.iter().position(|id| id == p));
    }
}

impl Default for KeyMatcher {
    /// Пустой матчер: ни падов, ни биндов — для пустышек обмена фаз
    /// (экземпляр живёт до замены вызывающим кодом).
    fn default() -> Self {
        Self {
            pads: Vec::new(),
            pad_binds: Vec::new(),
            global_binds: global_keybinds(),
            active: None,
        }
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
    fn toggle_resolves_to_set_or_reset() {
        let mut m = KeyMatcher::new(&setup());
        // 1 — usage 0x1E: тогл пустого состояния — Set.
        assert_eq!(
            m.match_key(&press(&[0x1E])),
            Some(KeyMatch::SetActivePad("pad-1".to_owned()))
        );
        assert_eq!(m.active_pad(), Some("pad-1"));
        // Повтор своего номера — Reset.
        assert_eq!(m.match_key(&press(&[0x1E])), Some(KeyMatch::ResetActivePad));
        assert_eq!(m.active_pad(), None);
        // Чужой номер — Set (2 = 0x1F).
        assert_eq!(
            m.match_key(&press(&[0x1F])),
            Some(KeyMatch::SetActivePad("pad-2".to_owned()))
        );
        assert_eq!(m.active_pad(), Some("pad-2"));
        // 0 — usage 0x27 — явное снятие.
        assert_eq!(m.match_key(&press(&[0x27])), Some(KeyMatch::ResetActivePad));
        assert_eq!(m.active_pad(), None);
        // Повторный 0 при пустом состоянии — всё равно Reset.
        assert_eq!(m.match_key(&press(&[0x27])), Some(KeyMatch::ResetActivePad));
        assert_eq!(m.active_pad(), None);
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
    fn set_active_pad_seeds_by_pad_id() {
        let mut m = KeyMatcher::new(&setup());
        m.set_active_pad(Some("pad-2"));
        assert_eq!(m.active_pad(), Some("pad-2"));
        // lap-active-pad без toggle — сразу на посеянный пад.
        assert_eq!(
            m.match_key(&press(&[0x2C])),
            Some(KeyMatch::Signal {
                pad: "pad-2".to_owned(),
                event: LapEvent::Lap,
            })
        );
        // Неизвестный id — сброс в None.
        m.set_active_pad(Some("pad-X"));
        assert_eq!(m.active_pad(), None);
    }

    #[test]
    fn unmatched_key_is_nothing() {
        let mut m = KeyMatcher::new(&setup());
        assert_eq!(m.match_key(&press(&[0x04])), None); // A
    }
}
