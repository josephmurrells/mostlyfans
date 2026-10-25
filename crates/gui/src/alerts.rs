use std::collections::{BTreeMap, BTreeSet};

use eframe::egui::{self, Margin, RichText, Stroke};
use mostlyfans_core::engine::FanState;
use mostlyfans_core::state::{ControlState, HardwareStatus};
use mostlyfans_core::{FanId, FanKind};

use crate::connection::ControlStatus;
use crate::icons;
use crate::notices::Topic;
use crate::theme::{self, SPACE_S, Tone};
use crate::widgets;

const MAX_VISIBLE: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Critical,
    Stopped,
    Driver,
    Hardware,
    MonitorOnly,
    Warning,
    Notice,
}

impl Severity {
    fn tone(self) -> Tone {
        match self {
            Self::Critical | Self::Stopped | Self::Driver | Self::Hardware => Tone::Danger,
            Self::MonitorOnly | Self::Warning | Self::Notice => Tone::Warn,
        }
    }

    pub fn is_safety(self) -> bool {
        self != Self::Notice
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Notice(Topic),
    Conflict,
    GpuConflict,
}

impl Key {
    fn dismissable(self) -> bool {
        match self {
            Self::Notice(topic) => topic.dismissable(),
            Self::Conflict | Self::GpuConflict => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    RestartControl,
    InstallPawnIo,
    RepairAutostart,
    ResumeControl,
    RetryFan(FanId),
}

impl Action {
    fn label(&self) -> &'static str {
        match self {
            Self::RestartControl => "Restart control",
            Self::InstallPawnIo => "Install PawnIO",
            Self::RepairAutostart => "Repair autostart",
            Self::ResumeControl => "Resume control",
            Self::RetryFan(_) => "Try again",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub severity: Severity,
    pub text: String,
    pub detail: Option<String>,
    pub action: Option<Action>,
    pub busy: bool,
    pub key: Option<Key>,
}

impl Alert {
    pub fn new(severity: Severity, text: impl Into<String>) -> Self {
        Self {
            severity,
            text: text.into(),
            detail: None,
            action: None,
            busy: false,
            key: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    pub fn key(mut self, key: Key) -> Self {
        self.key = Some(key);
        self
    }

    pub fn busy(mut self) -> Self {
        self.busy = true;
        self
    }

    pub fn dismissable(&self) -> bool {
        matches!(self.severity, Severity::Notice | Severity::MonitorOnly)
            && self.key.is_some_and(Key::dismissable)
    }
}

pub fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

pub fn collect(
    status: &ControlStatus,
    state: Option<&ControlState>,
    notices: &[(Topic, String)],
    driver: Option<Alert>,
) -> Vec<Alert> {
    let mut alerts = Vec::new();
    if let ControlStatus::Stopped(reason) = status {
        alerts.push(
            Alert::new(Severity::Stopped, format!("Fan control stopped: {reason}"))
                .detail("The fans are back under BIOS control until control restarts.")
                .action(Action::RestartControl),
        );
    }
    alerts.extend(driver);
    let live = matches!(status, ControlStatus::Running);
    if let Some(state) = state.filter(|_| live) {
        let unverified = state.board.as_ref().filter(|board| !board.fan_control);
        let misconfigured = unverified.filter(|board| board.pwm_misconfigured);
        if state.critical {
            let text = if crate::format::nothing_driven(state) {
                "Critical temperature: the fans are under BIOS control"
            } else {
                "Critical temperature: every fan is at 100%"
            };
            alerts.push(Alert::new(Severity::Critical, text));
        }
        let safety = &state.config.safety;
        if !state.critical && state.critical_reached && !safety.critical_enabled {
            let sensor = state
                .readings
                .sensors
                .iter()
                .find(|sensor| sensor.id == safety.critical_sensor);
            let name = sensor.map_or_else(
                || safety.critical_sensor.to_string(),
                |sensor| sensor.label.clone(),
            );
            let reading = sensor
                .and_then(|sensor| sensor.celsius)
                .map_or_else(String::new, |celsius| format!(" ({celsius:.0} °C now)"));
            alerts.push(
                Alert::new(
                    Severity::Critical,
                    format!("{name} reached the critical temperature{reading}"),
                )
                .detail("The critical temperature override is off, so the fans were not forced to 100%."),
            );
        }
        if !safety.critical_enabled {
            alerts.push(Alert::new(
                Severity::Warning,
                "The critical temperature override is off: fans are not forced to 100% when it gets hot.",
            ));
        }
        if !safety.sensor_fallback_enabled {
            alerts.push(Alert::new(
                Severity::Warning,
                "The missing-sensor fallback is off: a fan whose sensor fails goes back to BIOS control instead of 100%.",
            ));
        }
        if let HardwareStatus::Unavailable(reason) = &state.hardware {
            alerts.push(Alert::new(
                Severity::Hardware,
                format!("Hardware unavailable: {reason}"),
            ));
        }
        if state.user_monitor_only {
            let mut alert = Alert::new(
                Severity::MonitorOnly,
                "Monitor-only mode is on: all fans are left to the BIOS and the graphics driver.",
            )
            .action(Action::ResumeControl);
            if let Some(board) = misconfigured {
                alert = alert.detail(format!(
                    "The BIOS left the fan outputs of the {} misconfigured, so the motherboard fans stay with the BIOS after you resume.",
                    board.chip_name
                ));
            } else if let Some(board) = unverified {
                alert = alert.detail(format!(
                    "MostlyFans doesn't support fan control on the {}, so the motherboard fans stay with the BIOS after you resume.",
                    board.chip_name
                ));
            } else if !state.conflicts.is_empty() {
                alert = alert.detail(format!(
                    "{} is running, so MostlyFans stays monitor-only after you resume until it closes.",
                    state.conflicts.join(", ")
                ));
            }
            alerts.push(alert);
        } else if let Some(board) = misconfigured.filter(|_| state.monitor_only) {
            alerts.push(
                Alert::new(
                    Severity::MonitorOnly,
                    format!(
                        "Monitor-only mode: the BIOS set up the fan outputs of the {} with inverted polarity, a known BIOS fault that Linux it87 also won't drive, so MostlyFans only reads temperatures and fan speeds and leaves the fans to the BIOS.",
                        board.chip_name
                    ),
                )
                .detail("Check the fan settings in the BIOS setup: switch the fan headers to PWM or Smart Fan mode, or update the BIOS, then restart the PC."),
            );
        } else if state.monitor_only {
            let (text, key) = if let Some(board) = unverified {
                (
                    format!(
                        "Monitor-only mode: MostlyFans doesn't support fan control on the {}, so it only reads temperatures and fan speeds and leaves the fans to the BIOS.",
                        board.chip_name
                    ),
                    None,
                )
            } else if state.conflicts.is_empty() {
                (
                    "Monitor-only mode: the fans are left to the BIOS.".to_owned(),
                    None,
                )
            } else if state.gpus.fans.is_empty() {
                (
                    format!(
                        "Monitor-only mode: {} is running, so the fans are left to the BIOS.",
                        state.conflicts.join(", ")
                    ),
                    Some(Key::Conflict),
                )
            } else {
                (
                    format!(
                        "Monitor-only mode: {} is running, so the fans are left to the BIOS and the graphics driver.",
                        state.conflicts.join(", ")
                    ),
                    Some(Key::Conflict),
                )
            };
            let mut alert = Alert::new(Severity::MonitorOnly, text);
            alert.key = key;
            alerts.push(alert);
        }
        if !state.user_monitor_only
            && state.gpus.monitor_only
            && !state.gpus.conflicts.is_empty()
            && !state.gpus.fans.is_empty()
        {
            alerts.push(
                Alert::new(
                    Severity::MonitorOnly,
                    format!(
                        "{} is running, so MostlyFans isn't controlling the graphics card fans. Your other fans are still controlled.",
                        state.gpus.conflicts.join(", ")
                    ),
                )
                .key(Key::GpuConflict),
            );
        }
        for status in &state.fans {
            let FanState::Faulted(reason) = &status.state else {
                continue;
            };
            let owner = match status.id.kind() {
                FanKind::Header => "the BIOS",
                FanKind::Gpu => "the graphics driver",
            };
            let name = crate::format::fan_name(&state.config, state, &status.id);
            let mut alert = Alert::new(
                Severity::Warning,
                format!("{name} was handed back to {owner}: {reason}"),
            );
            let blocked = match status.id.kind() {
                FanKind::Header => state.monitor_only,
                FanKind::Gpu => state.gpus.monitor_only,
            };
            if !blocked {
                alert = alert.action(Action::RetryFan(status.id.clone()));
            }
            alerts.push(alert);
        }
        for warning in &state.warnings {
            alerts.push(Alert::new(Severity::Warning, sentence(warning)));
        }
    }
    for (topic, text) in notices {
        let severity = if topic.dismissable() {
            Severity::Notice
        } else {
            Severity::Warning
        };
        let mut alert = Alert::new(severity, text.clone()).key(Key::Notice(*topic));
        if *topic == Topic::Autostart {
            alert = alert.action(Action::RepairAutostart);
        }
        alerts.push(alert);
    }
    alerts.sort_by_key(|alert| alert.severity);
    alerts
}

pub fn ongoing(
    status: &ControlStatus,
    state: Option<&ControlState>,
    notices: &[(Topic, String)],
) -> BTreeSet<Key> {
    let mut keys: BTreeSet<Key> = notices
        .iter()
        .map(|(topic, _)| Key::Notice(*topic))
        .collect();
    match state.filter(|_| matches!(status, ControlStatus::Running)) {
        Some(state) => {
            if !state.conflicts.is_empty() {
                keys.insert(Key::Conflict);
            }
            if !state.gpus.conflicts.is_empty() {
                keys.insert(Key::GpuConflict);
            }
        }
        None => keys.extend([Key::Conflict, Key::GpuConflict]),
    }
    keys
}

#[derive(Default)]
pub struct AlertArea {
    expanded: bool,
    dismissed: BTreeMap<Key, String>,
}

impl AlertArea {
    fn visible(&mut self, alerts: Vec<Alert>, ongoing: &BTreeSet<Key>) -> Vec<Alert> {
        self.dismissed.retain(|key, _| ongoing.contains(key));
        alerts
            .into_iter()
            .filter(|alert| {
                !(alert.dismissable()
                    && alert
                        .key
                        .and_then(|key| self.dismissed.get(&key))
                        .is_some_and(|text| text == &alert.text))
            })
            .collect()
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        alerts: Vec<Alert>,
        ongoing: &BTreeSet<Key>,
    ) -> Option<Action> {
        let alerts = self.visible(alerts, ongoing);
        if alerts.is_empty() {
            return None;
        }
        let safety = alerts
            .iter()
            .filter(|alert| alert.severity.is_safety())
            .count();
        let room = MAX_VISIBLE.saturating_sub(safety);
        let mut action = None;
        let mut hidden = 0;
        let mut others = 0;
        for alert in &alerts {
            if !alert.severity.is_safety() {
                others += 1;
                if others > room && !self.expanded {
                    hidden += 1;
                    continue;
                }
            }
            if let Some(chosen) = self.row(ui, alert) {
                action = Some(chosen);
            }
            ui.add_space(SPACE_S);
        }
        if hidden > 0 || (self.expanded && others > room) {
            let label = if self.expanded {
                "Show fewer".to_owned()
            } else {
                format!("+{hidden} more")
            };
            if ui.link(label).clicked() {
                self.expanded = !self.expanded;
            }
            ui.add_space(SPACE_S);
        }
        action
    }

    fn row(&mut self, ui: &mut egui::Ui, alert: &Alert) -> Option<Action> {
        let palette = theme::of(ui);
        let tone = alert.severity.tone();
        let mut action = None;
        egui::Frame::new()
            .fill(palette.fill(tone))
            .stroke(Stroke::new(1.0, palette.tone(tone).gamma_multiply(0.45)))
            .corner_radius(theme::CARD_RADIUS)
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::Sides::new().shrink_left().wrap().show(
                    ui,
                    |ui| {
                        let icon = if tone == Tone::Danger {
                            icons::ERROR
                        } else {
                            icons::WARNING
                        };
                        ui.label(RichText::new(icon).color(palette.tone(tone)));
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&alert.text).color(palette.text).strong());
                            if let Some(detail) = &alert.detail {
                                ui.label(widgets::caption(ui, detail));
                            }
                        });
                    },
                    |ui| {
                        if alert.dismissable()
                            && let Some(key) = alert.key
                            && ui
                                .small_button(icons::CLOSE)
                                .on_hover_text("Dismiss")
                                .clicked()
                        {
                            self.dismissed.insert(key, alert.text.clone());
                        }
                        if alert.busy {
                            ui.spinner();
                        } else if let Some(chosen) = &alert.action
                            && ui.button(chosen.label()).clicked()
                        {
                            action = Some(chosen.clone());
                        }
                    },
                );
            });
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn alerts_are_ordered_by_severity() {
        let mut state = fixtures::state();
        state.critical = true;
        state.monitor_only = true;
        state.warnings = vec!["another program has held the hardware bus".to_owned()];
        let notices = vec![(Topic::Autostart, "repair me".to_owned())];
        let alerts = collect(&ControlStatus::Running, Some(&state), &notices, None);
        let order: Vec<Severity> = alerts.iter().map(|alert| alert.severity).collect();
        assert_eq!(
            order,
            vec![
                Severity::Critical,
                Severity::MonitorOnly,
                Severity::Warning,
                Severity::Notice
            ]
        );
        assert_eq!(alerts[2].text, "Another program has held the hardware bus");
    }

