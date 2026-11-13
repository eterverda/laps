//! Правила гонки: чистые данные и вычисления старта/длительности.
//! Ничего про течение гонки (состояния, запись, журнал) — это сервис
//! `service::race`.

use crate::model::flight::FeatKind;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::Duration;

/// Сигнал старта: случайная пауза в [min, max) между последним бипом
/// отсчёта и гудком. Единственный вариант — рандом (как в laps-bar/
/// laps-trainer): точный момент старта заранее неизвестен. В yaml —
/// строка-диапазон humantime: «1s-3s»; min == max — просто «3s»
/// (одно значение с обеих сторон).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StartSignalDelay {
    min: Duration,
    max: Duration,
}

impl Serialize for StartSignalDelay {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let min = humantime::format_duration(self.min).to_string();
        if self.min == self.max {
            s.serialize_str(&min)
        } else {
            s.serialize_str(&format!("{min}-{}", humantime::format_duration(self.max)))
        }
    }
}

impl<'de> Deserialize<'de> for StartSignalDelay {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // humantime-компоненты '-' не содержат: первый дефис — разделитель.
        let (min, max) = match s.split_once('-') {
            Some((min, max)) => (min.trim(), max.trim()),
            None => (s.trim(), s.trim()),
        };
        let min = humantime::parse_duration(min).map_err(serde::de::Error::custom)?;
        let max = humantime::parse_duration(max).map_err(serde::de::Error::custom)?;
        if min > max {
            return Err(serde::de::Error::custom(format!(
                "start-signal-delay: min > max ({min:?} > {max:?})"
            )));
        }
        Ok(Self { min, max })
    }
}

impl StartSignalDelay {
    /// Значение по умолчанию (1s-3s): дефолты в yaml не пишутся.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub const fn random(min: Duration, max: Duration) -> Self {
        assert!(min.as_nanos() <= max.as_nanos(), "start signal: min > max");
        Self { min, max }
    }

    pub fn min(&self) -> Duration {
        self.min
    }

    pub fn max(&self) -> Duration {
        self.max
    }
}

