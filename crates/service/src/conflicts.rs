use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_secs(5);

const CONTROLLING_TOOLS: [(&str, &str); 6] = [
    ("fancontrol.exe", "FanControl"),
    ("speedfan.exe", "SpeedFan"),
    ("argusmonitor.exe", "Argus Monitor"),
    ("asusfancontrolservice.exe", "ASUS Fan Xpert"),
    ("aisuite3.exe", "ASUS AI Suite 3"),
    ("armourycrate.service.exe", "ASUS Armoury Crate"),
];

const MONITORING_TOOLS: [(&str, &str); 2] = [
    ("librehardwaremonitor.exe", "LibreHardwareMonitor"),
    ("openhardwaremonitor.exe", "OpenHardwareMonitor"),
];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Findings {
    pub controlling: Vec<String>,
    pub monitoring: Vec<String>,
}

pub fn classify<S: AsRef<str>>(processes: &[S]) -> Findings {
    let matches = |table: &[(&str, &str)]| {
        let mut found: Vec<String> = table
            .iter()
            .filter(|(exe, _)| {
                processes
                    .iter()
                    .any(|name| name.as_ref().eq_ignore_ascii_case(exe))
            })
            .map(|(_, label)| (*label).to_owned())
            .collect();
        found.dedup();
        found
    };
    Findings {
        controlling: matches(&CONTROLLING_TOOLS),
        monitoring: matches(&MONITORING_TOOLS),
    }
}

pub type ProcessSource = Box<dyn FnMut() -> Result<Vec<String>, String> + Send>;

pub struct ConflictMonitor {
    source: ProcessSource,
    last_poll: Option<Instant>,
    findings: Findings,
}

impl Default for ConflictMonitor {
    fn default() -> Self {
        Self::with_source(Box::new(|| {
            fanforge_win::running_process_names().map_err(|error| error.to_string())
        }))
    }
}

impl ConflictMonitor {
    pub fn with_source(source: ProcessSource) -> Self {
        Self {
            source,
            last_poll: None,
            findings: Findings::default(),
        }
    }

    pub fn poll(&mut self) -> &Findings {
        let due = self
            .last_poll
            .is_none_or(|last| last.elapsed() >= POLL_INTERVAL);
        if due {
            self.last_poll = Some(Instant::now());
            match (self.source)() {
                Ok(names) => {
                    let findings = classify(&names);
                    if findings != self.findings {
                        tracing::info!(?findings, "fan tool conflicts changed");
                    }
                    self.findings = findings;
                }
                Err(error) => tracing::warn!(%error, "failed to enumerate processes"),
            }
        }
        &self.findings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_case_insensitively() {
        let findings = classify(&[
            "explorer.exe",
            "FanControl.exe",
            "LibreHardwareMonitor.exe",
            "HWiNFO64.EXE",
        ]);
        assert_eq!(findings.controlling, vec!["FanControl"]);
        assert_eq!(findings.monitoring, vec!["LibreHardwareMonitor"]);
        assert_eq!(classify::<&str>(&[]), Findings::default());
    }
}
