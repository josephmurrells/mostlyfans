use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use mostlyfans_core::FanId;
use mostlyfans_core::store::{self, StoreError};
use serde::{Deserialize, Serialize};

use crate::ite::IT8655E_ID;
use crate::superio::ChannelState;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiosSnapshot {
    #[serde(
        default = "default_chip_id",
        skip_serializing_if = "is_default_chip_id"
    )]
    pub chip_id: u16,
    #[serde(default)]
    pub channels: BTreeMap<FanId, ChannelState>,
    #[serde(default)]
    pub owned: BTreeSet<FanId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smartfan: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smartfan_route: Option<SmartfanRoute>,
    #[serde(skip)]
    pub lost: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartfanRoute {
    Smfi,
    Ecio,
}

fn default_chip_id() -> u16 {
    IT8655E_ID
}

fn is_default_chip_id(chip_id: &u16) -> bool {
    *chip_id == IT8655E_ID
}

impl Default for BiosSnapshot {
    fn default() -> Self {
        Self::for_chip(IT8655E_ID)
    }
}

impl BiosSnapshot {
    pub fn for_chip(chip_id: u16) -> Self {
        Self {
            chip_id,
            channels: BTreeMap::new(),
            owned: BTreeSet::new(),
            smartfan: None,
            smartfan_route: None,
            lost: false,
        }
    }

    pub fn load(path: &Path) -> Result<Self, StoreError> {
        Ok(store::load_toml(path)?.unwrap_or_default())
    }

    pub fn load_for(path: &Path, chip_id: u16) -> Result<Self, StoreError> {
        Ok(store::load_toml(path)?.unwrap_or_else(|| Self::for_chip(chip_id)))
    }

    pub fn belongs_to(&self, chip_id: u16) -> bool {
        self.chip_id == chip_id || self.owned.is_empty()
    }

    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        store::save_toml_atomic(path, self, 0)
    }

    pub fn channel(&self, index: usize) -> Option<ChannelState> {
        self.channels.get(&FanId::for_channel(index)).copied()
    }

    pub fn lost() -> Self {
        Self {
            lost: true,
            ..Self::default()
        }
    }

    pub fn is_owned(&self, index: usize) -> bool {
        self.owned.contains(&FanId::for_channel(index))
    }

    pub fn refresh(&mut self, states: &[ChannelState]) -> bool {
        let mut changed = false;
        for (index, state) in states.iter().enumerate() {
            if state.is_auto() && !self.is_owned(index) {
                let previous = self.channels.insert(FanId::for_channel(index), *state);
                changed |= previous != Some(*state);
            }
        }
        changed
    }

    pub fn claim(&mut self, index: usize, current: ChannelState) -> bool {
        if self.is_owned(index) {
            return false;
        }
        if current.is_auto() {
            self.channels.insert(FanId::for_channel(index), current);
        }
        self.owned.insert(FanId::for_channel(index))
    }

    pub fn claim_exact(&mut self, index: usize, current: ChannelState) -> bool {
        if self.is_owned(index) {
            return false;
        }
        self.channels.insert(FanId::for_channel(index), current);
        self.owned.insert(FanId::for_channel(index))
    }

    pub fn release(&mut self, index: usize) -> bool {
        self.owned.remove(&FanId::for_channel(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(control: u8, duty: u8) -> ChannelState {
        ChannelState {
            control,
            duty,
            main_enabled: true,
            ..ChannelState::default()
        }
    }

    #[test]
    fn refresh_tracks_bios_changes_for_channels_we_do_not_own() {
        let mut snapshot = BiosSnapshot::default();
        assert!(snapshot.refresh(&[state(0x80, 0x30), state(0x01, 0x10)]));
        assert_eq!(snapshot.channel(0), Some(state(0x80, 0x30)));
        assert_eq!(snapshot.channel(1), None);
        assert!(!snapshot.refresh(&[state(0x80, 0x30), state(0x01, 0x10)]));
        assert!(snapshot.refresh(&[state(0x80, 0x44), state(0x01, 0x10)]));
        assert_eq!(snapshot.channel(0), Some(state(0x80, 0x44)));
    }

    #[test]
    fn owned_channels_keep_their_pre_control_snapshot() {
        let mut snapshot = BiosSnapshot::default();
        assert!(snapshot.claim(0, state(0x80, 0x30)));
        assert!(!snapshot.claim(0, state(0x00, 0x99)));
        assert!(!snapshot.refresh(&[state(0x80, 0x77)]));
        assert_eq!(snapshot.channel(0), Some(state(0x80, 0x30)));
        assert!(snapshot.release(0));
        assert!(snapshot.refresh(&[state(0x80, 0x77)]));
        assert_eq!(snapshot.channel(0), Some(state(0x80, 0x77)));
    }

    #[test]
    fn claiming_a_manual_channel_does_not_record_it_as_bios_state() {
        let mut snapshot = BiosSnapshot::default();
        assert!(snapshot.claim(2, state(0x02, 0x50)));
        assert_eq!(snapshot.channel(2), None);
        assert!(snapshot.is_owned(2));
    }

    #[test]
    fn persists_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bios_snapshot.toml");
        let mut snapshot = BiosSnapshot::default();
        snapshot.refresh(&[state(0x80, 0x30), state(0x01, 0x10)]);
        snapshot.claim(0, state(0x80, 0x30));
        snapshot.save(&path).unwrap();
        assert_eq!(BiosSnapshot::load(&path).unwrap(), snapshot);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("chip_id"));
        assert!(!std::fs::read_to_string(&path).unwrap().contains("kind"));
    }

    #[test]
    fn other_chips_record_their_id_and_control_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bios_snapshot.toml");
        let mut snapshot = BiosSnapshot::for_chip(0xD42B);
        let nuvoton = ChannelState {
            control: 0x50,
            duty: 0x80,
            main_enabled: true,
            kind: crate::superio::ControlKind::Nuvoton,
        };
        assert!(nuvoton.is_auto());
        snapshot.refresh(&[nuvoton]);
        snapshot.save(&path).unwrap();
        let loaded = BiosSnapshot::load(&path).unwrap();
        assert_eq!(loaded, snapshot);
        assert_eq!(loaded.chip_id, 0xD42B);
        assert_eq!(BiosSnapshot::default().chip_id, IT8655E_ID);
    }
}
