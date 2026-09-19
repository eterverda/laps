#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Pilot {
    pub name: String,
}

impl Pilot {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pilot() {
        let pilot: Pilot = serde_yml::from_str("name: Иванов Иван Иваныч\n").unwrap();
        assert_eq!(pilot.name, "Иванов Иван Иваныч");
    }

    #[test]
    fn test_new() {
        let pilot = Pilot::new("Цой Жив");
        assert_eq!(pilot.name, "Цой Жив");
    }
}
