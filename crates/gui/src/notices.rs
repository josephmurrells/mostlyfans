use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Topic {
    Guardian,
    GuardianJob,
    Autostart,
}

#[derive(Debug, Clone, Default)]
pub struct Notices {
    entries: Arc<Mutex<BTreeMap<Topic, String>>>,
}

impl Notices {
    fn entries(&self) -> MutexGuard<'_, BTreeMap<Topic, String>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set(&self, topic: Topic, text: impl Into<String>) {
        self.entries().insert(topic, text.into());
    }

    pub fn clear(&self, topic: Topic) {
        self.entries().remove(&topic);
    }

    pub fn list(&self) -> Vec<(Topic, String)> {
        self.entries()
            .iter()
            .map(|(topic, text)| (*topic, text.clone()))
            .collect()
    }
}

impl Topic {
    pub fn dismissable(self) -> bool {
        matches!(self, Self::Autostart)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_are_replaced_per_topic() {
        let notices = Notices::default();
        notices.set(Topic::Guardian, "first");
        notices.set(Topic::Autostart, "path");
        notices.set(Topic::Guardian, "second");
        assert_eq!(
            notices.list(),
            vec![
                (Topic::Guardian, "second".to_owned()),
                (Topic::Autostart, "path".to_owned())
            ]
        );
        notices.clear(Topic::Guardian);
        assert_eq!(notices.list(), vec![(Topic::Autostart, "path".to_owned())]);
    }

    #[test]
    fn only_the_autostart_notice_can_be_dismissed() {
        assert!(Topic::Autostart.dismissable());
        assert!(!Topic::Guardian.dismissable());
        assert!(!Topic::GuardianJob.dismissable());
    }
}