impl Default for StartSignalDelay {
    fn default() -> Self {
        StartSignalDelay::random(Duration::from_secs(1), Duration::from_secs(3))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RaceRules {
    /// Пропуск отсчёта из 3 бипов перед стартом. true — бипы не
    /// звучат, остаётся только start-signal-delay.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_skip_countdown: bool,
    /// Дефолт (1s-3s) в файл не пишется: пропуск = дефолт.
    #[serde(default, skip_serializing_if = "StartSignalDelay::is_default")]
    pub start_signal_delay: StartSignalDelay,
    /// Лимит длительности гонки. None — без лимита (пропуск ключа в
    /// файле). Some — humantime-строкой («1m 30s»).
    #[serde(
        default,
        with = "opt_humantime",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_duration: Option<Duration>,
    /// Максимум кругов. Гонка закрывается в момент, когда ВСЕ пилоты
    /// достигли лимита (пилоты — назначения журнала). Ортогонально
    /// лимиту длительности и ручному финишу: срабатывает тот, кто
    /// первый. None — без лимита, в файл не пишется.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_laps: Option<u32>,
    /// Холшот. Включён — первый круг начинается с пролета стартовых ворот,
    /// отображается как «HS»; выключен — первый круг начинается со старта.
    /// На запись в журнал не влияет — только имена строк и будущие
    /// результаты.
    pub holeshot_enabled: bool,
    /// Итоговый вид достижения: что показываем под кругами и по чему
    /// распределяются места (сравнение фитов этого вида). Пишется
    /// всегда — дефолт (most) неочевиден.
    pub feat_kind: FeatKind,
}

/// Число бипов отсчёта перед стартовой паузой.
pub const PRE_BEEP_SECS: u64 = 3;

/// Пауза для конкретного старта: равномерный выбор в [min_delay,
/// max_delay); вырожденный случай (min == max) — само значение.
pub fn sample_start_delay(signal: &StartSignalDelay) -> Duration {
    let min = signal.min();
    let span = signal.max().saturating_sub(min).as_millis() as u64;
    if span == 0 {
        return min;
    }
    min + Duration::from_millis(rand::random_range(0..span))
}

/// Option<Duration> как humantime-строка: Some — «90s», None — null.
/// humantime::serde умеет Duration; обёртка ради Option.
mod opt_humantime {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        d.map(|d| humantime::format_duration(d).to_string())
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        s.map(|s| humantime::parse_duration(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Конструктор держит границы; вырожденный диапазон min == max
    /// отдаёт само значение.
    #[test]
    fn rules_yaml_uses_humantime() {
        let parse = |s: &str| serde_yaml::from_str::<RaceRules>(s).unwrap();
        let yaml = serde_yaml::to_string(&parse(
            "max-duration: 1m 30s\nholeshot-enabled: true\nfeat-kind: most\n",
        ))
        .unwrap();
        assert!(yaml.contains("max-duration: 1m 30s"), "{yaml}");
        assert!(!yaml.contains("start-signal-delay"), "{yaml}");
        let back = parse(&yaml);
        assert_eq!(back.max_duration, Some(Duration::from_secs(90)));
        assert_eq!(back.start_signal_delay, StartSignalDelay::default());
        let yaml = serde_yaml::to_string(&parse(
            "start-signal-delay: 1s-5s\nholeshot-enabled: true\nfeat-kind: most\n",
        ))
        .unwrap();
        assert!(yaml.contains("start-signal-delay: 1s-5s"), "{yaml}");
        assert_eq!(
            parse(&yaml).start_signal_delay,
            StartSignalDelay::random(Duration::from_secs(1), Duration::from_secs(5))
        );
        let yaml = serde_yaml::to_string(&parse(
            "start-signal-delay: 7s\nholeshot-enabled: true\nfeat-kind: most\n",
        ))
        .unwrap();
        assert!(yaml.contains("start-signal-delay: 7s"), "{yaml}");
        assert_eq!(
            parse(&yaml).start_signal_delay,
            StartSignalDelay::random(Duration::from_secs(7), Duration::from_secs(7))
        );
    }

    #[test]
    fn rules_yaml_format_details() {
        let parse = |s: &str| serde_yaml::from_str::<RaceRules>(s).unwrap();
        let yaml =
            serde_yaml::to_string(&parse("holeshot-enabled: true\nfeat-kind: most\n")).unwrap();
        assert!(yaml.contains("feat-kind: most"), "{yaml}");
        let yaml =
            serde_yaml::to_string(&parse("holeshot-enabled: true\nfeat-kind: best-1\n")).unwrap();
        assert!(yaml.contains("feat-kind: best-1"), "{yaml}");
        assert_eq!(parse(&yaml).feat_kind, FeatKind::Best1);
        // None лимита — поле отсутствует (пропуск = без лимита).
        let yaml =
            serde_yaml::to_string(&parse("holeshot-enabled: true\nfeat-kind: most\n")).unwrap();
        assert!(!yaml.contains("max-duration"), "{yaml}");
        assert_eq!(parse(&yaml).max_duration, None);
    }

    #[test]
    fn delay_bounds() {
        let random = StartSignalDelay::random(Duration::from_secs(2), Duration::from_secs(4));
        assert_eq!(random.min(), Duration::from_secs(2));
        assert_eq!(random.max(), Duration::from_secs(4));

        let degenerate = StartSignalDelay::random(Duration::from_secs(7), Duration::from_secs(7));
        assert_eq!(degenerate.min(), Duration::from_secs(7));
        assert_eq!(degenerate.max(), Duration::from_secs(7));

        assert_eq!(StartSignalDelay::default().min(), Duration::from_secs(1));
    }

    /// Пропуск start-skip-countdown в yaml = false: отсчёт по
    /// умолчанию звучит.
    #[test]
    fn start_skip_countdown_absent_means_false() {
        let r: RaceRules =
            serde_yaml::from_str("max-duration: 1m 30s\nholeshot-enabled: true\nfeat-kind: most\n")
                .unwrap();
        assert!(!r.start_skip_countdown);
    }
}
