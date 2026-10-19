use serde::{Deserialize, Serialize};

const KEY: &str = "fanforge-ui";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    #[default]
    Overview,
    Fans,
    Curves,
    Settings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub page: Page,
    pub setup_skipped: bool,
}

impl Prefs {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        storage
            .and_then(|storage| eframe::get_value(storage, KEY))
            .unwrap_or_default()
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY, self);
    }
}
