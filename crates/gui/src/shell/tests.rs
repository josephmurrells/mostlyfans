use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{Theme, Vec2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use fanforge_core::Duty;
use fanforge_core::SensorId;
use fanforge_core::calibration::{
    CalibrationOutcome, CalibrationPoint, CalibrationReport, FanCalibration,
};
use fanforge_core::config::FanMode;
use fanforge_core::state::ControlState;
use fanforge_core::telemetry::SensorReading;

use super::*;
use crate::autostart::AutostartState;
use crate::fixtures::{self, FakeLink};
use crate::notices::Topic;
use crate::overview::unnamed_fans;

const SIZE: Vec2 = vec2(1080.0, 720.0);

fn shell(link: &FakeLink, page: Page) -> Shell {
    Shell::new(
        Box::new(link.clone()),
        Notices::default(),
        AutostartPanel::new(AutostartState::Off),
        PawnIoPanel::new(Arc::new(|| {})),
        Prefs {
            page,
            ..Prefs::default()
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
        ("Temperatures", Page::Temperatures),
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
        state.warnings = vec!["critical sensor `cpu/package` is not reporting".to_owned()];
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
            (Page::Temperatures, "temperatures"),
            (Page::Fans, "fans"),
            (Page::Curves, "curves"),
            (Page::Settings, "settings"),
        ] {
            let link = FakeLink::new(with_cores(fixtures::state()));
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

    let link = FakeLink::new(fixtures::with_second_chip(fixtures::state()));
    let mut view = shell(&link, Page::Fans);
    view.fans.select(fixtures::second_chip_fan(0));
    let mut harness = harness_with(view, SIZE, Theme::Dark);
    screenshot(&mut harness, &dir, "fans-second-chip-dark");

    let link = FakeLink::new(fixtures::state());
    link.0.borrow_mut().status = ControlStatus::Stopped("the control thread panicked".into());
    let mut harness = harness_with(shell(&link, Page::Overview), SIZE, Theme::Dark);
    screenshot(&mut harness, &dir, "stopped-dark");

    for width in [760.0, 1080.0] {
        let link = FakeLink::new(fixtures::state());
        let mut harness = harness_with(
            shell(&link, Page::Settings),
            vec2(width, 1500.0),
            Theme::Dark,
        );
        screenshot(&mut harness, &dir, &format!("settings-full-{width}-dark"));
    }

    let link = FakeLink::new(fixtures::state());
    let mut harness = harness_with(shell(&link, Page::Fans), vec2(760.0, 600.0), Theme::Light);
    screenshot(&mut harness, &dir, "compact-light");

    for (theme, suffix) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
        let link = FakeLink::new(fixtures::with_gpu(fixtures::state()));
        let mut view = shell(&link, Page::Fans);
        view.fans.select(fixtures::gpu_fan(0));
        let mut harness = harness_with(view, SIZE, theme);
        screenshot(&mut harness, &dir, &format!("fans-gpu-{suffix}"));
    }

    for width in [760.0, 980.0, 1200.0] {
        let mut state = fixtures::with_gpu(fixtures::state());
        let active = state.config.active_profile.clone();
        if let Some(profile) = state.config.profiles.get_mut(&active) {
            profile.fans.insert(
                fixtures::gpu_fan(1),
                FanMode::Sync {
                    fan: fixtures::gpu_fan(0),
                },
            );
        }
        let link = FakeLink::new(state);
        let mut harness = harness_with(
            shell(&link, Page::Overview),
            vec2(width, 1100.0),
            Theme::Dark,
        );
        screenshot(&mut harness, &dir, &format!("overview-gpu-{width}"));
    }

    let mut paused = fixtures::state();
    paused.user_monitor_only = true;
    paused.monitor_only = true;
    let link = FakeLink::new(paused);
    let mut harness = harness_with(shell(&link, Page::Settings), SIZE, Theme::Dark);
    screenshot(&mut harness, &dir, "settings-monitor-only-dark");

    let mut flagged = fixtures::state();
    flagged.fans[1].state =
        fanforge_core::engine::FanState::Faulted(fanforge_core::response::NO_RESPONSE.to_owned());
    let link = FakeLink::new(flagged);
    let mut view = shell(&link, Page::Fans);
    view.fans.select(fixtures::fan(1));
    let mut harness = harness_with(view, SIZE, Theme::Light);
    screenshot(&mut harness, &dir, "fans-flagged-light");

    let link = FakeLink::new(with_cores(fixtures::state()));
    let mut pinned = shell(&link, Page::Overview);
    pinned.prefs.pinned_sensors = Some(vec![
        SensorId::new("cpu/package"),
        SensorId::core(0),
        SensorId::core(1),
        SensorId::new("drive/gone"),
    ]);
    pinned.prefs.pinned_fans = Some(vec![fixtures::fan(0), fixtures::fan(1)]);
    let mut harness = harness_with(pinned, SIZE, Theme::Dark);
    screenshot(&mut harness, &dir, "overview-pinned-dark");
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
        not_restarting: false,
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
fn a_gpu_fan_shows_driver_control_without_header_settings() {
    let link = FakeLink::new(fixtures::with_gpu(fixtures::state()));
    let mut view = shell(&link, Page::Fans);
    view.fans.select(fixtures::gpu_fan(0));
    let harness = harness_with(view, SIZE, Theme::Dark);
    assert!(harness.query_by_label("Graphics card").is_some());
    assert!(harness.query_by_label("Driver").is_some());
    assert!(harness.query_by_label("Header type").is_none());
    assert!(harness.query_by_label("Calibrate").is_none());
    assert!(
        harness
            .query_by_label_contains("minimum 30% (driver)")
            .is_some()
    );
}

#[test]
fn second_chip_fans_are_grouped_and_only_monitored() {
    let link = FakeLink::new(fixtures::with_second_chip(fixtures::state()));
    let mut view = shell(&link, Page::Fans);
    view.fans.select(fixtures::second_chip_fan(1));
    let mut harness = harness_with(view, SIZE, Theme::Dark);
    assert!(
        harness
            .query_by_label("Motherboard (second chip)")
            .is_some()
    );
    assert!(harness.query_by_label("Motherboard").is_some());
    assert!(
        harness
            .query_all_by_label_contains("Fan 2 (IT8792E)")
            .next()
            .is_some()
    );
    assert!(
        harness
            .query_all_by_label_contains("only controls the second fan chip on Gigabyte boards")
            .count()
            >= 2
    );
    let plain = harness_with(
        shell(&FakeLink::new(fixtures::state()), Page::Fans),
        SIZE,
        Theme::Dark,
    );
    for text in ["not set up", "No fan connected"] {
        assert_eq!(
            harness.query_all_by_label_contains(text).count(),
            plain.query_all_by_label_contains(text).count(),
            "{text}"
        );
    }
    let identify = widgets::with_icon(icons::IDENTIFY, "Identify");
    assert!(harness.query_by_label(&identify).is_none());
    assert!(harness.query_by_label("Calibrate").is_none());
    assert!(harness.query_by_label("Header type").is_none());
    assert!(harness.query_by_label("Fixed").is_none());
    harness.run();
    assert!(link.sent().is_empty());
}

#[test]
fn second_chip_fans_are_never_offered_to_follow_or_to_set_up() {
    let state = fixtures::with_second_chip(fixtures::state());
    assert!(
        unnamed_fans(&state, &state.config)
            .iter()
            .all(|fan| fan.header_channel().is_some_and(|(chip, _)| chip == 0))
    );
    let candidates = crate::fans::follow_candidates(&state.config, &state, &fixtures::fan(0));
    assert!(!candidates.is_empty());
    assert!(
        candidates
            .iter()
            .all(|(fan, _)| fan.header_channel().is_some_and(|(chip, _)| chip == 0))
    );
}

#[test]
fn settings_lists_the_second_fan_controller() {
    let link = FakeLink::new(fixtures::with_second_chip(fixtures::state()));
    let harness = harness(&link, Page::Settings);
    assert!(harness.query_by_label("Second fan controller").is_some());
    assert!(
        harness
            .query_by_label("ITE IT8792E (0x8733) · 3 fan headers · monitor only")
            .is_some()
    );
}

#[test]
fn a_gpu_tool_is_named_in_its_own_alert() {
    let link = FakeLink::new(fixtures::with_gpu(fixtures::state()));
    link.update(|state| {
        state.gpus.monitor_only = true;
        state.gpus.conflicts = vec!["MSI Afterburner".to_owned()];
    });
    let harness = harness(&link, Page::Overview);
    assert!(
        harness
            .query_by_label_contains(
                "MSI Afterburner is running, so FanForge isn't controlling the graphics card fans"
            )
            .is_some()
    );
    assert!(
        harness
            .query_by_label_contains("Monitor-only mode")
            .is_none()
    );
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
            outcome: CalibrationOutcome::Aborted("Board temp 1 reached 75 °C".to_owned()),
        });
    });
    let harness = harness(&link, Page::Fans);
    assert!(harness.query_by_label("Stops at 35%").is_some());
    assert!(
        harness
            .query_by_label("Calibration stopped: Board temp 1 reached 75 °C")
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

#[test]
fn renaming_a_fan_keeps_focus_when_the_unsaved_bar_appears() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .unwrap()
        .focus();
    settle(&mut harness);
    for letter in ["C", "P", "U"] {
        harness
            .get_all_by_role(Role::TextInput)
            .next()
            .unwrap()
            .type_text(letter);
        settle(&mut harness);
    }
    assert!(harness.query_by_label("Apply").is_some());
    let draft = harness.state().draft.as_ref().unwrap();
    assert_eq!(
        draft.settings(&fixtures::fan(0)).name.as_deref(),
        Some("CPU coolerCPU")
    );
}

