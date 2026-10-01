use std::ops::Div;

pub const CELL_SIZE: egui::Vec2 = egui::vec2(8.0, 16.0);

#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub col: isize,
    pub row: isize,
}

impl Cell {
    pub const fn translate(&self, cols: isize, rows: isize) -> Cell {
        cell(self.col + cols, self.row + rows)
    }

    pub const fn extrude(&self, cols: isize, rows: isize) -> egui::Rect {
        let (left, right) = if cols > 0 {
            (self.col, self.col + cols)
        } else {
            (self.col + cols, self.col)
        };
        let (top, bottom) = if rows > 0 {
            (self.row, self.row + rows)
        } else {
            (self.row + rows, self.row)
        };
        egui::Rect::from_min_max(cell_pos2(left, top), cell_pos2(right, bottom))
    }

    pub const fn to_pos2(self) -> egui::Pos2 {
        cell_pos2(self.col, self.row)
    }
}

impl From<Cell> for egui::Pos2 {
    fn from(val: Cell) -> Self {
        egui::pos2(cell_x(val.col), cell_y(val.row))
    }
}

impl From<Cell> for egui::Vec2 {
    fn from(val: Cell) -> Self {
        egui::vec2(cell_x(val.col), cell_y(val.row))
    }
}

impl From<egui::Pos2> for Cell {
    fn from(value: egui::Pos2) -> Self {
        cell_at(value)
    }
}

impl From<egui::Vec2> for Cell {
    fn from(value: egui::Vec2) -> Self {
        cell_at(egui::pos2(value.x, value.y))
    }
}

pub trait IntoCell {
    fn into_cell(self) -> Cell;
}

impl<T> IntoCell for T
where
    T: Into<Cell>,
{
    fn into_cell(self) -> Cell {
        self.into()
    }
}

pub const fn cell(col: isize, row: isize) -> Cell {
    Cell { col, row }
}

pub fn cell_at(pos: impl Into<egui::Pos2>) -> Cell {
    let pos: egui::Pos2 = pos.into();
    Cell {
        col: pos.x.div(CELL_SIZE.x).floor() as isize,
        row: pos.y.div(CELL_SIZE.y).floor() as isize,
    }
}

#[inline]
pub const fn cell_x(col: isize) -> f32 {
    col as f32 * CELL_SIZE.x
}

#[inline]
pub const fn cell_y(row: isize) -> f32 {
    row as f32 * CELL_SIZE.y
}

#[inline]
const fn cell_pos2(col: isize, row: isize) -> egui::Pos2 {
    egui::pos2(cell_x(col), cell_y(row))
}

#[inline]
pub fn whole_cols(px: f32) -> isize {
    px.div(CELL_SIZE.x).ceil() as isize
}
