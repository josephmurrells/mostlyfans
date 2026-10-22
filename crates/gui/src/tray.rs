use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

use eframe::egui::{self, ViewportCommand};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::glyph;

const ICON_SIZE: u32 = 32;
const PROFILE_OFFSET: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    SelectProfile(String),
    MonitorOnly(bool),
}

pub struct Tray {
    icon: TrayIcon,
    menu: Menu,
    status: MenuItem,
    tint: [u8; 3],
    show_id: MenuId,
    monitor_only: CheckMenuItem,
    profiles: Vec<(MenuId, CheckMenuItem, String)>,
    profile_names: Vec<String>,
    events: Receiver<MenuEvent>,
    exit_requested: Arc<AtomicBool>,
    tooltip: String,
}

impl Tray {
    pub fn new(ctx: &egui::Context) -> Result<Self, Box<dyn std::error::Error>> {
        let menu = Menu::new();
        let status = MenuItem::new("Starting…", false, None);
        let show = MenuItem::new("Open FanForge", true, None);
        let monitor_only = CheckMenuItem::new("Monitor only", true, false, None);
        let exit = MenuItem::new("Quit FanForge", true, None);
        menu.append_items(&[
            &status,
            &PredefinedMenuItem::separator(),
            &show,
            &PredefinedMenuItem::separator(),
            &monitor_only,
            &PredefinedMenuItem::separator(),
            &exit,
        ])?;

        let (menu_tx, events) = mpsc::channel();
        let menu_ctx = ctx.clone();
        let show_id = show.id().clone();
        let show_for_handler = show_id.clone();
        let exit_id = exit.id().clone();
        let exit_requested = Arc::new(AtomicBool::new(false));
        let exit_flag = Arc::clone(&exit_requested);
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == exit_id {
                exit_flag.store(true, Ordering::SeqCst);
                menu_ctx.send_viewport_cmd(ViewportCommand::Close);
            } else if event.id == show_for_handler {
                reveal(&menu_ctx);
            }
            menu_tx.send(event).ok();
            menu_ctx.request_repaint();
        }));

        let click_ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                reveal(&click_ctx);
            }
        }));

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("FanForge")
            .with_icon(fan_icon(glyph::BLUE)?)
            .build()?;
        Ok(Self {
            icon,
            menu,
            status,
            tint: glyph::BLUE,
            show_id,
            monitor_only,
            profiles: Vec::new(),
            profile_names: Vec::new(),
            events,
            exit_requested,
            tooltip: "FanForge".to_owned(),
        })
    }

    pub fn exit_requested(&self) -> bool {
        self.exit_requested.load(Ordering::SeqCst)
    }

    pub fn actions(&self) -> Vec<TrayAction> {
        let mut actions = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if event.id == self.show_id {
                actions.push(TrayAction::Show);
            } else if event.id == *self.monitor_only.id() {
                actions.push(TrayAction::MonitorOnly(self.monitor_only.is_checked()));
            } else if let Some((_, _, name)) =
                self.profiles.iter().find(|(id, _, _)| *id == event.id)
            {
                actions.push(TrayAction::SelectProfile(name.clone()));
            }
        }
        actions
    }

    pub fn sync(
        &mut self,
        profiles: &[String],
        active: &str,
        status: &str,
        tooltip: &str,
        tint: [u8; 3],
        monitor_only: bool,
    ) {
        if self.monitor_only.is_checked() != monitor_only {
            self.monitor_only.set_checked(monitor_only);
        }
        if self.profile_names != profiles {
            for (_, item, _) in self.profiles.drain(..) {
                if let Err(error) = self.menu.remove(&item) {
                    tracing::warn!(%error, "failed to remove tray menu item");
                }
            }
            for (position, name) in profiles.iter().enumerate() {
                let item = CheckMenuItem::new(name, true, name == active, None);
                if let Err(error) = self.menu.insert(&item, position + PROFILE_OFFSET) {
                    tracing::warn!(%error, "failed to add tray menu item");
                }
                self.profiles.push((item.id().clone(), item, name.clone()));
            }
            self.profile_names = profiles.to_vec();
        }
        for (_, item, name) in &self.profiles {
            item.set_checked(name == active);
        }
        if self.status.text() != status {
            self.status.set_text(status);
        }
        if self.tint != tint {
            match fan_icon(tint) {
                Ok(icon) => {
                    if let Err(error) = self.icon.set_icon(Some(icon)) {
                        tracing::warn!(%error, "failed to update the tray icon");
                    }
                }
                Err(error) => tracing::warn!(%error, "failed to draw the tray icon"),
            }
            self.tint = tint;
        }
        if self.tooltip != tooltip {
            if let Err(error) = self.icon.set_tooltip(Some(tooltip)) {
                tracing::warn!(%error, "failed to update tray tooltip");
            }
            self.tooltip = tooltip.to_owned();
        }
    }
}

pub fn reveal(ctx: &egui::Context) {
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
    ctx.request_repaint();
}

fn fan_icon(tint: [u8; 3]) -> Result<Icon, tray_icon::BadIcon> {
    Icon::from_rgba(glyph::fan_rgba(ICON_SIZE, tint), ICON_SIZE, ICON_SIZE)
}