#[test]
fn renaming_a_curve_keeps_focus_when_the_unsaved_bar_appears() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Curves);
    let before = harness
        .state()
        .draft
        .clone()
        .unwrap_or_else(|| link.live_config());
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .unwrap()
        .focus();
    settle(&mut harness);
    for letter in ["x", "y", "z"] {
        harness
            .get_all_by_role(Role::TextInput)
            .next()
            .unwrap()
            .type_text(letter);
        settle(&mut harness);
    }
    assert!(harness.query_by_label("Apply").is_some());
    let draft = harness.state().draft.as_ref().unwrap();
    assert_eq!(
        draft.curves[0].name,
        format!("{}xyz", before.curves[0].name)
    );
}

#[test]
fn typing_in_a_number_field_keeps_focus_when_the_unsaved_bar_appears() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Settings);
    harness.get_by_label("for at least").scroll_to_me();
    settle(&mut harness);
    harness.get_by_label("for at least").click();
    settle(&mut harness);
    harness.key_press_modifiers(Modifiers::COMMAND, Key::A);
    settle(&mut harness);
    for digit in ["2", "5"] {
        harness.get_by_label("for at least").type_text(digit);
        settle(&mut harness);
    }
    harness.key_press(Key::Enter);
    settle(&mut harness);
    assert!(harness.query_by_label("Apply").is_some());
    let draft = harness.state().draft.as_ref().unwrap();
    assert_eq!(draft.safety.release_hold_secs, 25.0);
}