    #[test]
    fn only_notices_and_tool_conflicts_can_be_dismissed() {
        let mut state = fixtures::state();
        state.critical = true;
        state.monitor_only = true;
        state.warnings = vec!["critical sensor `cpu/package` is not reporting".to_owned()];
        let notices = vec![
            (Topic::Guardian, "guardian down".to_owned()),
            (Topic::Autostart, "repair me".to_owned()),
        ];
        let alerts = collect(
            &ControlStatus::Stopped("panic".into()),
            Some(&state),
            &notices,
            Some(Alert::new(Severity::Driver, "driver")),
        );
        let dismissable: Vec<&str> = alerts
            .iter()
            .filter(|alert| alert.dismissable())
            .map(|alert| alert.text.as_str())
            .collect();
        assert_eq!(dismissable, vec!["repair me"]);
        assert!(
            alerts
                .iter()
                .find(|alert| alert.text == "guardian down")
                .is_some_and(|alert| alert.severity.is_safety())
        );

        let mut critical = Alert::new(Severity::Critical, "hot").key(Key::Conflict);
        assert!(!critical.dismissable());
        critical.severity = Severity::MonitorOnly;
        assert!(critical.dismissable());
    }

    #[test]
    fn monitor_only_alerts_without_a_tool_keep_no_close_button() {
        let dismissable = |state: &ControlState| {
            collect(&ControlStatus::Running, Some(state), &[], None)
                .iter()
                .any(Alert::dismissable)
        };

        let mut plain = fixtures::state();
        plain.monitor_only = true;
        assert!(!dismissable(&plain));

        let mut unsupported = plain.clone();
        unsupported.conflicts = vec!["FanControl".to_owned()];
        if let Some(board) = unsupported.board.as_mut() {
            board.fan_control = false;
        }
        assert!(!dismissable(&unsupported));

        let mut misconfigured = unsupported.clone();
        if let Some(board) = misconfigured.board.as_mut() {
            board.pwm_misconfigured = true;
        }
        assert!(!dismissable(&misconfigured));

        let mut chosen = plain.clone();
        chosen.conflicts = vec!["FanControl".to_owned()];
        chosen.user_monitor_only = true;
        assert!(!dismissable(&chosen));

        chosen.user_monitor_only = false;
        assert!(dismissable(&chosen));
    }

