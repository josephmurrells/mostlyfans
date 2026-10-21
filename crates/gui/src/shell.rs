use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, CornerRadius, FontId, Id, Key, KeyboardShortcut, Margin, Modifiers, RichText,
    Role, Sense, Stroke, UiBuilder, WidgetInfo, pos2, vec2,
};
use fanforge_core::config::Config;
use fanforge_core::state::{ControlState, HardwareStatus};

use crate::alerts::{self, Action, AlertArea};
use crate::changes;
use crate::connection::{ControlLink, ControlStatus, Request, RequestKind};
use crate::curves::CurveEditor;
use crate::fans::{FanPage, FansView};
use crate::format;
use crate::history::History;
use crate::icons;
use crate::notices::Notices;
use crate::overview::{self, OverviewAction, OverviewPage};
use crate::pawnio_setup::PawnIoPanel;
use crate::pins;
use crate::prefs::{Page, Prefs};
use crate::settings::{AutostartPanel, SettingsView};
use crate::status::{self, Overall};
use crate::temperatures::{self, TemperaturesAction, TemperaturesPage};
use crate::theme::{self, BODY, SECTION, SPACE_L, SPACE_M, SPACE_S, SPACE_XL, Tone};
use crate::widgets;

const APPLY_SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
const COMPACT_BELOW: f32 = 860.0;
const NAV_WIDTH: f32 = 208.0;
const NAV_COMPACT_WIDTH: f32 = 60.0;
const CONTENT_MAX_WIDTH: f32 = 1120.0;

const PAGES: [(Page, &str, &str); 5] = [
    (Page::Overview, icons::OVERVIEW, "Overview"),
    (Page::Temperatures, icons::TEMPERATURES, "Temperatures"),
    (Page::Fans, icons::FANS, "Fans"),
    (Page::Curves, icons::CURVES, "Curves"),
    (Page::Settings, icons::SETTINGS, "Settings"),
];

struct PendingApply {
    config: Config,
    accepted_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchChoice {
    ApplyAndSwitch,
    DiscardAndSwitch,
    Cancel,
}

pub struct Shell {
    link: Box<dyn ControlLink>,
    notices: Notices,
    autostart: AutostartPanel,
    pawnio: PawnIoPanel,
    pub prefs: Prefs,
    prefs_changed: bool,
    draft: Option<Config>,
    dirty: bool,
    pending: Option<PendingApply>,
    switch_prompt: Option<String>,
    queued_switch: Option<String>,
    profile_override: Option<(String, Instant)>,
    history: History,
    alerts: AlertArea,
    toasts: widgets::Toasts,
    fans: FansView,
    curves: CurveEditor,
    settings: SettingsView,
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

impl Shell {
    pub fn new(
        link: Box<dyn ControlLink>,
        notices: Notices,
        autostart: AutostartPanel,
        pawnio: PawnIoPanel,
        prefs: Prefs,
    ) -> Self {
        Self {
            link,
            notices,
            autostart,
            pawnio,
            prefs,
            prefs_changed: false,
            draft: None,
            dirty: false,
            pending: None,
            switch_prompt: None,
            queued_switch: None,
            profile_override: None,
            history: History::default(),
            alerts: AlertArea::default(),
            toasts: widgets::Toasts::default(),
            fans: FansView::default(),
            curves: CurveEditor::default(),
            settings: SettingsView::default(),
        }
    }

    pub fn take_prefs_changed(&mut self) -> bool {
        std::mem::take(&mut self.prefs_changed)
    }

    pub fn view(&self) -> (ControlStatus, Option<Arc<ControlState>>) {
        self.link.view()
    }

    pub fn shown_profile(&self, state: &ControlState) -> String {
        self.profile_override.as_ref().map_or_else(
            || state.config.active_profile.clone(),
            |(name, _)| name.clone(),
        )
    }

    pub fn logic(&mut self) {
        let (_, state) = self.link.view();
        if let Some(state) = &state {
            self.history.record(state, Instant::now());
        }
        self.handle_results(state.as_deref());
        self.settle_pending(state.as_deref());
        self.autostart.poll(&self.notices);
    }

    pub fn request_profile(&mut self, name: String) -> bool {
        let (_, state) = self.link.view();
        if state
            .as_deref()
            .is_some_and(|state| self.shown_profile(state) == name)
        {
            return false;
        }
        if self.pending.is_some() {
            self.queued_switch = Some(name);
            false
        } else if self.dirty {
            self.switch_prompt = Some(name);
            true
        } else {
            self.switch_profile(name);
            false
        }
    }

