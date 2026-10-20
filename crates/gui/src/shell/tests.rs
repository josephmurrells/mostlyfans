use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{Theme, Vec2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use fanforge_core::Duty;
use fanforge_core::calibration::{
    CalibrationOutcome, CalibrationPoint, CalibrationReport, FanCalibration,
};
use fanforge_core::config::FanMode;
use fanforge_core::state::ControlState;

use super::*;
use crate::autostart::AutostartState;
use crate::fixtures::{self, FakeLink};
use crate::notices::Topic;

const SIZE: Vec2 = vec2(1080.0, 720.0);

fn shell(link: &FakeLink, page: Page) -> Shell {
    Shell::new(
        Box::new(link.clone()),
        Notices::default(),
        AutostartPanel::new(AutostartState::Off),
        PawnIoPanel::new(Arc::new(|| {})),
        Prefs {
            page,
            setup_skipped: false,
        },
    )
}

fn harness_with(shell: Shell, size: Vec2, theme: Theme) -> Harness<'static, Shell> {
    let mut harness = Harness::builder()
        .with_size(size)
        .with_theme(theme)
        .with_context_setup(theme::install)
        .build_ui_state(
            |ui, shell: &mut Shell| {
                shell.logic();
                shell.ui(ui);
            },
            shell,
        );
    harness.run_steps(4);
    harness
}

fn harness(link: &FakeLink, page: Page) -> Harness<'static, Shell> {
    harness_with(shell(link, page), SIZE, Theme::Dark)
}

fn settle(harness: &mut Harness<'_, Shell>) {
    harness.run_steps(6);
}

fn click(harness: &mut Harness<'_, Shell>, label: &str) {
    harness.get_by_label(label).scroll_to_me();
    settle(harness);
    harness.get_by_label(label).click();
    settle(harness);
}

fn make_dirty_with(harness: &mut Harness<'_, Shell>, mode: &str) {
    click(harness, mode);
    assert!(harness.query_by_label("Apply").is_some());
}

fn make_dirty(harness: &mut Harness<'_, Shell>) {
    make_dirty_with(harness, "BIOS");
}

fn active_mode(link: &FakeLink, index: usize) -> FanMode {
    link.live_config().mode(&fixtures::fan(index))
}

#[test]
fn the_sidebar_switches_pages() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Overview);
    for (label, page) in [
        ("Fans", Page::Fans),
        ("Curves", Page::Curves),
        ("Settings", Page::Settings),
        ("Overview", Page::Overview),
    ] {
        harness.get_by_role_and_label(Role::Tab, label).click();
        settle(&mut harness);
        assert_eq!(harness.state().prefs.page, page);
    }
}

#[test]
fn the_unsaved_bar_appears_and_applies() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    assert!(harness.query_by_label("Apply").is_none());
    make_dirty(&mut harness);
    assert!(
        harness
            .query_by_label_contains("1 unsaved change")
            .is_some()
    );
    harness.get_by_label("Apply").click();
    settle(&mut harness);
    assert_eq!(link.sent(), vec!["config:Balanced"]);
    assert_eq!(active_mode(&link, 0), FanMode::Auto);
    assert!(harness.query_by_label("Apply").is_none());
    assert_eq!(harness.state().toasts.texts(), vec!["Changes applied"]);
}

#[test]
fn ctrl_s_applies_and_revert_discards() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    make_dirty(&mut harness);
    harness.get_by_label("Revert").click();
    settle(&mut harness);
    assert!(harness.query_by_label("Apply").is_none());
    assert!(link.sent().is_empty());

    make_dirty(&mut harness);
    harness.key_press_modifiers(Modifiers::COMMAND, Key::S);
    settle(&mut harness);
    assert_eq!(link.sent(), vec!["config:Balanced"]);
}

#[test]
fn undoing_an_edit_hides_the_unsaved_bar() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    make_dirty(&mut harness);
    harness.get_by_label("Curve").click();
    settle(&mut harness);
    assert!(harness.query_by_label("Apply").is_none());
}

#[test]
fn a_clean_profile_switch_applies_at_once_and_survives_the_next_apply() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    assert!(!harness.state_mut().request_profile("Silent".to_owned()));
    settle(&mut harness);
    assert_eq!(link.sent(), vec!["profile:Silent"]);
    make_dirty_with(&mut harness, "Fixed");
    harness.get_by_label("Apply").click();
    settle(&mut harness);
    assert_eq!(link.sent(), vec!["profile:Silent", "config:Silent"]);
}

