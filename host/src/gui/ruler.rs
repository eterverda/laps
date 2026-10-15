use super::grid::CELL_SIZE;

#[derive(Clone, Copy, PartialEq)]
pub struct Ruler {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl Default for Ruler {
    fn default() -> Self {
        Self {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        }
    }
}

impl Ruler {
    #[allow(dead_code)]
    #[must_use]
    pub fn translate(self, dx: f32, dy: f32) -> Self {
        Self {
            left: self.left + dx,
            top: self.top + dy,
            right: self.right + dx,
            bottom: self.bottom + dy,
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn extrude(self, dx: f32, dy: f32) -> Self {
        Self {
            left: self.left + dx.min(0.0),
            top: self.top + dy.min(0.0),
            right: self.right + dx.max(0.0),
            bottom: self.bottom + dy.max(0.0),
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn extrude_vec(self, vec: impl Into<egui::Vec2>) -> Self {
        let vec = vec.into();
        self.extrude(vec.x, vec.y)
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn shrink(self, dx: f32, dy: f32) -> Self {
        Self {
            left: self.left + dx.max(0.0),
            top: self.top + dy.max(0.0),
            right: self.right + dx.min(0.0),
            bottom: self.bottom + dy.min(0.0),
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn shrink_vec(self, vec: impl Into<egui::Vec2>) -> Self {
        let vec = vec.into();
        self.shrink(vec.x, vec.y)
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn collapse_left(self) -> Self {
        Self {
            left: self.left,
            top: self.top,
            right: self.left,
            bottom: self.bottom,
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn collapse_top(self) -> Self {
        Self {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.top,
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn collapse_right(self) -> Self {
        Self {
            left: self.right,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn collapse_bottom(self) -> Self {
        Self {
            left: self.left,
            top: self.bottom,
            right: self.right,
            bottom: self.bottom,
        }
    }
}

impl From<egui::Pos2> for Ruler {
    #[inline(always)]
    fn from(pos: egui::Pos2) -> Self {
        Self {
            left: pos.x,
            top: pos.y,
            right: pos.x,
            bottom: pos.y,
        }
    }
}

impl From<egui::Rect> for Ruler {
    #[inline(always)]
    fn from(rect: egui::Rect) -> Self {
        Self {
            left: rect.min.x,
            top: rect.min.y,
            right: rect.max.x,
            bottom: rect.max.y,
        }
    }
}

impl From<Ruler> for egui::Rect {
    fn from(ruler: Ruler) -> Self {
        Self {
            min: egui::pos2(ruler.left, ruler.top),
            max: egui::pos2(ruler.right, ruler.bottom),
        }
    }
}

#[allow(unused)]
pub trait IntoRuler {
    #[must_use]
    fn into_ruler(self) -> Ruler;
}

impl IntoRuler for egui::Pos2 {
    #[inline(always)]
    fn into_ruler(self) -> Ruler {
        Ruler {
            left: self.x,
            top: self.y,
            right: self.x,
            bottom: self.y,
        }
    }
}

impl IntoRuler for &egui::Pos2 {
    #[inline(always)]
    fn into_ruler(self) -> Ruler {
        Ruler {
            left: self.x,
            top: self.y,
            right: self.x,
            bottom: self.y,
        }
    }
}

impl IntoRuler for egui::Rect {
    #[inline(always)]
    fn into_ruler(self) -> Ruler {
        Ruler {
            left: self.min.x,
            top: self.min.y,
            right: self.max.x,
            bottom: self.max.y,
        }
    }
}

impl IntoRuler for &egui::Rect {
    #[inline(always)]
    fn into_ruler(self) -> Ruler {
        Ruler {
            left: self.min.x,
            top: self.min.y,
            right: self.max.x,
            bottom: self.max.y,
        }
    }
}

#[allow(dead_code)]
pub trait CellsTall {
    fn cells_tall(self) -> f32;
}

impl CellsTall for i32 {
    fn cells_tall(self) -> f32 {
        self as f32 * CELL_SIZE.y
    }
}

impl CellsTall for f64 {
    fn cells_tall(self) -> f32 {
        self as f32 * CELL_SIZE.y
    }
}

#[allow(dead_code)]
pub trait CellsWide {
    fn cells_wide(self) -> f32;
}

impl CellsWide for i32 {
    fn cells_wide(self) -> f32 {
        self as f32 * CELL_SIZE.x
    }
}

impl CellsWide for f64 {
    fn cells_wide(self) -> f32 {
        self as f32 * CELL_SIZE.x
    }
}
