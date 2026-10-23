use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;

use eframe::egui::Vec2;
use serde::Deserialize;

pub const MIN_SIZE: Vec2 = Vec2::new(720.0, 520.0);
const WINDOW_KEY: &str = "window";

#[derive(Deserialize)]
struct Settings {
    normal_geometry: Geometry,
}

#[derive(Deserialize)]
struct Geometry {
    inner_size_points: Vec2,
}

fn collapsed(window: &str) -> bool {
    ron::from_str::<Settings>(window).is_ok_and(|settings| {
        let size = settings.normal_geometry.inner_size_points;
        size.x < MIN_SIZE.x / 2.0 || size.y < MIN_SIZE.y / 2.0
    })
}

pub fn repair(path: &Path) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let Ok(mut entries) = ron::from_str::<HashMap<String, String>>(&text) else {
        return;
    };
    if !entries
        .get(WINDOW_KEY)
        .is_some_and(|window| collapsed(window))
    {
        return;
    }
    entries.remove(WINDOW_KEY);
    match rewrite(path, &entries) {
        Ok(()) => tracing::info!("discarded a window size saved while minimized"),
        Err(error) => tracing::warn!(%error, "could not discard the saved window size"),
    }
}

fn rewrite(path: &Path, entries: &HashMap<String, String>) -> Result<(), Box<dyn Error>> {
    let text = ron::ser::to_string_pretty(entries, ron::ser::PrettyConfig::default())?;
    let staging = path.with_extension("ron.tmp");
    fs::write(&staging, text)?;
    fs::rename(&staging, path)?;
    Ok(())
}

#[derive(Default)]
pub struct LastGoodWindow(Option<String>);

impl LastGoodWindow {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        Self(
            storage
                .and_then(|storage| storage.get_string(WINDOW_KEY))
                .filter(|window| !collapsed(window)),
        )
    }

    pub fn guard(&mut self, storage: &mut dyn eframe::Storage) {
        match storage.get_string(WINDOW_KEY) {
            Some(window) if collapsed(&window) => {
                if let Some(good) = &self.0 {
                    storage.set_string(WINDOW_KEY, good.clone());
                }
            }
            Some(window) => self.0 = Some(window),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use eframe::Storage as _;

    use super::*;

    const PREFS: &str = r#""mostlyfans-ui": "(page:Overview,setup_skipped:false,pinned_sensors:None,pinned_fans:None)""#;

    fn window(width: f32, height: f32) -> String {
        format!(
            "(normal_geometry:(outer_position_pixels:Some((x:0.0,y:0.0)),scale_factor:1.0,inner_size_points:(x:{width:.1},y:{height:.1})),fullscreen:false,maximized:false)"
        )
    }

    fn file(width: f32, height: f32) -> String {
        format!(r#"{{{PREFS},"window": "{}"}}"#, window(width, height))
    }

    fn run(contents: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window.ron");
        fs::write(&path, contents).unwrap();
        repair(&path);
        fs::read_to_string(&path).unwrap()
    }

    #[derive(Default)]
    struct Memory(HashMap<String, String>);

    impl eframe::Storage for Memory {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }

        fn flush(&mut self) {}
    }

    #[test]
    fn a_window_saved_while_minimized_is_forgotten_and_prefs_are_kept() {
        let after: HashMap<String, String> = ron::from_str(&run(&file(64.0, 64.0))).unwrap();
        assert!(!after.contains_key(WINDOW_KEY));
        assert!(after["mostlyfans-ui"].contains("page:Overview"));
    }

    #[test]
    fn normal_and_small_but_usable_windows_are_left_untouched() {
        for (width, height) in [(1080.0, 720.0), (600.0, 433.0), (480.0, 347.0)] {
            let before = file(width, height);
            assert_eq!(run(&before), before);
        }
    }

    #[test]
    fn files_without_a_readable_window_entry_are_left_alone() {
        for contents in [
            "not ron",
            r#"{"window": "something else"}"#,
            &format!("{{{PREFS}}}"),
        ] {
            assert_eq!(run(contents), contents);
        }
    }

    #[test]
    fn a_minimized_save_puts_the_last_good_window_back() {
        let mut storage = Memory::default();
        storage.set_string(WINDOW_KEY, window(1000.0, 700.0));
        let mut last = LastGoodWindow::load(Some(&storage));
        storage.set_string(WINDOW_KEY, window(0.0, 0.0));
        last.guard(&mut storage);
        assert_eq!(storage.get_string(WINDOW_KEY), Some(window(1000.0, 700.0)));

        storage.set_string(WINDOW_KEY, window(900.0, 600.0));
        last.guard(&mut storage);
        storage.set_string(WINDOW_KEY, window(0.0, 0.0));
        last.guard(&mut storage);
        assert_eq!(storage.get_string(WINDOW_KEY), Some(window(900.0, 600.0)));
    }
}