#[test]
fn the_safety_toggles_default_on_and_disable_their_controls_when_off() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Settings);
    let override_label = "Critical temperature override: all fans to 100% when it gets too hot";
    let fallback_label =
        "Missing-sensor fallback: fans go to 100% when their sensor stops reporting";
    assert!(harness.query_by_label(fallback_label).is_some());
    assert!(
        !harness
            .get_by_role_and_label(Role::Slider, "Full speed at")
            .accesskit_node()
            .is_disabled()
    );
    click(&mut harness, override_label);
    assert!(harness.query_by_label("Apply").is_some());
    let draft = harness.state().draft.as_ref().unwrap();
    assert!(!draft.safety.critical_enabled);
    assert!(draft.safety.sensor_fallback_enabled);
    assert!(
        harness
            .get_by_role_and_label(Role::Slider, "Full speed at")
            .accesskit_node()
            .is_disabled()
    );
    let disabled = |harness: &Harness<'_, Shell>, role: Role, label: &str| {
        harness
            .get_by_role_and_label(role, label)
            .accesskit_node()
            .is_disabled()
    };
    assert!(!disabled(&harness, Role::Slider, "Back to normal below"));
    assert!(!disabled(&harness, Role::SpinButton, "for at least"));
    assert!(!disabled(
        &harness,
        Role::SpinButton,
        "Missing sensor readings"
    ));
    click(&mut harness, fallback_label);
    assert!(disabled(&harness, Role::Slider, "Back to normal below"));
    assert!(disabled(&harness, Role::SpinButton, "for at least"));
    assert!(!disabled(
        &harness,
        Role::SpinButton,
        "Missing sensor readings"
    ));
}

