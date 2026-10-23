use std::collections::BTreeSet;
use std::path::Path;

use mostlyfans_core::FanId;
use mostlyfans_core::store;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ownership {
    #[serde(default)]
    pub fans: BTreeSet<FanId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loaded {
    Owned(Ownership),
    Lost,
}

impl Loaded {
    pub fn owns(&self, fan: &FanId) -> bool {
        match self {
            Self::Owned(ownership) => ownership.fans.contains(fan),
            Self::Lost => true,
        }
    }
}

pub fn load(path: &Path) -> Loaded {
    match store::load_toml::<Ownership>(path) {
        Ok(ownership) => Loaded::Owned(ownership.unwrap_or_default()),
        Err(error) => {
            tracing::error!(%error, "GPU fan ownership file is unreadable");
            Loaded::Lost
        }
    }
}

pub fn save(path: &Path, ownership: &Ownership) -> Result<(), String> {
    store::save_toml_atomic(path, ownership, 0).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_round_trips_and_a_broken_file_counts_as_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpu_owned.toml");
        assert_eq!(load(&path), Loaded::Owned(Ownership::default()));
        let ownership = Ownership {
            fans: [FanId::from("gpu/nvidia/gpu-1/fan1")].into(),
        };
        save(&path, &ownership).unwrap();
        let loaded = load(&path);
        assert!(loaded.owns(&"gpu/nvidia/gpu-1/fan1".into()));
        assert!(!loaded.owns(&"gpu/nvidia/gpu-1/fan2".into()));
        std::fs::write(&path, "not toml [").unwrap();
        assert_eq!(load(&path), Loaded::Lost);
        assert!(load(&path).owns(&"gpu/nvidia/gpu-1/fan2".into()));
    }
}