    #[test]
    fn a_stopped_controller_hides_the_stale_snapshot_alerts() {
        let mut state = fixtures::state();
        state.critical = true;
        let alerts = collect(
            &ControlStatus::Stopped("panic".into()),
            Some(&state),
            &[],
            None,
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].action, Some(Action::RestartControl));
    }

    #[test]
    fn a_dismissed_notice_returns_when_its_text_changes() {
        let mut area = AlertArea::default();
        area.dismissed
            .insert(Key::Notice(Topic::Autostart), "old text".to_owned());
        let ongoing = BTreeSet::from([Key::Notice(Topic::Autostart)]);
        let notice = |text: &str| {
            collect(
                &ControlStatus::Running,
                None,
                &[(Topic::Autostart, text.to_owned())],
                None,
            )
        };
        assert!(area.visible(notice("old text"), &ongoing).is_empty());
        assert_eq!(area.visible(notice("new text"), &ongoing).len(), 1);
    }

    #[test]
    fn conflict_alerts_stay_dismissed_until_the_tool_closes() {
        let mut state = fixtures::with_gpu(fixtures::state());
        state.monitor_only = true;
        state.conflicts = vec!["FanControl".to_owned()];
        state.gpus.monitor_only = true;
        state.gpus.conflicts = vec!["MSI Afterburner".to_owned()];
        let frame = |area: &mut AlertArea, status: &ControlStatus, state: &ControlState| {
            area.visible(
                collect(status, Some(state), &[], None),
                &ongoing(status, Some(state), &[]),
            )
        };

        let mut area = AlertArea::default();
        let shown = frame(&mut area, &ControlStatus::Running, &state);
        let dismissable: Vec<&Alert> = shown.iter().filter(|alert| alert.dismissable()).collect();
        assert_eq!(
            dismissable
                .iter()
                .filter_map(|alert| alert.key)
                .collect::<Vec<_>>(),
            vec![Key::Conflict, Key::GpuConflict]
        );
        for alert in dismissable {
            if let Some(key) = alert.key {
                area.dismissed.insert(key, alert.text.clone());
            }
        }
        assert!(frame(&mut area, &ControlStatus::Running, &state).is_empty());

        frame(&mut area, &ControlStatus::Stopped("panic".into()), &state);
        state.user_monitor_only = true;
        frame(&mut area, &ControlStatus::Running, &state);
        state.user_monitor_only = false;
        assert!(frame(&mut area, &ControlStatus::Running, &state).is_empty());

        let mut closed = state.clone();
        closed.conflicts.clear();
        closed.gpus.conflicts.clear();
        frame(&mut area, &ControlStatus::Running, &closed);
        assert_eq!(frame(&mut area, &ControlStatus::Running, &state).len(), 2);
    }