#[test]
fn a_dirty_profile_switch_asks_first() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    make_dirty(&mut harness);
    assert!(harness.state_mut().request_profile("Silent".to_owned()));
    settle(&mut harness);
    assert!(link.sent().is_empty());
    harness.get_by_label("Cancel").click();
    settle(&mut harness);
    assert!(harness.query_by_label("Discard and switch").is_none());
    assert!(link.sent().is_empty());

    harness.state_mut().request_profile("Silent".to_owned());
    settle(&mut harness);
    harness.get_by_label("Discard and switch").click();
    settle(&mut harness);
    assert_eq!(link.sent(), vec!["profile:Silent"]);
    assert_eq!(active_mode(&link, 0), FanMode::Auto);
    assert!(harness.query_by_label("Apply").is_none());
}

#[test]
fn apply_and_switch_sends_the_edits_with_the_new_profile() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    make_dirty(&mut harness);
    harness
        .state_mut()
        .request_profile("Performance".to_owned());
    settle(&mut harness);
    harness.get_by_label("Apply and switch").click();
    settle(&mut harness);
    assert_eq!(link.sent(), vec!["config:Performance"]);
    assert_eq!(link.live_config().active_profile, "Performance");
}

#[test]
fn the_likely_cpu_header_needs_confirmation_to_become_a_case_fan() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    click(&mut harness, "Case fan");
    assert!(harness.query_by_label("Apply").is_none());
    click(&mut harness, "Cancel");
    assert!(harness.query_by_label("Yes, it is a case fan").is_none());

    click(&mut harness, "Case fan");
    click(&mut harness, "Yes, it is a case fan");
    harness.get_by_label("Apply").click();
    settle(&mut harness);
    assert!(link.live_config().settings(&fixtures::fan(0)).case_fan);
}

#[test]
fn a_cpu_fan_or_pump_can_never_be_allowed_to_stop() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    click(&mut harness, "Advanced");
    let checkbox = harness.get_by_label("Allow 0 RPM when the curve asks for 0%");
    assert!(checkbox.accesskit_node().is_disabled());
}

#[test]
fn safety_alerts_have_no_dismiss_button() {
    let link = FakeLink::new(fixtures::state());
    link.update(|state| {
        state.warnings = vec!["critical sensor `cpu/tctl` is not reporting".to_owned()];
        state.monitor_only = true;
    });
    let shell = shell(&link, Page::Overview);
    shell
        .notices
        .set(Topic::Guardian, "The guardian is not running");
    let mut harness = harness_with(shell, SIZE, Theme::Dark);
    assert!(
        harness
            .query_by_label_contains("is not reporting")
            .is_some()
    );
    assert!(harness.query_by_label(icons::CLOSE).is_none());

    harness
        .state()
        .notices
        .set(Topic::Autostart, "The autostart task is wrong");
    settle(&mut harness);
    assert!(
        harness
            .query_by_label("The autostart task is wrong")
            .is_none()
    );
    harness.get_by_label("+1 more").click();
    settle(&mut harness);
    harness.get_by_label(icons::CLOSE).click();
    settle(&mut harness);
    assert!(
        harness
            .query_by_label("The autostart task is wrong")
            .is_none()
    );
    assert!(
        harness
            .query_by_label_contains("is not reporting")
            .is_some()
    );
}

#[test]
fn identify_is_disabled_in_monitor_only_mode() {
    let link = FakeLink::new(fixtures::state());
    let mut running = harness(&link, Page::Fans);
    let label = widgets::with_icon(icons::IDENTIFY, "Identify");
    click(&mut running, &label);
    assert_eq!(link.sent(), vec!["identify:pwm1"]);
    assert!(running.query_by_label("Spinning…").is_some());

    let link = FakeLink::new(fixtures::state());
    link.update(|state| state.monitor_only = true);
    let monitor = harness(&link, Page::Fans);
    assert!(monitor.get_by_label(&label).accesskit_node().is_disabled());
}

#[test]
fn opening_a_fan_card_shows_that_fan() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Overview);
    harness.get_by_role_and_label(Role::Button, "Fan 2").click();
    settle(&mut harness);
    assert_eq!(harness.state().prefs.page, Page::Fans);
    assert!(
        harness
            .query_by_label_contains("Fan setup · 1 of 1")
            .is_some()
    );
}

