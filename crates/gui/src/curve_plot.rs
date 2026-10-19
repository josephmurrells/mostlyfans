use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Pos2, Rect, Sense, Shape, Stroke,
    Vec2, pos2, vec2,
};
use fanforge_core::config::MAX_CURVE_POINTS;
use fanforge_core::curve::{CurveDef, CurveKind, CurvePoint, sorted_points};

use crate::theme::{self, CAPTION, Palette};

const HEIGHT: f32 = 300.0;
const PREVIEW_HEIGHT: f32 = 120.0;
const LEFT_MARGIN: f32 = 40.0;
const BOTTOM_MARGIN: f32 = 24.0;
const TOP_MARGIN: f32 = 12.0;
const RIGHT_MARGIN: f32 = 12.0;
const MAX_TEMP: f32 = 125.0;
const MAX_DUTY: f32 = 100.0;
const GRAB_RADIUS: f32 = 14.0;
const POINT_RADIUS: f32 = 6.0;

pub struct Live {
    pub temp: Option<f32>,
    pub output: Option<f32>,
}

pub struct FloorMark {
    pub percent: f32,
    pub label: String,
}

struct Mapping {
    area: Rect,
}

impl Mapping {
    fn new(frame: Rect, compact: bool) -> Self {
        let (left, bottom) = if compact {
            (28.0, 18.0)
        } else {
            (LEFT_MARGIN, BOTTOM_MARGIN)
        };
        Self {
            area: Rect::from_min_max(
                frame.min + vec2(left, TOP_MARGIN),
                frame.max - vec2(RIGHT_MARGIN, bottom),
            ),
        }
    }

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

pub fn shape(curve: &CurveDef) -> Option<Vec<CurvePoint>> {
    match &curve.kind {
        CurveKind::Graph { points, .. } => Some(points.clone()),
        CurveKind::Linear { start, end, .. } => Some(vec![*start, *end]),
        CurveKind::Flat { duty } => Some(vec![
            CurvePoint::new(0.0, *duty),
            CurvePoint::new(MAX_TEMP, *duty),
        ]),
        CurveKind::Mix { .. } => None,
    }
}

pub fn show(
    ui: &mut egui::Ui,
    points: &mut Vec<CurvePoint>,
    dragging: &mut Option<usize>,
    min_points: usize,
    live: &Live,
    floor: Option<&FloorMark>,
) -> bool {
    let size = vec2(ui.available_width(), HEIGHT);
    let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
    let mapping = Mapping::new(response.rect, false);
    let palette = theme::of(ui);

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
            None if points.len() < MAX_CURVE_POINTS => {
                points.push(mapping.to_curve(cursor));
                changed = true;
            }
            Some(_) | None => {}
        }
    }
    if nearest.is_some() || dragging.is_some() {
        ui.ctx().set_cursor_icon(if dragging.is_some() {
            CursorIcon::Grabbing
        } else {
            CursorIcon::Grab
        });
    }

    draw_grid(&painter, &mapping, palette, false);
    if let Some(floor) = floor {
        draw_floor(
            &painter,
            &mapping,
            palette,
            floor.percent,
            Some(&floor.label),
        );
    }
    draw_curve(&painter, &mapping, palette, points);
    for (index, point) in points.iter().enumerate() {
        let active = Some(index) == *dragging || Some(index) == nearest;
        let center = mapping.to_screen(point.temp, point.duty);
        if active {
            painter.circle_filled(center, POINT_RADIUS + 1.5, palette.accent);
            tag(
                &painter,
                palette,
                center + vec2(10.0, -10.0),
                Align2::LEFT_BOTTOM,
                &format!("{:.0} °C → {:.0}%", point.temp, point.duty),
                palette.text,
            );
        } else {
            painter.circle(
                center,
                POINT_RADIUS,
                palette.surface,
                Stroke::new(2.5, palette.accent),
            );
        }
    }
    draw_live(&painter, &mapping, palette, live);
    changed
}

pub fn preview(ui: &mut egui::Ui, curve: &CurveDef, live: &Live, floor: Option<f32>) {
    let Some(points) = shape(curve) else {
        return;
    };
    let size = vec2(ui.available_width().min(520.0), PREVIEW_HEIGHT);
    let (response, painter) = ui.allocate_painter(size, Sense::hover());
    let mapping = Mapping::new(response.rect, true);
    let palette = theme::of(ui);
    draw_grid(&painter, &mapping, palette, true);
    if let Some(floor) = floor {
        draw_floor(&painter, &mapping, palette, floor, None);
    }
    draw_curve(&painter, &mapping, palette, &points);
    draw_live(&painter, &mapping, palette, live);
}

pub fn thumbnail(ui: &mut egui::Ui, curve: &CurveDef, size: Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let palette = theme::of(ui);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(4), palette.control);
    let Some(points) = shape(curve) else {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "max",
            FontId::proportional(10.0),
            palette.text_muted,
        );
        return;
    };
    let mapping = Mapping {
        area: rect.shrink(3.0),
    };
    painter.line(
        line_points(&mapping, &points),
        Stroke::new(1.5, palette.accent),
    );
}

