use std::collections::{BTreeMap, BTreeSet};

use fanforge_core::config::Config;

fn keyed<K: Ord, V: PartialEq>(live: &BTreeMap<K, V>, draft: &BTreeMap<K, V>) -> usize {
    let keys: BTreeSet<&K> = live.keys().chain(draft.keys()).collect();
    keys.into_iter()
        .filter(|key| live.get(key) != draft.get(key))
        .count()
}

pub fn count(live: &Config, draft: &Config) -> usize {
    let mut changes = 0;
    changes += usize::from(live.interval_ms != draft.interval_ms);
    changes += usize::from(live.active_profile != draft.active_profile);
    changes += usize::from(live.safety != draft.safety);
    changes += keyed(&live.fans, &draft.fans);
    let profiles: BTreeSet<&String> = live.profiles.keys().chain(draft.profiles.keys()).collect();
    for name in profiles {
        changes += match (live.profiles.get(name), draft.profiles.get(name)) {
            (Some(live), Some(draft)) => keyed(&live.fans, &draft.fans),
            _ => 1,
        };
    }
    let curves: BTreeSet<_> = live
        .curves
        .iter()
        .chain(draft.curves.iter())
        .map(|curve| &curve.id)
        .collect();
    for id in curves {
        let find = |config: &Config| config.curves.iter().find(|curve| &curve.id == id).cloned();
        changes += usize::from(find(live) != find(draft));
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use fanforge_core::Duty;
    use fanforge_core::config::{FanMode, Profile};

    #[test]
    fn an_unchanged_draft_has_no_changes() {
        let config = fixtures::config();
        assert_eq!(count(&config, &config.clone()), 0);
    }

    #[test]
    fn every_kind_of_edit_counts_once() {
        let live = fixtures::config();
        let mut draft = live.clone();
        draft.interval_ms = 500;
        draft.safety.critical_c = 90.0;
        draft.fans.entry(fixtures::fan(1)).or_default().case_fan = true;
        draft.curves[0].name = "Renamed".to_owned();
        let active = draft.active_profile.clone();
        draft.profiles.get_mut(&active).unwrap().fans.insert(
            fixtures::fan(2),
            FanMode::Manual {
                duty: Duty::new(60.0),
            },
        );
        draft
            .profiles
            .insert("Gaming".to_owned(), Profile::default());
        assert_eq!(count(&live, &draft), 6);
    }

    #[test]
    fn removing_a_profile_or_curve_counts() {
        let live = fixtures::config();
        let mut draft = live.clone();
        draft.profiles.remove("Silent");
        draft.curves.clear();
        assert_eq!(count(&live, &draft), 2);
    }
}
