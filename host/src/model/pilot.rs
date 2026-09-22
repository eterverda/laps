#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pilot {
    pub name: String,
}

const PILOT_RANDOM_NAMES: &[&str] = &[
    "Скорострельников Генадий",
    "Поэт Бездомный",
    "Иванов Иван Иваныч",
    "Цой Жив",
    "Ушат Помоев",
    "Борат Сагдиев",
    "Камаз Обоев",
    "Развал Устоев",
];

impl Pilot {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// Случайный пилот из PILOT_RANDOM_NAMES: выдача по порядку без
    /// повторов, при исчерпании колоды — перетасовка.
    pub fn random() -> Self {
        use rand::seq::SliceRandom;
        static REMAINING: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
        let mut remaining = REMAINING.lock().unwrap();
        if remaining.is_empty() {
            *remaining = PILOT_RANDOM_NAMES.to_vec();
            remaining.shuffle(&mut rand::rng());
        }
        Self::new(remaining.pop().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let pilot = Pilot::new("Цой Жив");
        assert_eq!(pilot.name, "Цой Жив");
    }

    // Один тест — единственный потребитель колоды: тесты бегут
    // параллельно, два теста на общем статике были бы флаки.
    #[test]
    fn test_random() {
        let dealt: Vec<String> = (0..PILOT_RANDOM_NAMES.len())
            .map(|_| Pilot::random().name)
            .collect();
        for name in &dealt {
            assert!(PILOT_RANDOM_NAMES.contains(&name.as_str()));
        }
        // Полный цикл — это перестановка всех имён (порядок выдачи = колода).
        let mut sorted = dealt;
        sorted.sort();
        let mut expected = PILOT_RANDOM_NAMES.to_vec();
        expected.sort();
        assert_eq!(sorted, expected);
        // Следующая выдача — начало нового цикла, имя всё ещё из массива.
        assert!(PILOT_RANDOM_NAMES.contains(&Pilot::random().name.as_str()));
    }
}
