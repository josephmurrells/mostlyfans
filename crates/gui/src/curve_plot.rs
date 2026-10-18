use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, pos2,
    vec2,
};
use fanforge_core::curve::{CurvePoint, sorted_points};

use crate::format;

const HEIGHT: f32 = 300.0;
const LEFT_MARGIN: f32 = 44.0;
const BOTTOM_MARGIN: f32 = 26.0;
const TOP_MARGIN: f32 = 10.0;
const RIGHT_MARGIN: f32 = 12.0;
const MAX_TEMP: f32 = 125.0;
const MAX_DUTY: f32 = 100.0;
const GRAB_RADIUS: f32 = 12.0;
const POINT_RADIUS: f32 = 5.0;
const CURVE_COLOR: Color32 = Color32::from_rgb(64, 156, 255);

pub struct Live {
    pub temp: Option<f32>,
    pub output: Option<f32>,
}

struct Mapping {
    area: Rect,
}

impl Mapping {
    fn to_screen(&self, temp: f32, duty: f32) -> Pos2 {
        pos2(
            egui::lerp(self.area.x_range(), temp / MAX_TEMP),
            egui::lerp(self.area.bottom()..=self.area.top(), duty / MAX_DUTY),
        )
    }

    fn to_curve(&self, position: Pos2) -> CurvePoint {
        let temp = egui::remap_clamp(position.x, self.area.x_range(), 0.0..=MAX_TEMP);
        let duty = egui::remap_clamp(
            position.y,
            self.area.bottom()..=self.area.top(),
            0.0..=MAX_DUTY,
        );
        CurvePoint::new(temp.round(), duty.round())
    }
}

pub fn show(
    ui: &mut egui::Ui,
    points: &mut Vec<CurvePoint>,
    dragging: &mut Option<usize>,
    min_points: usize,
    live: &Live,
) -> bool {
    let size = vec2(ui.available_width(), HEIGHT);
    let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
    let frame = response.rect;
    let mapping = Mapping {
        area: Rect::from_min_max(
            frame.min + vec2(LEFT_MARGIN, TOP_MARGIN),
            frame.max - vec2(RIGHT_MARGIN, BOTTOM_MARGIN),
        ),
    };
    let visuals = ui.visuals();
    painter.rect_filled(mapping.area, 4.0, visuals.extreme_bg_color);
    draw_grid(&painter, &mapping, visuals.weak_text_color());

    let pointer = response
        .interact_pointer_pos()
        .or_else(|| response.hover_pos());
    let nearest = pointer.and_then(|cursor| {
        points
            .iter()
            .enumerate()
            .map(|(index, point)| {
                (
                    index,
                    mapping.to_screen(point.temp, point.duty).distance(cursor),
                )
            })
            .filter(|(_, distance)| *distance <= GRAB_RADIUS)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index)
    });

    let mut changed = false;
    if response.drag_started() {
        *dragging = nearest;
    }
    if response.dragged()
        && let (Some(index), Some(cursor)) = (*dragging, pointer)
        && let Some(point) = points.get_mut(index)
    {
        let moved = mapping.to_curve(cursor);
        if moved != *point {
            *point = moved;
            changed = true;
        }
    }
    if response.drag_stopped() {
        *dragging = None;
    }
    if response.double_clicked()
        && let Some(cursor) = pointer
    {
        match nearest {
            Some(index) if points.len() > min_points => {
                points.remove(index);
                changed = true;
            }
            Some(_) => {}
            None => {
                points.push(mapping.to_curve(cursor));
                changed = true;
            }
        }
    }
    if nearest.is_some() || dragging.is_some() {
        ui.ctx().set_cursor_icon(if dragging.is_some() {
            CursorIcon::Grabbing
        } else {
            CursorIcon::Grab
        });
    }

    draw_curve(&painter, &mapping, points);
    for (index, point) in points.iter().enumerate() {
        let active = Some(index) == *dragging || Some(index) == nearest;
        let center = mapping.to_screen(point.temp, point.duty);
        painter.circle_filled(
            center,
            if active {
                POINT_RADIUS + 2.0
            } else {
                POINT_RADIUS
            },
            CURVE_COLOR,
        );
        if active {
            painter.text(
                center + vec2(8.0, -8.0),
                Align2::LEFT_BOTTOM,
                format!("{:.0} °C → {:.0}%", point.temp, point.duty),
                FontId::proportional(12.0),
                visuals.strong_text_color(),
            );
        }
    }
    if let Some(temp) = live.temp.filter(|temp| (0.0..=MAX_TEMP).contains(temp)) {
        let top = mapping.to_screen(temp, MAX_DUTY);
        let bottom = mapping.to_screen(temp, 0.0);
        painter.line_segment([top, bottom], Stroke::new(1.5, format::WARN));
        if let Some(duty) = live.output {
            painter.circle_stroke(
                mapping.to_screen(temp, duty),
                POINT_RADIUS + 2.0,
                Stroke::new(2.0, format::WARN),
            );
        }
    }
    painter.rect_stroke(
        mapping.area,
        4.0,
        Stroke::new(1.0, visuals.weak_text_color()),
        StrokeKind::Inside,
    );
    changed
}

fn draw_grid(painter: &egui::Painter, mapping: &Mapping, color: Color32) {
    let grid = Stroke::new(0.5, color.gamma_multiply(0.4));
    let font = FontId::proportional(11.0);
    for step in (0..=MAX_TEMP as u32).step_by(10) {
        let temp = step as f32;
        let bottom = mapping.to_screen(temp, 0.0);
        painter.line_segment([bottom, mapping.to_screen(temp, MAX_DUTY)], grid);
        if step % 20 == 0 {
            painter.text(
                bottom + Vec2::Y * 4.0,
                Align2::CENTER_TOP,
                format!("{step}°"),
                font.clone(),
                color,
            );
        }
    }
    for step in (0..=MAX_DUTY as u32).step_by(10) {
        let duty = step as f32;
        let left = mapping.to_screen(0.0, duty);
        painter.line_segment([left, mapping.to_screen(MAX_TEMP, duty)], grid);
        if step % 20 == 0 {
            painter.text(
                left - Vec2::X * 6.0,
                Align2::RIGHT_CENTER,
                format!("{step}%"),
                font.clone(),
                color,
            );
        }
    }
}

fn draw_curve(painter: &egui::Painter, mapping: &Mapping, points: &[CurvePoint]) {
    let sorted = sorted_points(points);
    let (Some(first), Some(last)) = (sorted.first(), sorted.last()) else {
        return;
    };
    let line: Vec<Pos2> = std::iter::once(mapping.to_screen(0.0, first.duty))
        .chain(
            sorted
                .iter()
                .map(|point| mapping.to_screen(point.temp.min(MAX_TEMP), point.duty)),
        )
        .chain(std::iter::once(mapping.to_screen(MAX_TEMP, last.duty)))
        .collect();
    painter.line(line, Stroke::new(2.0, CURVE_COLOR));
}