#[test]
fn turned_off_safety_shows_warnings_and_heat_still_raises_a_danger_alert() {
    let link = FakeLink::new(fixtures::state());
    link.update(|state| {
        state.config.safety.critical_enabled = false;
        state.config.safety.sensor_fallback_enabled = false;
        state.critical_reached = true;
        state.readings.sensors[0].celsius = Some(92.0);
    });
    let harness = harness(&link, Page::Overview);
    assert!(
        harness
            .query_all_by_label_contains("The critical temperature override is off")
            .next()
            .is_some()
    );
    assert!(
        harness
            .query_all_by_label_contains("The missing-sensor fallback is off")
            .next()
            .is_some()
    );
    assert!(
        harness
            .query_all_by_label_contains("reached the critical temperature (92 °C now)")
            .next()
            .is_some()
    );
    assert!(harness.query_by_label("Critical (override off)").is_some());
    assert!(harness.query_by_label(icons::CLOSE).is_none());
}

fn with_cores(mut state: ControlState) -> ControlState {
    for (index, celsius) in [41.1, 41.5, 39.9, 39.3].into_iter().enumerate() {
        state.readings.sensors.push(SensorReading {
            id: SensorId::core(index),
            label: format!("Core{index} (CCD1)"),
            celsius: Some(celsius),
        });
    }
    state
}

#[test]
fn the_temperatures_page_groups_sensors_and_pins_them() {
    let link = FakeLink::new(with_cores(fixtures::state()));
    let mut harness = harness(&link, Page::Temperatures);
    for heading in ["CPU", "Motherboard", "Core2 (CCD1)"] {
        assert!(harness.query_by_label(heading).is_some(), "{heading}");
    }
    assert!(harness.query_by_label("Display only").is_none());
    let pin = widgets::with_icon(icons::PIN, "Pin");
    harness.query_all_by_label(&pin).next().unwrap().click();
    settle(&mut harness);
    let pinned = harness.state().prefs.pinned_sensors.clone().unwrap();
    assert_eq!(pinned.last(), Some(&SensorId::core(0)));
    assert!(harness.state_mut().take_prefs_changed());

    harness.get_by_label("Reset Overview to automatic").click();
    settle(&mut harness);
    assert_eq!(harness.state().prefs.pinned_sensors, None);
}

#[test]
fn per_core_sensors_stay_off_the_automatic_overview() {
    let link = FakeLink::new(with_cores(fixtures::state()));
    let harness = harness(&link, Page::Overview);
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Core0 (CCD1)")
            .is_none()
    );
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Board temp 1")
            .is_some()
    );
}

#[test]
fn right_click_unpins_tiles_and_fan_cards() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Overview);
    let unpin = widgets::with_icon(icons::UNPIN, "Unpin from Overview");

    harness
        .get_by_role_and_label(Role::Button, "Board temp 1")
        .click_secondary();
    settle(&mut harness);
    harness.get_by_label(&unpin).click();
    settle(&mut harness);
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Board temp 1")
            .is_none()
    );
    assert_eq!(harness.state().prefs.page, Page::Overview);

    harness
        .get_by_role_and_label(Role::Button, "Fan 2")
        .click_secondary();
    settle(&mut harness);
    harness.get_by_label(&unpin).click();
    settle(&mut harness);
    assert!(
        harness
            .query_by_role_and_label(Role::Button, "Fan 2")
            .is_none()
    );
    assert_eq!(
        harness.state().prefs.pinned_fans.as_ref().map(Vec::len),
        Some(fixtures::state().readings.fans.len() - 1)
    );
}