    #[test]
    fn unverified_chips_explain_why_fans_are_left_alone() {
        let mut state = fixtures::state();
        state.monitor_only = true;
        state.critical = true;
        if let Some(board) = state.board.as_mut() {
            board.chip_name = "Nuvoton NCT6798D".to_owned();
            board.fan_control = false;
        }
        let alerts = collect(&ControlStatus::Running, Some(&state), &[], None);
        let texts: Vec<&str> = alerts.iter().map(|alert| alert.text.as_str()).collect();
        assert!(texts.contains(&"Critical temperature: the fans are under BIOS control"));
        assert!(texts.contains(
            &"Monitor-only mode: MostlyFans doesn't support fan control on the Nuvoton NCT6798D, so it only reads temperatures and fan speeds and leaves the fans to the BIOS."
        ));
        assert!(!texts.iter().any(|text| text.contains("100%")));

        let mut conflict = fixtures::state();
        conflict.monitor_only = true;
        conflict.critical = true;
        let alerts = collect(&ControlStatus::Running, Some(&conflict), &[], None);
        assert_eq!(
            alerts[0].text,
            "Critical temperature: the fans are under BIOS control"
        );

        conflict.monitor_only = false;
        let alerts = collect(&ControlStatus::Running, Some(&conflict), &[], None);
        assert_eq!(alerts[0].text, "Critical temperature: every fan is at 100%");
    }