    fn switch_profile(&mut self, name: String) {
        if let Some(draft) = self.draft.as_mut() {
            draft.active_profile = name.clone();
        }
        self.link.send(Request::SetActiveProfile(name.clone()));
        self.profile_override = Some((name, Instant::now()));
    }

    fn apply(&mut self) {
        if let Some(draft) = &self.draft {
            self.link
                .send(Request::ReplaceConfig(Box::new(draft.clone())));
            self.pending = Some(PendingApply {
                config: draft.clone(),
                accepted_at: None,
            });
        }
    }

    fn revert(&mut self) {
        self.dirty = false;
        self.draft = None;
        self.pending = None;
    }

    fn handle_results(&mut self, state: Option<&ControlState>) {
        for result in self.link.take_results() {
            match (result.kind, result.outcome) {
                (RequestKind::Config, Ok(())) => {
                    if let Some(pending) = self.pending.as_mut() {
                        pending.accepted_at = Some(Instant::now());
                    }
                }
                (RequestKind::Config, Err(message)) => {
                    self.pending = None;
                    self.queued_switch = None;
                    self.toasts
                        .error(format!("Your changes were rejected: {message}"));
                }
                (RequestKind::Profile | RequestKind::Identify | RequestKind::Calibrate, Ok(())) => {
                }
                (RequestKind::Profile, Err(message)) => {
                    if let Some((name, _)) = self.profile_override.take()
                        && let (Some(draft), Some(state)) = (self.draft.as_mut(), state)
                        && draft.active_profile == name
                    {
                        draft.active_profile = state.config.active_profile.clone();
                    }
                    self.toasts
                        .error(format!("The profile could not be switched: {message}"));
                }
                (RequestKind::Identify, Err(message)) => {
                    self.toasts.error(format!("Identify failed: {message}"));
                }
                (RequestKind::Calibrate, Err(message)) => {
                    self.toasts
                        .error(format!("Calibration could not start: {message}"));
                }
            }
        }
    }

    fn settle_pending(&mut self, state: Option<&ControlState>) {
        let Some(pending) = &self.pending else {
            return;
        };
        let Some(accepted_at) = pending.accepted_at else {
            return;
        };
        let visible = state.is_some_and(|state| {
            let expected = Config {
                calibration: state.config.calibration.clone(),
                ..pending.config.clone()
            };
            state.config == expected
        });
        if visible || accepted_at.elapsed() >= APPLY_SETTLE_TIMEOUT {
            self.pending = None;
            self.dirty = false;
            self.draft = None;
            if visible {
                self.toasts.info(Tone::Ok, "Changes applied");
            } else {
                self.toasts.info(
                    Tone::Warn,
                    "Changes sent, but fan control has not confirmed them yet",
                );
            }
            if let Some(name) = self.queued_switch.take() {
                self.switch_profile(name);
            }
        }
    }

