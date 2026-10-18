use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use fanforge_core::FanId;
use fanforge_core::store::{self, StoreError};
use serde::{Deserialize, Serialize};

use crate::ite::ChannelState;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiosSnapshot {
    #[serde(default)]
    pub channels: BTreeMap<FanId, ChannelState>,
    #[serde(default)]
    pub owned: BTreeSet<FanId>,
}

impl BiosSnapshot {
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        Ok(store::load_toml(path)?.unwrap_or_default())
    }

    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        store::save_toml_atomic(path, self, 0)
    }

    pub fn channel(&self, index: usize) -> Option<ChannelState> {
        self.channels.get(&FanId::for_channel(index)).copied()
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
    }
}