    #[test]
    fn a_failed_pwm_check_points_at_the_bios_instead_of_the_chip() {
        let mut state = fixtures::state();
        state.monitor_only = true;
        if let Some(board) = state.board.as_mut() {
            board.chip_name = "ITE IT8628E".to_owned();
            board.fan_control = false;
            board.pwm_misconfigured = true;
        }
        let alerts = collect(&ControlStatus::Running, Some(&state), &[], None);
        let alert = alerts
            .iter()
            .find(|alert| alert.severity == Severity::MonitorOnly)
            .unwrap();
        assert_eq!(
            alert.text,
            "Monitor-only mode: the BIOS set up the fan outputs of the ITE IT8628E with inverted polarity, a known BIOS fault that Linux it87 also won't drive, so MostlyFans only reads temperatures and fan speeds and leaves the fans to the BIOS."
        );
        assert!(alert.detail.as_deref().unwrap().contains("BIOS setup"));
        assert!(
            !alerts
                .iter()
                .any(|alert| alert.text.contains("doesn't support"))
        );

        state.user_monitor_only = true;
        let alerts = collect(&ControlStatus::Running, Some(&state), &[], None);
        let detail = alerts
            .iter()
            .find(|alert| alert.severity == Severity::MonitorOnly)
            .and_then(|alert| alert.detail.as_deref())
            .unwrap();
        assert!(
            detail.starts_with("The BIOS left the fan outputs of the ITE IT8628E misconfigured")
        );
    }
}
