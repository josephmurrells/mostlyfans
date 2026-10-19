use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Margin, Order, Pos2, Rect, Response, RichText,
    Role, Sense, Stroke, Vec2, WidgetInfo, pos2, vec2,
};

use crate::icons;
use crate::theme::{
    self, BODY, CAPTION, CONTROL_RADIUS, SECTION, SPACE_L, SPACE_M, SPACE_S, SPACE_XS, Tone,
};

const TOAST_DURATION: Duration = Duration::from_secs(4);
const DOT_RADIUS: f32 = 4.0;
const MIN_SPARK_SPAN: f32 = 0.05;

pub fn with_icon(icon: &str, text: &str) -> String {
    format!("{icon}  {text}")
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).weak()
}

pub fn caption(ui: &egui::Ui, text: impl Into<String>) -> RichText {
    RichText::new(text)
        .size(CAPTION)
        .color(theme::of(ui).text_muted)
}

pub fn value(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).font(theme::semibold(size))
}

pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let palette = theme::of(ui);
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.border))
        .corner_radius(theme::CARD_RADIUS)
        .inner_margin(Margin::same(SPACE_L as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn card_title(ui: &mut egui::Ui, title: &str, description: Option<&str>) {
    ui.label(value(title, SECTION));
    if let Some(description) = description {
        ui.label(caption(ui, description));
    }
    ui.add_space(SPACE_XS);
}

pub fn page_title(ui: &mut egui::Ui, title: &str, subtitle: Option<&str>) {
    ui.label(value(title, theme::PAGE));
    if let Some(subtitle) = subtitle {
        ui.label(muted(subtitle));
    }
    ui.add_space(SPACE_S);
}

pub fn dot(ui: &mut egui::Ui, tone: Tone) {
    let color = theme::of(ui).tone(tone);
    let (rect, _) =
        ui.allocate_exact_size(vec2(DOT_RADIUS * 2.0, DOT_RADIUS * 2.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), DOT_RADIUS, color);
}

pub fn status(ui: &mut egui::Ui, tone: Tone, text: &str) -> Response {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        dot(ui, tone);
        ui.label(text)
    })
    .inner
}

pub fn pill(ui: &mut egui::Ui, tone: Tone, text: &str) -> Response {
    let palette = theme::of(ui);
    egui::Frame::new()
        .fill(palette.fill(tone))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.horizontal(|ui| {
                dot(ui, tone);
                ui.label(RichText::new(text).color(palette.tone(tone)).strong());
            });
        })
        .response
}

pub fn chip(ui: &mut egui::Ui, text: &str) -> Response {
    let palette = theme::of(ui);
    egui::Frame::new()
        .fill(palette.control)
        .corner_radius(CornerRadius::same(CONTROL_RADIUS))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(CAPTION).color(palette.text));
        })
        .response
}

pub fn primary_button(ui: &mut egui::Ui, text: &str) -> Response {
    let palette = theme::of(ui);
    ui.add(
        egui::Button::new(RichText::new(text).color(palette.on_accent).strong())
            .fill(palette.accent)
            .stroke(Stroke::NONE),
    )
}

pub fn danger_button(ui: &mut egui::Ui, text: &str) -> Response {
    let palette = theme::of(ui);
    ui.add(
        egui::Button::new(RichText::new(text).color(palette.on_accent).strong())
            .fill(palette.danger)
            .stroke(Stroke::NONE),
    )
}

pub struct Segment<'a, T> {
    pub value: T,
    pub label: &'a str,
    pub disabled: Option<&'a str>,
}

impl<'a, T> Segment<'a, T> {
    pub fn new(value: T, label: &'a str) -> Self {
        Self {
            value,
            label,
            disabled: None,
        }
    }

    pub fn disabled_because(mut self, reason: Option<&'a str>) -> Self {
        self.disabled = reason;
        self
    }
}

