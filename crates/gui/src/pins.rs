use fanforge_core::state::ControlState;
use fanforge_core::{FanId, SensorId};

use crate::overview;
use crate::prefs::Prefs;

pub fn automatic_sensors(state: &ControlState) -> Vec<SensorId> {
    overview::ordered_sensors(state)
        .into_iter()
        .take(overview::TILE_COUNT)
        .map(|sensor| sensor.id.clone())
        .collect()
}

pub fn automatic_fans(state: &ControlState) -> Vec<FanId> {
    overview::ordered_fans(state)
        .into_iter()
        .map(|fan| fan.id.clone())
        .collect()
}

pub fn sensors(prefs: &Prefs, state: &ControlState) -> Vec<SensorId> {
    prefs
        .pinned_sensors
        .clone()
        .unwrap_or_else(|| automatic_sensors(state))
}

pub fn fans(prefs: &Prefs, state: &ControlState) -> Vec<FanId> {
    prefs
        .pinned_fans
        .clone()
        .unwrap_or_else(|| automatic_fans(state))
}

fn set<T: PartialEq + Clone>(
    list: &mut Option<Vec<T>>,
    automatic: impl FnOnce() -> Vec<T>,
    id: &T,
    pinned: bool,
) {
    let list = list.get_or_insert_with(automatic);
    let present = list.contains(id);
    if pinned && !present {
        list.push(id.clone());
    } else if !pinned && present {
        list.retain(|item| item != id);
    }
}

pub fn set_sensor(prefs: &mut Prefs, state: &ControlState, id: &SensorId, pinned: bool) {
    set(
        &mut prefs.pinned_sensors,
        || automatic_sensors(state),
        id,
        pinned,
    );
}

pub fn set_fan(prefs: &mut Prefs, state: &ControlState, id: &FanId, pinned: bool) {
    set(&mut prefs.pinned_fans, || automatic_fans(state), id, pinned);
}

pub fn reset(prefs: &mut Prefs) {
    prefs.pinned_sensors = None;
    prefs.pinned_fans = None;
}

pub fn customised(prefs: &Prefs) -> bool {
    prefs.pinned_sensors.is_some() || prefs.pinned_fans.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn nothing_pinned_follows_the_automatic_overview() {
        let state = fixtures::state();
        let prefs = Prefs::default();
        assert_eq!(sensors(&prefs, &state), automatic_sensors(&state));
        assert_eq!(fans(&prefs, &state), automatic_fans(&state));
        assert!(!customised(&prefs));
    }

    #[test]
    fn the_first_unpin_freezes_the_automatic_list() {
        let state = fixtures::state();
        let mut prefs = Prefs::default();
        let automatic = automatic_sensors(&state);
        set_sensor(&mut prefs, &state, &automatic[0], false);
        assert_eq!(prefs.pinned_sensors.as_deref(), Some(&automatic[1..]));
        assert_eq!(prefs.pinned_fans, None);
    }

    #[test]
    fn pins_append_once_and_unpins_remove() {
        let state = fixtures::state();
        let mut prefs = Prefs::default();
        let core = SensorId::core(0);
        set_sensor(&mut prefs, &state, &core, true);
        set_sensor(&mut prefs, &state, &core, true);
        let pinned = prefs.pinned_sensors.clone().unwrap();
        assert_eq!(pinned.last(), Some(&core));
        assert_eq!(pinned.iter().filter(|id| **id == core).count(), 1);
        set_sensor(&mut prefs, &state, &core, false);
        assert!(!prefs.pinned_sensors.unwrap().contains(&core));
    }

    #[test]
    fn fans_pin_separately_and_reset_restores_automatic() {
        let state = fixtures::state();
        let mut prefs = Prefs::default();
        set_fan(&mut prefs, &state, &fixtures::fan(1), false);
        assert!(!fans(&prefs, &state).contains(&fixtures::fan(1)));
        assert_eq!(prefs.pinned_sensors, None);
        reset(&mut prefs);
        assert_eq!(prefs, Prefs::default());
    }
}