    fn refresh_draft(&mut self, state: Option<&ControlState>) {
        let Some(state) = state else {
            return;
        };
        if self.profile_override.as_ref().is_some_and(|(name, at)| {
            &state.config.active_profile == name || at.elapsed() >= APPLY_SETTLE_TIMEOUT
        }) {
            self.profile_override = None;
        }
        if self.dirty
            && self.pending.is_none()
            && self
                .draft
                .as_ref()
                .is_some_and(|draft| changes::count(&state.config, draft) == 0)
        {
            self.dirty = false;
        }
        if !self.dirty {
            let mut draft = state.config.clone();
            if let Some((name, _)) = &self.profile_override {
                draft.active_profile = name.clone();
            }
            self.draft = Some(draft);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let (status, state) = self.link.view();
        self.refresh_draft(state.as_deref());
        let overall = status::overall(&status, state.as_deref());
        let ctx = ui.ctx().clone();
        let palette = theme::of(ui);
        let compact = ui.max_rect().width() < COMPACT_BELOW;

        if self.dirty
            && self.pending.is_none()
            && ui.input_mut(|input| {
                input.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::S))
            })
        {
            self.apply();
        }

        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(if compact {
                NAV_COMPACT_WIDTH
            } else {
                NAV_WIDTH
            })
            .frame(
                egui::Frame::new()
                    .fill(palette.surface)
                    .inner_margin(Margin::symmetric(8, SPACE_L as i8)),
            )
            .show(ui, |ui| self.nav(ui, compact, state.as_deref()));
        egui::Panel::top("status")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(palette.bg)
                    .inner_margin(Margin::symmetric(SPACE_XL as i8, SPACE_M as i8)),
            )
            .show(ui, |ui| self.status_bar(ui, &overall, state.as_deref()));
        if self.dirty {
            egui::Panel::bottom("unsaved")
                .frame(
                    egui::Frame::new()
                        .fill(palette.surface)
                        .inner_margin(Margin::symmetric(SPACE_XL as i8, 10)),
                )
                .show(ui, |ui| self.unsaved_bar(ui, state.as_deref()));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(palette.bg).inner_margin(Margin {
                left: SPACE_XL as i8,
                right: SPACE_XL as i8,
                top: 0,
                bottom: SPACE_L as i8,
            }))
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(CONTENT_MAX_WIDTH));
                self.alert_area(ui, &status, state.as_deref());
                let editable = self.pending.is_none();
                ui.scope_builder(UiBuilder::new().scope_id(Id::unique("page")), |ui| {
                    ui.add_enabled_ui(editable, |ui| {
                        self.page(ui, &status, state.as_deref(), overall.is_live());
                    });
                });
            });
        self.profile_prompt(&ctx, state.as_deref());
        self.toasts.show(&ctx);
    }

    fn nav(&mut self, ui: &mut egui::Ui, compact: bool, state: Option<&ControlState>) {
        let palette = theme::of(ui);
        ui.horizontal(|ui| {
            ui.add_space(if compact { 10.0 } else { 8.0 });
            ui.label(
                RichText::new(icons::FANS)
                    .font(theme::icon_font(20.0))
                    .color(palette.accent),
            );
            if !compact {
                ui.label(widgets::value("FanForge", SECTION));
            }
        });
        ui.add_space(SPACE_XL);
        for (page, icon, label) in PAGES {
            if nav_item(ui, compact, self.prefs.page == page, icon, label).clicked() {
                self.prefs.page = page;
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            if let Some(state) = state
                && !compact
            {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(widgets::caption(ui, format!("Version {}", state.version)));
                });
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, overall: &Overall, state: Option<&ControlState>) {
        ui.horizontal(|ui| {
            let pill = widgets::pill(ui, overall.tone(), &overall.label());
            if let Some(state) = state {
                pill.on_hover_ui(|ui| {
                    let (text, tone) = format::hardware(&state.hardware);
                    widgets::status(ui, tone, &text);
                    if let Some(board) = &state.board {
                        ui.label(format!(
                            "{} {}",
                            board.identity.manufacturer, board.identity.product
                        ));
                        ui.label(widgets::muted(format!(
                            "{} ({:#06x}) · {} fan headers{}",
                            board.chip_name,
                            board.chip_id,
                            board.fan_headers,
                            if board.fan_control {
                                ""
                            } else {
                                " · monitor only"
                            }
                        )));
                    }
                });
                ui.add_space(SPACE_S);
                let sensor = format::critical_sensor_label(state);
                let reading = state
                    .readings
                    .temperature(&state.config.safety.critical_sensor);
                ui.label(widgets::muted(sensor));
                let palette = theme::of(ui);
                let color = if overall.is_live() {
                    palette.text
                } else {
                    palette.text_muted
                };
                ui.label(
                    RichText::new(format::whole_celsius(reading))
                        .font(theme::semibold(BODY))
                        .color(color),
                );
                if !overall.is_live() {
                    ui.label(widgets::caption(ui, "last known"));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(state) = state {
                    self.profile_switcher(ui, state);
                }
            });
        });
    }

    fn profile_switcher(&mut self, ui: &mut egui::Ui, state: &ControlState) {
        let shown = self.shown_profile(state);
        let mut chosen = None;
        egui::ComboBox::from_id_salt("profile-switcher")
            .selected_text(RichText::new(&shown).strong())
            .width(160.0)
            .show_ui(ui, |ui| {
                for name in state.config.profiles.keys() {
                    if ui.selectable_label(name == &shown, name).clicked() && name != &shown {
                        chosen = Some(name.clone());
                    }
                }
            })
            .response
            .on_hover_text("Active profile");
        ui.label(widgets::muted("Profile"));
        if let Some(name) = chosen {
            self.request_profile(name);
        }
    }

    fn unsaved_bar(&mut self, ui: &mut egui::Ui, state: Option<&ControlState>) {
        let count = match (state, &self.draft) {
            (Some(state), Some(draft)) => changes::count(&state.config, draft),
            _ => 0,
        };
        let applying = self.pending.is_some();
        ui.horizontal(|ui| {
            if applying {
                widgets::spinner_label(ui, "Applying…");
            } else {
                widgets::dot(ui, Tone::Warn);
                ui.label(
                    RichText::new(plural(count, "unsaved change", "unsaved changes")).strong(),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let apply = ui.add_enabled_ui(!applying, |ui| {
                    widgets::primary_button(ui, "Apply").on_hover_text("Ctrl+S")
                });
                if apply.inner.clicked() {
                    self.apply();
                }
                if ui
                    .add_enabled(!applying, egui::Button::new("Revert"))
                    .clicked()
                {
                    self.revert();
                }
            });
        });
    }

    fn alert_area(
        &mut self,
        ui: &mut egui::Ui,
        status: &ControlStatus,
        state: Option<&ControlState>,
    ) {
        let driver_missing =
            state.is_some_and(|state| state.hardware == HardwareStatus::DriverMissing);
        let driver = self.pawnio.alert(driver_missing);
        let alerts = alerts::collect(status, state, &self.notices.list(), driver);
        match self.alerts.show(ui, alerts) {
            Some(Action::RestartControl) => {
                if let Err(error) = self.link.restart() {
                    self.toasts
                        .error(format!("Fan control could not restart: {error}"));
                }
            }
            Some(Action::InstallPawnIo) => self.pawnio.start(ui.ctx()),
            Some(Action::RepairAutostart) => self.autostart.repair(ui.ctx()),
            None => {}
        }
    }

    fn page(
        &mut self,
        ui: &mut egui::Ui,
        status: &ControlStatus,
        state: Option<&ControlState>,
        live: bool,
    ) {
        let (Some(state), Some(draft)) = (state, self.draft.as_mut()) else {
            ui.add_space(SPACE_XL * 2.0);
            ui.vertical_centered(|ui| {
                ui.spinner();
                ui.label(widgets::muted("Starting fan control…"));
            });
            return;
        };
        let changed = match self.prefs.page {
            Page::Overview => {
                let sensors = pins::sensors(&self.prefs, state);
                let fans = pins::fans(&self.prefs, state);
                let page = OverviewPage {
                    state,
                    history: &self.history,
                    live,
                    offer_setup: !self.prefs.setup_skipped,
                    sensors: &sensors,
                    fans: &fans,
                };
                let action = egui::ScrollArea::vertical()
                    .show(ui, |ui| overview::show(ui, &page))
                    .inner;
                match action {
                    Some(OverviewAction::OpenFan(fan)) => {
                        self.fans.select(fan);
                        self.prefs.page = Page::Fans;
                    }
                    Some(OverviewAction::OpenFans) => self.prefs.page = Page::Fans,
                    Some(OverviewAction::OpenTemperatures) => {
                        self.prefs.page = Page::Temperatures;
                    }
                    Some(OverviewAction::UnpinSensor(sensor)) => {
                        pins::set_sensor(&mut self.prefs, state, &sensor, false);
                        self.prefs_changed = true;
                    }
                    Some(OverviewAction::UnpinFan(fan)) => {
                        pins::set_fan(&mut self.prefs, state, &fan, false);
                        self.prefs_changed = true;
                    }
                    Some(OverviewAction::StartSetup) => {
                        if let Some(fan) = overview::unnamed_fans(state, &state.config).first() {
                            self.fans.select(fan.clone());
                        }
                        self.prefs.page = Page::Fans;
                    }
                    Some(OverviewAction::SkipSetup) => self.prefs.setup_skipped = true,
                    None => {}
                }
                false
            }
            Page::Temperatures => {
                let pinned = pins::sensors(&self.prefs, state).into_iter().collect();
                let page = TemperaturesPage {
                    state,
                    history: &self.history,
                    pinned: &pinned,
                    customised: pins::customised(&self.prefs),
                    live,
                };
                let action = egui::ScrollArea::vertical()
                    .show(ui, |ui| temperatures::show(ui, &page))
                    .inner;
                match action {
                    Some(TemperaturesAction::Pin(sensor)) => {
                        pins::set_sensor(&mut self.prefs, state, &sensor, true);
                        self.prefs_changed = true;
                    }
                    Some(TemperaturesAction::Unpin(sensor)) => {
                        pins::set_sensor(&mut self.prefs, state, &sensor, false);
                        self.prefs_changed = true;
                    }
                    Some(TemperaturesAction::ResetOverview) => {
                        pins::reset(&mut self.prefs);
                        self.prefs_changed = true;
                    }
                    None => {}
                }
                false
            }
            Page::Fans => {
                let pinned_fans = pins::fans(&self.prefs, state);
                let outcome = self.fans.show(
                    ui,
                    FanPage {
                        state,
                        draft,
                        link: &*self.link,
                        identify_block: identify_block(status, state),
                        setup_skipped: self.prefs.setup_skipped,
                        pinned: &pinned_fans,
                    },
                );
                if let Some((fan, pinned)) = outcome.pin {
                    pins::set_fan(&mut self.prefs, state, &fan, pinned);
                    self.prefs_changed = true;
                }
                if outcome.skip_setup {
                    self.prefs.setup_skipped = true;
                }
                if outcome.resume_setup {
                    self.prefs.setup_skipped = false;
                }
                outcome.changed
            }
            Page::Curves => self.curves.show(ui, state, draft),
            Page::Settings => self.settings.show(ui, state, draft, &mut self.autostart),
        };
        self.dirty |= changed;
    }

    fn profile_prompt(&mut self, ctx: &egui::Context, state: Option<&ControlState>) {
        let Some(name) = self.switch_prompt.clone() else {
            return;
        };
        let count = match (state, &self.draft) {
            (Some(state), Some(draft)) => changes::count(&state.config, draft),
            _ => 0,
        };
        let draft_has_profile = self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.profiles.contains_key(&name));
        let mut choice = None;
        let modal = egui::Modal::new(Id::unique("switch-profile")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(widgets::value(format!("Switch to {name}?"), SECTION));
            ui.add_space(SPACE_S);
            ui.label(widgets::muted(format!(
                "You have {}. Apply before switching, or discard to keep the profile as it was.",
                plural(count, "unsaved change", "unsaved changes")
            )));
            ui.add_space(SPACE_L);
            ui.horizontal(|ui| {
                let exists = draft_has_profile;
                let apply = ui.add_enabled_ui(exists, |ui| {
                    widgets::primary_button(ui, "Apply and switch").on_disabled_hover_text(
                        "Your unsaved changes rename or delete this profile",
                    )
                });
                if apply.inner.clicked() {
                    choice = Some(SwitchChoice::ApplyAndSwitch);
                }
                if ui.button("Discard and switch").clicked() {
                    choice = Some(SwitchChoice::DiscardAndSwitch);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(SwitchChoice::Cancel);
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some(SwitchChoice::Cancel);
        }
        match choice {
            Some(SwitchChoice::ApplyAndSwitch) => {
                if let Some(draft) = self.draft.as_mut() {
                    draft.active_profile = name;
                }
                self.apply();
                self.switch_prompt = None;
            }
            Some(SwitchChoice::DiscardAndSwitch) => {
                self.revert();
                self.switch_prompt = None;
                self.switch_profile(name);
            }
            Some(SwitchChoice::Cancel) => self.switch_prompt = None,
            None => {}
        }
    }
}

fn identify_block(status: &ControlStatus, state: &ControlState) -> Option<&'static str> {
    if matches!(status, ControlStatus::Stopped(_)) {
        Some("Fan control is stopped")
    } else if state.monitor_only {
        Some("Monitor-only mode: FanForge is not controlling the fans")
    } else if state.hardware != HardwareStatus::Ready {
        Some("The fan controller is not available")
    } else {
        None
    }
}

fn nav_item(
    ui: &mut egui::Ui,
    compact: bool,
    selected: bool,
    icon: &str,
    label: &str,
) -> egui::Response {
    let palette = theme::of(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(
            rect,
            CornerRadius::same(theme::CONTROL_RADIUS),
            palette.fill(Tone::Accent),
        );
        painter.line_segment(
            [
                pos2(rect.left() + 1.0, rect.top() + 10.0),
                pos2(rect.left() + 1.0, rect.bottom() - 10.0),
            ],
            Stroke::new(3.0, palette.accent),
        );
    } else if response.hovered() {
        painter.rect_filled(
            rect,
            CornerRadius::same(theme::CONTROL_RADIUS),
            palette.control,
        );
    }
    let color = if selected {
        palette.text
    } else {
        palette.text_muted
    };
    let icon_x = if compact {
        rect.center().x
    } else {
        rect.left() + 22.0
    };
    painter.text(
        pos2(icon_x, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        theme::icon_font(16.0),
        if selected { palette.accent } else { color },
    );
    if !compact {
        let font = if selected {
            theme::semibold(BODY)
        } else {
            FontId::proportional(BODY)
        };
        painter.text(
            pos2(rect.left() + 44.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            color,
        );
    }
    response.widget_info(|| WidgetInfo::selected(Role::Tab, true, selected, label));
    let response = if compact {
        response.on_hover_text(label)
    } else {
        response
    };
    ui.add_space(2.0);
    response
}

#[cfg(test)]
mod tests;
