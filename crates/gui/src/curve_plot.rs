use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, PointerButton, Pos2, Rect, Sense,
    Shape, Stroke, Vec2, pos2, vec2,
};
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
const FINE_STEP: f32 = 1.0;
const COARSE_STEP: f32 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Remove,
    Add,
}

#[derive(Debug, Default)]
pub struct Interaction {
    dragging: Option<Drag>,
    pub refused: Option<Refusal>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Drag {
    index: usize,
    low: f32,
    high: f32,
}

impl Drag {
    fn start(points: &[CurvePoint], index: usize) -> Self {
        let (low, high) = neighbour_bounds(points, index);
        Self { index, low, high }
    }

    fn target(self, cursor: CurvePoint, step: f32) -> CurvePoint {
        CurvePoint::new(
            snap_between(cursor.temp, step, self.low, self.high),
            snap(cursor.duty, step, MAX_DUTY),
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PointLimits {
    pub min: usize,
    pub max: usize,
}

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
        CurvePoint::new(temp, duty)
    }
}

fn snap(value: f32, step: f32, max: f32) -> f32 {
    ((value / step).round() * step).clamp(0.0, max)
}

fn snapped(point: CurvePoint, step: f32) -> CurvePoint {
    CurvePoint::new(
        snap(point.temp, step, MAX_TEMP),
        snap(point.duty, step, MAX_DUTY),
    )
}

fn neighbour_bounds(points: &[CurvePoint], index: usize) -> (f32, f32) {
    let temp = points[index].temp;
    let low = index
        .checked_sub(1)
        .and_then(|before| points.get(before))
        .map_or(0.0, |point| point.temp);
    let high = points.get(index + 1).map_or(MAX_TEMP, |point| point.temp);
    (low.min(high).min(temp), low.max(high).max(temp))
}

fn stacked_pick(points: &[CurvePoint], index: usize, rightward: bool) -> usize {
    let target = points[index];
    let mut stacked = points
        .iter()
        .enumerate()
        .filter(|(_, point)| **point == target)
        .map(|(stacked, _)| stacked);
    let picked = if rightward {
        stacked.next_back()
    } else {
        stacked.next()
    };
    picked.unwrap_or(index)
}

fn snap_between(value: f32, step: f32, low: f32, high: f32) -> f32 {
    let clamped = value.max(low).min(high);
    let within = |candidate: f32| (low..=high).contains(&candidate);
    let snapped = snap(clamped, step, MAX_TEMP);
    if within(snapped) {
        return snapped;
    }
    let inward = if snapped > high {
        snapped - step
    } else {
        snapped + step
    };
    if within(inward) { inward } else { clamped }
}

fn point_at(mapping: &Mapping, points: &[CurvePoint], cursor: Pos2) -> Option<usize> {
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
    interaction: &mut Interaction,
    limits: PointLimits,
    live: &Live,
    floor: Option<&FloorMark>,
) -> bool {
    let size = vec2(ui.available_width(), HEIGHT);
    let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
    let mapping = Mapping::new(response.rect, false);
    let palette = theme::of(ui);
    let step = if ui.input(|input| input.modifiers.shift) {
        COARSE_STEP
    } else {
        FINE_STEP
    };

    let pointer = response
        .interact_pointer_pos()
        .or_else(|| response.hover_pos());
    let nearest = pointer.and_then(|cursor| point_at(&mapping, points, cursor));

    let mut changed = false;
    let mut refused = None;
    if response.drag_started_by(PointerButton::Primary) {
        let origin = ui.input(|input| input.pointer.press_origin()).or(pointer);
        let rightward = origin
            .zip(pointer)
            .is_some_and(|(origin, cursor)| cursor.x > origin.x);
        interaction.dragging = origin
            .and_then(|cursor| point_at(&mapping, points, cursor))
            .map(|index| Drag::start(points, stacked_pick(points, index, rightward)));
    }
    if response.dragged_by(PointerButton::Primary)
        && let (Some(drag), Some(cursor)) = (interaction.dragging, pointer)
        && let Some(point) = points.get_mut(drag.index)
    {
        let moved = drag.target(mapping.to_curve(cursor), step);
        if moved != *point {
            *point = moved;
            changed = true;
        }
    }
    if !response.dragged_by(PointerButton::Primary) {
        interaction.dragging = None;
    }
    let idle = interaction.dragging.is_none() && !ui.input(|input| input.pointer.primary_down());
    let removing = if response.double_clicked() && idle {
        match (nearest, pointer) {
            (None, Some(cursor)) if points.len() < limits.max => {
                let point = snapped(mapping.to_curve(cursor), FINE_STEP);
                let at = points.partition_point(|existing| existing.temp <= point.temp);
                points.insert(at, point);
                changed = true;
                None
            }
            (None, Some(_)) => {
                refused = Some(Refusal::Add);
                None
            }
            (nearest, _) => nearest,
        }
    } else if response.secondary_clicked() && idle {
        nearest
    } else {
        None
    };
    if let Some(index) = removing {
        if points.len() > limits.min {
            points.remove(index);
            changed = true;
        } else {
            refused = Some(Refusal::Remove);
        }
    }
    if changed {
        interaction.refused = None;
    } else if refused.is_some() {
        interaction.refused = refused;
    }
    let dragging = interaction.dragging.map(|drag| drag.index);
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
        let active = Some(index) == dragging || Some(index) == nearest;
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

#[cfg(test)]
mod tests {
    use eframe::egui::{Event, Modifiers};
    use egui_kittest::Harness;

    use super::*;

    const GRAPH: PointLimits = PointLimits { min: 1, max: 4 };
    const LINE: PointLimits = PointLimits { min: 2, max: 2 };

    struct Plot {
        points: Vec<CurvePoint>,
        interaction: Interaction,
        limits: PointLimits,
        frame: Rect,
    }

    fn harness(points: Vec<CurvePoint>, limits: PointLimits) -> Harness<'static, Plot> {
        let plot = Plot {
            points,
            interaction: Interaction::default(),
            limits,
            frame: Rect::NOTHING,
        };
        let mut harness = Harness::builder()
            .with_size(vec2(640.0, 400.0))
            .with_step_dt(1.0 / 60.0)
            .with_context_setup(theme::install)
            .build_ui_state(
                |ui, plot: &mut Plot| {
                    plot.frame =
                        Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), HEIGHT));
                    let live = Live {
                        temp: None,
                        output: None,
                    };
                    show(
                        ui,
                        &mut plot.points,
                        &mut plot.interaction,
                        plot.limits,
                        &live,
                        None,
                    );
                },
                plot,
            );
        harness.run_steps(2);
        harness
    }

    fn screen(harness: &Harness<'_, Plot>, temp: f32, duty: f32) -> Pos2 {
        Mapping::new(harness.state().frame, false).to_screen(temp, duty)
    }

    fn button(harness: &mut Harness<'_, Plot>, pos: Pos2, button: PointerButton, pressed: bool) {
        harness.event(Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        });
        harness.run_steps(1);
    }

    fn move_to(harness: &mut Harness<'_, Plot>, pos: Pos2) {
        harness.hover_at(pos);
        harness.run_steps(1);
    }

    fn click_with(harness: &mut Harness<'_, Plot>, pos: Pos2, pointer: PointerButton) {
        move_to(harness, pos);
        button(harness, pos, pointer, true);
        button(harness, pos, pointer, false);
        harness.run_steps(2);
    }

    fn double_click(harness: &mut Harness<'_, Plot>, pos: Pos2) {
        move_to(harness, pos);
        for _ in 0..2 {
            button(harness, pos, PointerButton::Primary, true);
            button(harness, pos, PointerButton::Primary, false);
        }
        harness.run_steps(2);
    }

    fn drag_with(harness: &mut Harness<'_, Plot>, path: &[Pos2], pointer: PointerButton) {
        let (Some(&from), Some(&to)) = (path.first(), path.last()) else {
            return;
        };
        move_to(harness, from);
        button(harness, from, pointer, true);
        for pair in path.windows(2) {
            for fraction in [0.5, 1.0] {
                move_to(harness, pair[0].lerp(pair[1], fraction));
            }
        }
        button(harness, to, pointer, false);
        harness.run_steps(2);
    }

    fn three_points() -> Vec<CurvePoint> {
        vec![
            CurvePoint::new(30.0, 30.0),
            CurvePoint::new(60.0, 50.0),
            CurvePoint::new(85.0, 100.0),
        ]
    }

    #[test]
    fn snapping_rounds_to_the_step_and_stays_in_range() {
        assert_eq!(snap(42.4, FINE_STEP, MAX_TEMP), 42.0);
        assert_eq!(snap(42.6, FINE_STEP, MAX_TEMP), 43.0);
        assert_eq!(snap(42.4, COARSE_STEP, MAX_TEMP), 40.0);
        assert_eq!(snap(42.6, COARSE_STEP, MAX_TEMP), 45.0);
        assert_eq!(snap(-3.0, COARSE_STEP, MAX_DUTY), 0.0);
        assert_eq!(snap(124.0, COARSE_STEP, MAX_TEMP), MAX_TEMP);
        assert_eq!(snap(99.0, COARSE_STEP, MAX_DUTY), MAX_DUTY);
        assert_eq!(snap(130.0, FINE_STEP, MAX_TEMP), MAX_TEMP);
    }

    #[test]
    fn snapping_between_bounds_stays_on_the_grid_when_it_can() {
        assert_eq!(snap_between(70.0, COARSE_STEP, 30.0, 62.5), 60.0);
        assert_eq!(snap_between(10.0, COARSE_STEP, 32.5, 60.0), 35.0);
        assert_eq!(snap_between(44.0, COARSE_STEP, 30.0, 60.0), 45.0);
        assert_eq!(snap_between(70.0, COARSE_STEP, 61.0, 63.0), 63.0);
        assert_eq!(snap_between(31.0, FINE_STEP, 30.4, 30.6), 30.6);
        assert_eq!(snap_between(50.0, FINE_STEP, 50.0, 50.0), 50.0);
    }

    #[test]
    fn neighbour_bounds_follow_index_order() {
        let points = three_points();
        assert_eq!(neighbour_bounds(&points, 0), (0.0, 60.0));
        assert_eq!(neighbour_bounds(&points, 1), (30.0, 85.0));
        assert_eq!(neighbour_bounds(&points, 2), (60.0, MAX_TEMP));
        assert_eq!(
            neighbour_bounds(&[CurvePoint::new(40.0, 40.0)], 0),
            (0.0, MAX_TEMP)
        );
    }

    #[test]
    fn neighbour_bounds_stay_ordered_for_an_unsorted_list() {
        let points = vec![
            CurvePoint::new(70.0, 70.0),
            CurvePoint::new(30.0, 30.0),
            CurvePoint::new(50.0, 40.0),
        ];
        assert_eq!(neighbour_bounds(&points, 0), (0.0, 70.0));
        assert_eq!(neighbour_bounds(&points, 1), (30.0, 70.0));
        assert_eq!(neighbour_bounds(&points, 2), (30.0, MAX_TEMP));
    }

    #[test]
    fn stacked_points_are_picked_by_drag_direction() {
        let stacked = vec![
            CurvePoint::new(20.0, 10.0),
            CurvePoint::new(50.0, 40.0),
            CurvePoint::new(50.0, 40.0),
            CurvePoint::new(90.0, 90.0),
        ];
        assert_eq!(stacked_pick(&stacked, 1, false), 1);
        assert_eq!(stacked_pick(&stacked, 1, true), 2);
        assert_eq!(stacked_pick(&stacked, 3, true), 3);
    }

    #[test]
    fn a_stacked_point_can_be_dragged_out_either_way() {
        let stacked = vec![
            CurvePoint::new(20.0, 10.0),
            CurvePoint::new(50.0, 40.0),
            CurvePoint::new(50.0, 40.0),
            CurvePoint::new(90.0, 90.0),
        ];
        let mut harness = harness(stacked, GRAPH);
        let from = screen(&harness, 50.0, 40.0);
        let to = screen(&harness, 70.0, 40.0);
        drag_with(&mut harness, &[from, to], PointerButton::Primary);
        assert_eq!(harness.state().points[1], CurvePoint::new(50.0, 40.0));
        assert_eq!(harness.state().points[2], CurvePoint::new(70.0, 40.0));
    }

    #[test]
    fn a_point_added_in_the_middle_is_kept_in_order_and_drags_in_place() {
        let mut harness = harness(three_points(), GRAPH);
        let pos = screen(&harness, 45.0, 20.0);
        double_click(&mut harness, pos);
        assert_eq!(harness.state().points[1], CurvePoint::new(45.0, 20.0));
        assert_eq!(harness.state().points.len(), 4);
        let to = screen(&harness, 47.0, 25.0);
        drag_with(&mut harness, &[pos, to], PointerButton::Primary);
        assert_eq!(harness.state().points[1], CurvePoint::new(47.0, 25.0));
        assert_eq!(harness.state().points[2], CurvePoint::new(60.0, 50.0));
    }

    #[test]
    fn tied_points_separate_by_index() {
        let stacked = vec![
            CurvePoint::new(20.0, 10.0),
            CurvePoint::new(50.0, 40.0),
            CurvePoint::new(50.0, 60.0),
            CurvePoint::new(90.0, 90.0),
        ];
        assert_eq!(neighbour_bounds(&stacked, 1), (20.0, 50.0));
        assert_eq!(neighbour_bounds(&stacked, 2), (50.0, 90.0));
    }

    #[test]
    fn a_line_start_never_passes_its_end() {
        let line = [CurvePoint::new(40.0, 30.0), CurvePoint::new(80.0, 100.0)];
        assert_eq!(neighbour_bounds(&line, 0), (0.0, 80.0));
        assert_eq!(neighbour_bounds(&line, 1), (40.0, MAX_TEMP));
        let reversed = [CurvePoint::new(80.0, 30.0), CurvePoint::new(40.0, 100.0)];
        assert_eq!(neighbour_bounds(&reversed, 0), (0.0, 80.0));
        assert_eq!(neighbour_bounds(&reversed, 1), (40.0, MAX_TEMP));
    }

    #[test]
    fn a_point_dragged_onto_its_neighbour_cannot_pass_it_on_a_later_drag() {
        let mut harness = harness(three_points(), GRAPH);
        let first = [
            screen(&harness, 60.0, 50.0),
            screen(&harness, 85.0, 60.0),
            screen(&harness, 100.0, 60.0),
        ];
        drag_with(&mut harness, &first, PointerButton::Primary);
        assert_eq!(harness.state().points[1], CurvePoint::new(85.0, 60.0));
        let second = [
            screen(&harness, 85.0, 60.0),
            screen(&harness, 100.0, 60.0),
            screen(&harness, 115.0, 60.0),
        ];
        drag_with(&mut harness, &second, PointerButton::Primary);
        assert_eq!(harness.state().points[1], CurvePoint::new(85.0, 60.0));
        assert_eq!(harness.state().points[2], CurvePoint::new(85.0, 100.0));
    }

    #[test]
    fn right_clicking_a_point_removes_it() {
        let mut harness = harness(three_points(), GRAPH);
        let pos = screen(&harness, 60.0, 50.0);
        click_with(&mut harness, pos, PointerButton::Secondary);
        let plot = harness.state();
        assert_eq!(
            plot.points,
            vec![CurvePoint::new(30.0, 30.0), CurvePoint::new(85.0, 100.0)]
        );
        assert_eq!(plot.interaction.refused, None);
    }

    #[test]
    fn right_clicking_empty_space_changes_nothing() {
        let mut harness = harness(three_points(), GRAPH);
        let pos = screen(&harness, 110.0, 10.0);
        click_with(&mut harness, pos, PointerButton::Secondary);
        assert_eq!(harness.state().points, three_points());
        assert_eq!(harness.state().interaction.refused, None);
    }

    #[test]
    fn the_minimum_point_count_is_kept_and_flagged_until_the_next_change() {
        let line = vec![CurvePoint::new(40.0, 30.0), CurvePoint::new(80.0, 100.0)];
        let mut harness = harness(line.clone(), LINE);
        let pos = screen(&harness, 40.0, 30.0);
        click_with(&mut harness, pos, PointerButton::Secondary);
        assert_eq!(harness.state().points, line);
        assert_eq!(harness.state().interaction.refused, Some(Refusal::Remove));
        let from = screen(&harness, 80.0, 100.0);
        let to = screen(&harness, 90.0, 90.0);
        drag_with(&mut harness, &[from, to], PointerButton::Primary);
        assert_eq!(harness.state().points[1], CurvePoint::new(90.0, 90.0));
        assert_eq!(harness.state().interaction.refused, None);
    }

    #[test]
    fn adding_past_the_maximum_is_refused() {
        let mut points = three_points();
        points.push(CurvePoint::new(100.0, 100.0));
        let mut harness = harness(points.clone(), GRAPH);
        let pos = screen(&harness, 110.0, 20.0);
        double_click(&mut harness, pos);
        assert_eq!(harness.state().points, points);
        assert_eq!(harness.state().interaction.refused, Some(Refusal::Add));
    }

    #[test]
    fn double_clicking_empty_space_adds_a_point() {
        let mut harness = harness(three_points(), GRAPH);
        let pos = screen(&harness, 110.0, 20.0);
        double_click(&mut harness, pos);
        assert_eq!(harness.state().points.len(), 4);
        assert_eq!(harness.state().points[3], CurvePoint::new(110.0, 20.0));
    }

    #[test]
    fn dragging_snaps_and_stops_at_the_neighbour() {
        let mut harness = harness(three_points(), GRAPH);
        let path = [
            screen(&harness, 60.0, 50.0),
            screen(&harness, 85.0, 40.0),
            screen(&harness, 100.0, 30.0),
            screen(&harness, 120.0, 20.3),
        ];
        drag_with(&mut harness, &path, PointerButton::Primary);
        assert_eq!(harness.state().points[1], CurvePoint::new(85.0, 20.0));
        assert_eq!(harness.state().interaction.dragging, None);
    }

    #[test]
    fn holding_shift_snaps_to_coarse_steps() {
        let mut harness = harness(three_points(), GRAPH);
        harness.event(Event::ModifiersChanged(Modifiers::SHIFT));
        let from = screen(&harness, 60.0, 50.0);
        let to = screen(&harness, 67.4, 42.6);
        drag_with(&mut harness, &[from, to], PointerButton::Primary);
        assert_eq!(harness.state().points[1], CurvePoint::new(65.0, 45.0));
    }

    #[test]
    fn right_dragging_never_moves_a_point() {
        let mut harness = harness(three_points(), GRAPH);
        let from = screen(&harness, 60.0, 50.0);
        let to = screen(&harness, 70.0, 70.0);
        drag_with(&mut harness, &[from, to], PointerButton::Secondary);
        assert_eq!(harness.state().points, three_points());
    }

    #[test]
    fn a_drag_whose_release_was_missed_does_not_block_removal() {
        let mut harness = harness(three_points(), GRAPH);
        harness.state_mut().interaction.dragging = Some(Drag::start(&three_points(), 1));
        let pos = screen(&harness, 60.0, 50.0);
        click_with(&mut harness, pos, PointerButton::Secondary);
        assert_eq!(harness.state().points.len(), 2);
        assert_eq!(harness.state().interaction.dragging, None);
    }

    #[test]
    fn right_clicking_during_a_drag_removes_nothing() {
        let mut harness = harness(three_points(), GRAPH);
        let from = screen(&harness, 60.0, 50.0);
        let to = screen(&harness, 70.0, 70.0);
        move_to(&mut harness, from);
        button(&mut harness, from, PointerButton::Primary, true);
        move_to(&mut harness, from.lerp(to, 0.5));
        move_to(&mut harness, to);
        button(&mut harness, to, PointerButton::Secondary, true);
        button(&mut harness, to, PointerButton::Secondary, false);
        button(&mut harness, to, PointerButton::Primary, false);
        harness.run_steps(2);
        assert_eq!(harness.state().points.len(), 3);
        assert_eq!(harness.state().points[1], CurvePoint::new(70.0, 70.0));
    }
}
