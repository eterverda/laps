use super::*;

pub struct Menu;

pub enum Action {
    Start,
    None,
}

impl From<Menu> for State {
    fn from(val: Menu) -> Self {
        State::Menu(val)
    }
}

impl Menu {
    pub fn update(&mut self, ui: &mut egui::Ui) -> Action {
        let mut action = Action::None;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 2.0 - 12.0);
            if ui.button("Start").clicked() {
                action = Action::Start;
            }
        });
        action
    }
}
