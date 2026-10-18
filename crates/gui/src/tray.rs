use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

use eframe::egui::{self, ViewportCommand};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

const ICON_SIZE: u32 = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    SelectProfile(String),
}

pub struct Tray {
    icon: TrayIcon,
    menu: Menu,
    show_id: MenuId,
    profiles: Vec<(MenuId, CheckMenuItem, String)>,
    profile_names: Vec<String>,
    events: Receiver<MenuEvent>,
    exit_requested: Arc<AtomicBool>,
    tooltip: String,
}

impl Tray {
    pub fn new(ctx: &egui::Context) -> Result<Self, Box<dyn std::error::Error>> {
        let menu = Menu::new();
        let show = MenuItem::new("Open FanForge", true, None);
        let exit = MenuItem::new("Exit window", true, None);
        menu.append_items(&[&show, &PredefinedMenuItem::separator(), &exit])?;

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
            .with_icon(fan_icon()?)
            .build()?;
        Ok(Self {
            icon,
            menu,
            show_id,
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
            } else if let Some((_, _, name)) =
                self.profiles.iter().find(|(id, _, _)| *id == event.id)
            {
                actions.push(TrayAction::SelectProfile(name.clone()));
            }
        }
        actions
    }

    pub fn sync(&mut self, profiles: &[String], active: &str, tooltip: &str) {
        if self.profile_names != profiles {
            for (_, item, _) in self.profiles.drain(..) {
                if let Err(error) = self.menu.remove(&item) {
                    tracing::warn!(%error, "failed to remove tray menu item");
                }
            }
            for (position, name) in profiles.iter().enumerate() {
                let item = CheckMenuItem::new(name, true, name == active, None);
                if let Err(error) = self.menu.insert(&item, position + 1) {
                    tracing::warn!(%error, "failed to add tray menu item");
                }
                self.profiles.push((item.id().clone(), item, name.clone()));
            }
            self.profile_names = profiles.to_vec();
        }
        for (_, item, name) in &self.profiles {
            item.set_checked(name == active);
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

fn fan_icon() -> Result<Icon, tray_icon::BadIcon> {
    let size = ICON_SIZE as f32;
    let center = size / 2.0;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let radius = (dx * dx + dy * dy).sqrt();
            let angle = dy.atan2(dx);
            let blade = (angle * 3.0).sin() > 0.2;
            let pixel = if radius < 3.5 {
                [240, 240, 240, 255]
            } else if radius < center - 1.0 && blade {
                [64, 156, 255, 255]
            } else if radius < center - 1.0 {
                [30, 40, 56, 255]
            } else {
                [0, 0, 0, 0]
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE)
}