#[test]
fn edits_are_locked_while_an_apply_is_in_flight() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    make_dirty(&mut harness);
    link.0.borrow_mut().hold = true;
    harness.get_by_label("Apply").click();
    settle(&mut harness);
    assert!(harness.query_by_label("Applying…").is_some());
    assert!(harness.get_by_label("Fixed").accesskit_node().is_disabled());
    link.release();
    settle(&mut harness);
    assert_eq!(harness.state().toasts.texts(), vec!["Changes applied"]);
    assert!(!harness.get_by_label("Fixed").accesskit_node().is_disabled());
}

#[test]
fn a_profile_switch_during_an_apply_waits_for_it() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    make_dirty(&mut harness);
    link.0.borrow_mut().hold = true;
    harness.get_by_label("Apply").click();
    settle(&mut harness);
    assert!(!harness.state_mut().request_profile("Silent".to_owned()));
    settle(&mut harness);
    assert!(harness.query_by_label("Discard and switch").is_none());
    link.release();
    settle(&mut harness);
    assert_eq!(link.sent(), vec!["config:Balanced", "profile:Silent"]);
    assert_eq!(link.live_config().active_profile, "Silent");
}

#[test]
fn a_rejected_apply_keeps_the_edits() {
    let link = FakeLink::new(fixtures::state());
    link.0.borrow_mut().reject_config = Some("invalid safety settings".to_owned());
    let mut harness = harness(&link, Page::Fans);
    make_dirty(&mut harness);
    harness.get_by_label("Apply").click();
    settle(&mut harness);
    assert!(harness.query_by_label("Apply").is_some());
    assert_eq!(
        harness.state().toasts.texts(),
        vec!["Your changes were rejected: invalid safety settings"]
    );
    assert_eq!(
        active_mode(&link, 0),
        fixtures::config().mode(&fixtures::fan(0))
    );
}

#[test]
fn the_status_dropdown_switches_profiles() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Overview);
    harness.get_by_label("Active profile").click();
    settle(&mut harness);
    harness.get_by_label("Performance").click();
    settle(&mut harness);
    assert_eq!(link.sent(), vec!["profile:Performance"]);
}

#[test]
fn choosing_the_active_profile_does_nothing() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    make_dirty(&mut harness);
    assert!(!harness.state_mut().request_profile("Balanced".to_owned()));
    settle(&mut harness);
    assert!(harness.query_by_label("Discard and switch").is_none());
    assert!(link.sent().is_empty());
}

#[test]
fn opening_a_page_never_rounds_or_clamps_values() {
    let link = FakeLink::new(fixtures::state());
    link.update(|state| {
        state.config.safety.release_c = 84.5;
        let active = state.config.active_profile.clone();
        state.config.profiles.get_mut(&active).unwrap().fans.insert(
            fixtures::fan(0),
            FanMode::Manual {
                duty: Duty::new(37.5),
            },
        );
        state
            .config
            .fans
            .entry(fixtures::fan(0))
            .or_default()
            .min_duty = Duty::new(12.3);
    });
    for page in [Page::Fans, Page::Settings] {
        let mut harness = harness(&link, page);
        if page == Page::Fans {
            click(&mut harness, "Advanced");
        }
        assert!(harness.query_by_label("Apply").is_none(), "{page:?}");
    }
}

fn screenshot(harness: &mut Harness<'_, Shell>, dir: &std::path::Path, name: &str) {
    let image = harness.render().expect("render");
    image.save(dir.join(format!("{name}.png"))).expect("save");
}

fn busy_state() -> ControlState {
    let mut state = fixtures::state();
    state.warnings = vec!["another program has held the hardware bus for a long time".into()];
    state
}