pub fn segmented<T: PartialEq + Clone>(
    ui: &mut egui::Ui,
    value: &mut T,
    options: &[Segment<'_, T>],
) -> bool {
    let palette = theme::of(ui);
    let mut changed = false;
    egui::Frame::new()
        .fill(palette.control)
        .corner_radius(CornerRadius::same(CONTROL_RADIUS))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for option in options {
                    let selected = option.value == *value;
                    let enabled = option.disabled.is_none();
                    let font = if selected {
                        theme::semibold(BODY)
                    } else {
                        FontId::proportional(BODY)
                    };
                    let galley = ui.painter().layout_no_wrap(
                        option.label.to_owned(),
                        font,
                        Color32::PLACEHOLDER,
                    );
                    let size = vec2(galley.size().x + 2.0 * SPACE_M, 26.0);
                    let sense = if enabled {
                        Sense::click()
                    } else {
                        Sense::hover()
                    };
                    let (rect, response) = ui.allocate_exact_size(size, sense);
                    let painter = ui.painter();
                    let radius = CornerRadius::same(CONTROL_RADIUS - 2);
                    if selected {
                        painter.rect_filled(rect, radius, palette.segment);
                    } else if enabled && response.hovered() {
                        painter.rect_filled(rect, radius, palette.control_hover);
                    }
                    let color = match (enabled, selected) {
                        (false, _) => palette.text_muted.gamma_multiply(0.55),
                        (true, true) => palette.text,
                        (true, false) => palette.text_muted,
                    };
                    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
                    response.widget_info(|| {
                        WidgetInfo::selected(Role::RadioButton, enabled, selected, option.label)
                    });
                    let response = match option.disabled {
                        Some(reason) => response.on_hover_text(reason),
                        None => response,
                    };
                    if enabled && response.clicked() && !selected {
                        *value = option.value.clone();
                        changed = true;
                    }
                }
            });
        });
    changed
}

pub fn duty_bar(ui: &mut egui::Ui, width: f32, duty: Option<f32>, floor: Option<f32>) -> Response {
    let palette = theme::of(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(3), palette.control);
    if let Some(duty) = duty {
        let fraction = (duty / 100.0).clamp(0.0, 1.0);
        let filled = Rect::from_min_size(rect.min, vec2(rect.width() * fraction, rect.height()));
        painter.rect_filled(filled, CornerRadius::same(3), palette.accent);
    }
    if let Some(floor) = floor.filter(|floor| *floor > 0.0) {
        let x = rect.left() + rect.width() * (floor / 100.0).clamp(0.0, 1.0);
        painter.line_segment(
            [pos2(x, rect.top() - 3.0), pos2(x, rect.bottom() + 3.0)],
            Stroke::new(2.0, palette.text_muted),
        );
    }
    response
}

pub fn sparkline(
    ui: &mut egui::Ui,
    size: Vec2,
    points: &[(f32, f32)],
    range: (f32, f32),
    color: Color32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let span = match (points.first(), points.last()) {
        (Some(first), Some(last)) => last.0 - first.0,
        _ => 0.0,
    };
    if points.len() < 2 || span < MIN_SPARK_SPAN {
        return response;
    }
    let (low, high) = range;
    let span = (high - low).max(f32::EPSILON);
    let line: Vec<Pos2> = points
        .iter()
        .map(|(x, y)| {
            pos2(
                rect.left() + rect.width() * x.clamp(0.0, 1.0),
                rect.bottom() - rect.height() * ((y - low) / span).clamp(0.0, 1.0),
            )
        })
        .collect();
    let painter = ui.painter();
    let fill = color.gamma_multiply(0.12);
    for pair in line.windows(2) {
        let quad = vec![
            pair[0],
            pair[1],
            pos2(pair[1].x, rect.bottom()),
            pos2(pair[0].x, rect.bottom()),
        ];
        painter.add(egui::Shape::convex_polygon(quad, fill, Stroke::NONE));
    }
    painter.line(line, Stroke::new(1.5, color));
    response
}

pub fn confirm(ui: &mut egui::Ui, id: Id, label: &str, question: &str, enabled: bool) -> bool {
    let open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    if !open {
        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
            ui.data_mut(|data| data.insert_temp(id, true));
        }
        return false;
    }
    let mut confirmed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(question).color(theme::of(ui).danger));
        if danger_button(ui, "Yes").clicked() {
            confirmed = true;
            ui.data_mut(|data| data.remove::<bool>(id));
        }
        if ui.button("No").clicked() {
            ui.data_mut(|data| data.remove::<bool>(id));
        }
    });
    confirmed
}

pub fn spinner_label(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(14.0));
        ui.label(text);
    });
}

struct Toast {
    text: String,
    tone: Tone,
    at: Instant,
    sticky: bool,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    pub fn info(&mut self, tone: Tone, text: impl Into<String>) {
        self.push(tone, text.into(), false);
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.push(Tone::Danger, text.into(), true);
    }