fn tag(
    painter: &egui::Painter,
    palette: &Palette,
    anchor: Pos2,
    align: Align2,
    text: &str,
    color: Color32,
) {
    let galley = painter.layout_no_wrap(text.to_owned(), FontId::proportional(CAPTION), color);
    let rect = align
        .anchor_size(anchor, galley.size())
        .expand2(vec2(6.0, 3.0));
    painter.rect(
        rect,
        CornerRadius::same(4),
        palette.surface_raised,
        Stroke::new(1.0, palette.border),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.min + vec2(6.0, 3.0), galley, color);
}

fn draw_grid(painter: &egui::Painter, mapping: &Mapping, palette: &Palette, compact: bool) {
    let grid = Stroke::new(1.0, palette.border.gamma_multiply(0.7));
    let font = FontId::proportional(if compact { 10.0 } else { 11.0 });
    for step in (0..=MAX_TEMP as u32).step_by(25) {
        let temp = step as f32;
        let bottom = mapping.to_screen(temp, 0.0);
        painter.line_segment([bottom, mapping.to_screen(temp, MAX_DUTY)], grid);
        painter.text(
            bottom + Vec2::Y * 5.0,
            Align2::CENTER_TOP,
            format!("{step}°"),
            font.clone(),
            palette.text_muted,
        );
    }
    let duty_step = if compact { 50 } else { 25 };
    for step in (0..=MAX_DUTY as u32).step_by(duty_step) {
        let duty = step as f32;
        let left = mapping.to_screen(0.0, duty);
        painter.line_segment([left, mapping.to_screen(MAX_TEMP, duty)], grid);
        painter.text(
            left - Vec2::X * 6.0,
            Align2::RIGHT_CENTER,
            format!("{step}%"),
            font.clone(),
            palette.text_muted,
        );
    }
}

fn draw_floor(
    painter: &egui::Painter,
    mapping: &Mapping,
    palette: &Palette,
    percent: f32,
    label: Option<&str>,
) {
    if percent <= 0.0 {
        return;
    }
    let top = mapping.to_screen(MAX_TEMP, percent);
    let band = Rect::from_min_max(
        pos2(mapping.area.left(), top.y),
        mapping.area.right_bottom(),
    );
    painter.rect_filled(
        band,
        CornerRadius::ZERO,
        palette.text_muted.gamma_multiply(0.08),
    );
    painter.add(Shape::dashed_line(
        &[pos2(mapping.area.left(), top.y), top],
        Stroke::new(1.0, palette.text_muted),
        6.0,
        4.0,
    ));
    if let Some(label) = label {
        painter.text(
            pos2(mapping.area.right() - 6.0, top.y + 4.0),
            Align2::RIGHT_TOP,
            label,
            FontId::proportional(CAPTION),
            palette.text_muted,
        );
    }
}

fn line_points(mapping: &Mapping, points: &[CurvePoint]) -> Vec<Pos2> {
    let sorted = sorted_points(points);
    let (Some(first), Some(last)) = (sorted.first(), sorted.last()) else {
        return Vec::new();
    };
    std::iter::once(mapping.to_screen(0.0, first.duty))
        .chain(
            sorted
                .iter()
                .map(|point| mapping.to_screen(point.temp.min(MAX_TEMP), point.duty)),
        )
        .chain(std::iter::once(mapping.to_screen(MAX_TEMP, last.duty)))
        .collect()
}

fn draw_curve(
    painter: &egui::Painter,
    mapping: &Mapping,
    palette: &Palette,
    points: &[CurvePoint],
) {
    let line = line_points(mapping, points);
    if line.len() < 2 {
        return;
    }
    let fill = palette.accent.gamma_multiply(0.12);
    let bottom = mapping.area.bottom();
    for pair in line.windows(2) {
        if (pair[1].x - pair[0].x).abs() < f32::EPSILON {
            continue;
        }
        painter.add(Shape::convex_polygon(
            vec![
                pair[0],
                pair[1],
                pos2(pair[1].x, bottom),
                pos2(pair[0].x, bottom),
            ],
            fill,
            Stroke::NONE,
        ));
    }
    painter.line(line, Stroke::new(2.5, palette.accent));
}

fn draw_live(painter: &egui::Painter, mapping: &Mapping, palette: &Palette, live: &Live) {
    let Some(temp) = live.temp.filter(|temp| (0.0..=MAX_TEMP).contains(temp)) else {
        return;
    };
    let top = mapping.to_screen(temp, MAX_DUTY);
    let bottom = mapping.to_screen(temp, 0.0);
    painter.add(Shape::dashed_line(
        &[top, bottom],
        Stroke::new(1.5, palette.warn),
        4.0,
        3.0,
    ));
    let Some(duty) = live.output else {
        return;
    };
    let center = mapping.to_screen(temp, duty);
    painter.circle(center, 5.0, palette.warn, Stroke::new(2.0, palette.surface));
    let (align, offset) = if temp > MAX_TEMP * 0.7 {
        (Align2::RIGHT_BOTTOM, vec2(-10.0, -8.0))
    } else {
        (Align2::LEFT_BOTTOM, vec2(10.0, -8.0))
    };
    tag(
        painter,
        palette,
        center + offset,
        align,
        &format!("{temp:.0} °C → {duty:.0}%"),
        palette.warn,
    );
}
