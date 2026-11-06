//! Расширения egui: комбинации, которых нет в апстриме.

/// Добавки к [`egui::Ui`].
pub trait UiExt {
    /// Дочерний Ui по абсолютному rect с содержимым-замыканием:
    /// `place` для `FnOnce(&mut Ui)` вместо виджета (курсор родителя
    /// не сдвигается — раскладка на коллере).
    fn place_ui<R>(
        &mut self,
        rect: egui::Rect,
        add_contents: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::InnerResponse<R>;
}

impl UiExt for egui::Ui {
    fn place_ui<R>(
        &mut self,
        rect: egui::Rect,
        add_contents: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::InnerResponse<R> {
        self.scope_builder(egui::UiBuilder::new().max_rect(rect), add_contents)
    }
}