#[test]
fn alerts_stay_with_nothing_pinned() {
    let link = FakeLink::new(fixtures::state());
    link.update(|state| {
        state.warnings = vec!["critical sensor `cpu/package` is not reporting".to_owned()];
    });
    let mut shell = shell(&link, Page::Overview);
    shell.prefs.pinned_sensors = Some(Vec::new());
    shell.prefs.pinned_fans = Some(Vec::new());
    let harness = harness_with(shell, SIZE, Theme::Dark);
    assert!(harness.query_by_label("No temperatures pinned.").is_some());
    assert!(harness.query_by_label("No fans pinned.").is_some());
    assert!(
        harness
            .query_by_label_contains("is not reporting")
            .is_some()
    );
}

#[test]
fn a_pinned_sensor_that_never_reported_waits_instead_of_going_missing() {
    let link = FakeLink::new(fixtures::state());
    let mut shell = shell(&link, Page::Overview);
    shell.prefs.pinned_sensors = Some(vec![SensorId::new("drive/gone")]);
    let harness = harness_with(shell, SIZE, Theme::Dark);
    assert!(harness.query_by_label("waiting for a reading").is_some());
    assert!(harness.query_by_label("Missing").is_none());
}

#[test]
fn fans_pin_from_their_detail_pane() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Fans);
    click(
        &mut harness,
        &widgets::with_icon(icons::UNPIN, "On Overview"),
    );
    let pinned = harness.state().prefs.pinned_fans.clone().unwrap();
    assert!(!pinned.contains(&fixtures::fan(0)));
    click(
        &mut harness,
        &widgets::with_icon(icons::PIN, "Pin to Overview"),
    );
    let pinned = harness.state().prefs.pinned_fans.clone().unwrap();
    assert_eq!(pinned.last(), Some(&fixtures::fan(0)));
}

#[test]
fn a_pinned_sensor_goes_missing_once_hardware_has_settled() {
    let link = FakeLink::new(fixtures::state());
    let mut shell = shell(&link, Page::Overview);
    shell.prefs.pinned_sensors = Some(vec![
        SensorId::new("cpu/package"),
        SensorId::new("drive/gone"),
    ]);
    let mut harness = harness_with(shell, SIZE, Theme::Dark);
    harness.state_mut().history.mark_settled(Instant::now());
    settle(&mut harness);
    assert!(harness.query_by_label("Missing").is_some());
    click(&mut harness, &widgets::with_icon(icons::UNPIN, "Unpin"));
    assert_eq!(
        harness.state().prefs.pinned_sensors,
        Some(vec![SensorId::new("cpu/package")])
    );
    assert!(harness.query_by_label("Missing").is_none());
}

#[test]
fn the_monitor_only_switch_applies_at_once_and_resumes_from_its_alert() {
    let link = FakeLink::new(fixtures::state());
    let mut harness = harness(&link, Page::Settings);
    click(
        &mut harness,
        "Monitor only: leave all fans to the BIOS and graphics driver",
    );
    assert_eq!(link.sent(), vec!["monitor-only:true"]);
    assert!(harness.query_by_label("Apply").is_none());
    click(&mut harness, "Resume control");
    assert_eq!(link.sent(), vec!["monitor-only:true", "monitor-only:false"]);
}

#[test]
fn a_flagged_fan_can_be_tried_again_from_its_alert_and_its_page() {
    let mut state = fixtures::state();
    state.fans[1].state = fanforge_core::engine::FanState::Faulted("the fan stopped at 30%".into());
    let link = FakeLink::new(state);
    let mut view = shell(&link, Page::Fans);
    view.fans.select(fixtures::fan(1));
    let mut harness = harness_with(view, SIZE, Theme::Dark);
    assert!(
        harness
            .query_by_label_contains("was handed back to the BIOS: the fan stopped at 30%")
            .is_some()
    );
    assert_eq!(harness.query_all_by_label("Try again").count(), 2);
    harness
        .query_all_by_label("Try again")
        .next()
        .unwrap()
        .click();
    settle(&mut harness);
    assert_eq!(link.sent(), vec![format!("retry:{}", fixtures::fan(1))]);
    assert!(harness.query_by_label("Try again").is_none());
}
