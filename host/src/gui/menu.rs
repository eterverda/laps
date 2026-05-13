use super::*;

pub struct Menu;

impl Into<State> for Menu {
    fn into(self) -> State {
        State::Menu(self)
    }
}

impl Menu {
    pub fn update(&mut self, ui: &mut egui::Ui, navigator: &mut Navigator) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 2.0 - 12.0);
            if ui.button("Start").clicked() {
                navigator.goto(live::Live::new(ui.ctx()));
            }
        });
    }
}