    fn push(&mut self, tone: Tone, text: String, sticky: bool) {
        self.items.retain(|item| item.text != text);
        self.items.push(Toast {
            text,
            tone,
            at: Instant::now(),
            sticky,
        });
    }

    #[cfg(test)]
    pub fn texts(&self) -> Vec<&str> {
        self.items.iter().map(|item| item.text.as_str()).collect()
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        self.items
            .retain(|item| item.sticky || item.at.elapsed() < TOAST_DURATION);
        if self.items.is_empty() {
            return;
        }
        if self.items.iter().any(|item| !item.sticky) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let mut closed = None;
        egui::Area::new(Id::unique("toasts"))
            .order(Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-SPACE_L, -SPACE_L))
            .show(ctx, |ui| {
                let palette = theme::of(ui);
                for (index, item) in self.items.iter().enumerate() {
                    egui::Frame::new()
                        .fill(palette.surface_raised)
                        .stroke(Stroke::new(1.0, palette.border))
                        .corner_radius(theme::CARD_RADIUS)
                        .shadow(ui.visuals().popup_shadow)
                        .inner_margin(Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_max_width(360.0);
                            ui.horizontal(|ui| {
                                let icon = match item.tone {
                                    Tone::Danger => icons::ERROR,
                                    Tone::Warn => icons::WARNING,
                                    Tone::Ok => icons::CHECK,
                                    Tone::Neutral | Tone::Accent => icons::INFO,
                                };
                                ui.label(RichText::new(icon).color(palette.tone(item.tone)));
                                ui.label(&item.text);
                                if item.sticky && ui.small_button(icons::CLOSE).clicked() {
                                    closed = Some(index);
                                }
                            });
                        });
                    ui.add_space(SPACE_S);
                }
            });
        if let Some(index) = closed {
            self.items.remove(index);
        }
    }
}

pub fn card_grid<T>(
    ui: &mut egui::Ui,
    items: &[T],
    min_width: f32,
    mut add: impl FnMut(&mut egui::Ui, &T),
) {
    if items.is_empty() {
        return;
    }
    let gap = ui.spacing().item_spacing.x;
    let fit = ((ui.available_width() + gap) / (min_width + gap)).floor() as usize;
    let columns = fit.clamp(1, items.len());
    for row in items.chunks(columns) {
        ui.columns(columns, |cells| {
            for (cell, item) in cells.iter_mut().zip(row) {
                add(cell, item);
            }
        });
        ui.add_space(SPACE_S);
    }
}

pub fn clickable_card(
    ui: &mut egui::Ui,
    id: Id,
    label: &str,
    add: impl FnOnce(&mut egui::Ui),
) -> Response {
    let palette = theme::of(ui);
    let frame = egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.border))
        .corner_radius(theme::CARD_RADIUS)
        .inner_margin(Margin::same(SPACE_L as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    let rect = frame.response.rect;
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    if response.hovered() {
        ui.painter().rect_stroke(
            rect,
            theme::CARD_RADIUS,
            Stroke::new(1.0, palette.accent),
            egui::StrokeKind::Inside,
        );
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

pub fn section_heading(ui: &mut egui::Ui, title: &str) {
    ui.add_space(SPACE_S);
    ui.label(value(title, SECTION));
    ui.add_space(SPACE_XS);
}

pub fn list_item(
    ui: &mut egui::Ui,
    id: Id,
    selected: bool,
    label: &str,
    add: impl FnOnce(&mut egui::Ui),
) -> Response {
    let palette = theme::of(ui);
    let mut prepared = egui::Frame::new()
        .corner_radius(CONTROL_RADIUS)
        .inner_margin(Margin::symmetric(10, 8))
        .begin(ui);
    let width = prepared.content_ui.available_width();
    prepared.content_ui.set_width(width);
    add(&mut prepared.content_ui);
    let rect = prepared.allocate_space(ui).rect;
    let response = ui.interact(rect, id, Sense::click());
    prepared.frame.fill = if selected {
        palette.fill(Tone::Accent)
    } else if response.hovered() {
        palette.control
    } else {
        Color32::TRANSPARENT
    };
    prepared.paint(ui);
    if selected {
        ui.painter().line_segment(
            [
                pos2(rect.left() + 1.0, rect.top() + 8.0),
                pos2(rect.left() + 1.0, rect.bottom() - 8.0),
            ],
            Stroke::new(3.0, palette.accent),
        );
    }
    response.widget_info(|| WidgetInfo::selected(Role::Tab, true, selected, label));
    response
}