#[test]
#[ignore = "renders screenshots for manual review; set FANFORGE_SCREENSHOTS"]
fn render_screenshots() {
    let Some(dir) = std::env::var_os("FANFORGE_SCREENSHOTS").map(PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for (theme, suffix) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
        for (page, name) in [
            (Page::Overview, "overview"),
            (Page::Fans, "fans"),
            (Page::Curves, "curves"),
            (Page::Settings, "settings"),
        ] {
            let link = FakeLink::new(fixtures::state());
            let mut harness = harness_with(shell(&link, page), SIZE, theme);
            for _ in 0..3 {
                link.update(|state| {
                    for sensor in &mut state.readings.sensors {
                        sensor.celsius = sensor.celsius.map(|value| value + 0.7);
                    }
                });
                harness.run_steps(2);
            }
            screenshot(&mut harness, &dir, &format!("{name}-{suffix}"));
        }
    }
    let link = FakeLink::new(busy_state());
    let mut harness = harness_with(shell(&link, Page::Fans), SIZE, Theme::Dark);
    make_dirty(&mut harness);
    screenshot(&mut harness, &dir, "dirty-dark");
    harness.state_mut().request_profile("Silent".to_owned());
    settle(&mut harness);
    screenshot(&mut harness, &dir, "prompt-dark");

    let link = FakeLink::new(fixtures::state());
    link.0.borrow_mut().status = ControlStatus::Stopped("the control thread panicked".into());
    let mut harness = harness_with(shell(&link, Page::Overview), SIZE, Theme::Dark);
    screenshot(&mut harness, &dir, "stopped-dark");

    let link = FakeLink::new(fixtures::state());
    let mut harness = harness_with(shell(&link, Page::Fans), vec2(760.0, 600.0), Theme::Light);
    screenshot(&mut harness, &dir, "compact-light");
}

fn calibrated(stop: f32) -> FanCalibration {
    FanCalibration {
        stop_duty: Some(Duty::new(stop)),
        start_duty: None,
        lowest_tested: Duty::new(stop),
        points: vec![CalibrationPoint {
            duty: Duty::FULL,
            rpm: 1800,
        }],
    }
}

#[test]
fn calibration_starts_shows_progress_and_cancels() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    click(&mut harness, "Calibrate");
    assert_eq!(link.sent(), vec!["calibrate:pwm1"]);
    assert!(
        harness
            .query_by_label_contains("Stepping down · 100% · 1500 rpm")
            .is_some()
    );
    let identify = widgets::with_icon(icons::IDENTIFY, "Identify");
    assert!(
        harness
            .get_by_label(&identify)
            .accesskit_node()
            .is_disabled()
    );
    click(&mut harness, "Cancel calibration");
    assert_eq!(link.sent(), vec!["calibrate:pwm1", "cancel-calibration"]);
    assert!(harness.query_by_label("Calibration cancelled").is_some());
}

#[test]
fn calibration_is_disabled_in_monitor_only_and_when_stopped() {
    let link = FakeLink::new(fixtures::state());
    link.update(|state| state.monitor_only = true);
    let monitor = harness(&link, Page::Fans);
    assert!(
        monitor
            .get_by_label("Calibrate")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        monitor
            .query_by_label_contains("Calibration is unavailable: Monitor-only mode")
            .is_some()
    );

    let link = FakeLink::new(fixtures::state());
    link.0.borrow_mut().status = ControlStatus::Stopped("stopped".into());
    let stopped = harness(&link, Page::Fans);
    assert!(
        stopped
            .get_by_label("Calibrate")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn calibration_results_and_abort_reasons_are_shown() {
    let link = FakeLink::new(fixtures::state());
    link.update(|state| {
        state
            .config
            .calibration
            .insert(fixtures::fan(0), calibrated(35.0));
        state.calibration.last = Some(CalibrationReport {
            fan: fixtures::fan(0),
            outcome: CalibrationOutcome::Aborted("Board temp 1 rose by 5.5 °C".to_owned()),
        });
    });
    let harness = harness(&link, Page::Fans);
    assert!(harness.query_by_label("Stops at 35%").is_some());
    assert!(
        harness
            .query_by_label("Calibration stopped: Board temp 1 rose by 5.5 °C")
            .is_some()
    );
    assert!(
        harness
            .query_all_by_label_contains("Floor 45% (calibrated)")
            .next()
            .is_some()
    );
}

#[test]
fn a_calibration_landing_during_an_apply_still_confirms_it() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    make_dirty(&mut harness);
    link.0.borrow_mut().hold = true;
    harness.get_by_label("Apply").click();
    settle(&mut harness);
    link.release();
    link.update(|state| {
        state
            .config
            .calibration
            .insert(fixtures::fan(1), calibrated(30.0));
    });
    settle(&mut harness);
    assert_eq!(harness.state().toasts.texts(), vec!["Changes applied"]);
    assert!(harness.query_by_label("Apply").is_none());
}
