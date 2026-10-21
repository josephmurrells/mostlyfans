use std::collections::BTreeMap;

use eframe::egui::{self, Margin, RichText, Stroke};
use fanforge_core::state::{ControlState, HardwareStatus};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    RestartControl,
    InstallPawnIo,
    RepairAutostart,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::RestartControl => "Restart control",
            Self::InstallPawnIo => "Install PawnIO",
            Self::RepairAutostart => "Repair autostart",
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
    pub topic: Option<Topic>,
}

impl Alert {
    pub fn new(severity: Severity, text: impl Into<String>) -> Self {
        Self {
            severity,
            text: text.into(),
            detail: None,
            action: None,
            busy: false,
            topic: None,
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

    pub fn busy(mut self) -> Self {
        self.busy = true;
        self
    }

    pub fn dismissable(&self) -> bool {
        !self.severity.is_safety() && self.topic.is_some_and(Topic::dismissable)
    }
}

fn sentence(text: &str) -> String {
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
        if state.critical {
            let text = if state.monitor_only && unverified.is_some() {
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
        if state.monitor_only {
            let text = if let Some(board) = unverified {
                format!(
                    "Monitor-only mode: FanForge doesn't support fan control on the {}, so it only reads temperatures and fan speeds and leaves the fans to the BIOS.",
                    board.chip_name
                )
            } else if state.conflicts.is_empty() {
                "Monitor-only mode: the fans are left to the BIOS.".to_owned()
            } else {
                format!(
                    "Monitor-only mode: {} is running, so the fans are left to the BIOS.",
                    state.conflicts.join(", ")
                )
            };
            alerts.push(Alert::new(Severity::MonitorOnly, text));
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
        let mut alert = Alert::new(severity, text.clone());
        alert.topic = Some(*topic);
        if *topic == Topic::Autostart {
            alert.action = Some(Action::RepairAutostart);
        }
        alerts.push(alert);
    }
    alerts.sort_by_key(|alert| alert.severity);
    alerts
}

#[derive(Default)]
pub struct AlertArea {
    expanded: bool,
    dismissed: BTreeMap<Topic, String>,
}

impl AlertArea {
    fn visible(&self, alerts: Vec<Alert>) -> Vec<Alert> {
        alerts
            .into_iter()
            .filter(|alert| {
                !(alert.dismissable()
                    && alert
                        .topic
                        .and_then(|topic| self.dismissed.get(&topic))
                        .is_some_and(|text| text == &alert.text))
            })
            .collect()
    }

    pub fn show(&mut self, ui: &mut egui::Ui, alerts: Vec<Alert>) -> Option<Action> {
        let alerts = self.visible(alerts);
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
                            && let Some(topic) = alert.topic
                            && ui
                                .small_button(icons::CLOSE)
                                .on_hover_text("Dismiss")
                                .clicked()
                        {
                            self.dismissed.insert(topic, alert.text.clone());
                        }
                        if alert.busy {
                            ui.spinner();
                        } else if let Some(chosen) = alert.action
                            && ui.button(chosen.label()).clicked()
                        {
                            action = Some(chosen);
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
    fn safety_alerts_can_never_be_dismissed() {
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
            .insert(Topic::Autostart, "old text".to_owned());
        let notice = |text: &str| {
            collect(
                &ControlStatus::Running,
                None,
                &[(Topic::Autostart, text.to_owned())],
                None,
            )
        };
        assert!(area.visible(notice("old text")).is_empty());
        assert_eq!(area.visible(notice("new text")).len(), 1);
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
            &"Monitor-only mode: FanForge doesn't support fan control on the Nuvoton NCT6798D, so it only reads temperatures and fan speeds and leaves the fans to the BIOS."
        ));
        assert!(!texts.iter().any(|text| text.contains("100%")));

        let mut conflict = fixtures::state();
        conflict.monitor_only = true;
        conflict.critical = true;
        let alerts = collect(&ControlStatus::Running, Some(&conflict), &[], None);
        assert_eq!(alerts[0].text, "Critical temperature: every fan is at 100%");
    }
}
