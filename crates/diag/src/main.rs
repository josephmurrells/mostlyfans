use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use fanforge_core::Duty;
use fanforge_hw::HwError;
use fanforge_hw::board::detect_identity;
use fanforge_hw::ite::{FAN_CHANNELS, It8655e, RegisterDump};
use fanforge_hw::mutex::{ISA_BUS, NamedMutex};
use fanforge_hw::pawnio;
use fanforge_hw::port::LpcIo;
use fanforge_hw::sensors;
use fanforge_hw::snapshot::BiosSnapshot;
use fanforge_hw::system::{Chip, Hardware, RELEASE_BUDGET, force_all_auto, open_chip};
use fanforge_win::InstanceMutex;
use fanforge_win::paths::{self, INSTANCE_MUTEX};

const MIN_DIAG_DUTY: f32 = 30.0;

type DiagResult = Result<(), Box<dyn Error>>;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "FanForge hardware diagnostics (run as administrator)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Detect PawnIO, the board and the Super I/O chip without changing anything")]
    Detect,
    #[command(about = "Print one round of fan and temperature readings")]
    Read,
    #[command(about = "Print GPU and drive temperatures (no driver or elevation needed)")]
    Temps,
    #[command(
        about = "Dump Super I/O config and every EC bank as JSON without changing chip state"
    )]
    Dump {
        #[arg(long, help = "Write to this file instead of stdout")]
        out: Option<PathBuf>,
    },
    #[command(about = "Compare two dumps")]
    Diff { before: PathBuf, after: PathBuf },
    #[command(about = "Temporarily set one fan channel to a fixed duty, then restore BIOS control")]
    SetPwm {
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=3))]
        channel: u8,
        #[arg(long, help = "Duty in percent (30-100)")]
        duty: f32,
        #[arg(
            long,
            default_value_t = 30,
            help = "Seconds before BIOS control is restored"
        )]
        timeout: u64,
        #[arg(long, help = "Run even if FanForge is running")]
        force: bool,
    },
    #[command(about = "Return every fan channel to BIOS control")]
    Restore,
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Detect => detect(),
        Command::Read => read(),
        Command::Temps => temps(),
        Command::Dump { out } => dump(out.as_deref()),
        Command::Diff { before, after } => diff(&before, &after),
        Command::SetPwm {
            channel,
            duty,
            timeout,
            force,
        } => set_pwm(
            usize::from(channel - 1),
            duty,
            Duration::from_secs(timeout),
            force,
        ),
        Command::Restore => restore(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn attach() -> Result<It8655e<LpcIo>, HwError> {
    It8655e::attach(LpcIo::open_primary()?, NamedMutex::open(ISA_BUS)?)
}

fn detect() -> DiagResult {
    let version = pawnio::installed_version().unwrap_or_else(|| "not installed".to_owned());
    println!("PawnIO version: {version}");
    let identity = detect_identity();
    println!("Board: {} {}", identity.manufacturer, identity.product);
    let config = attach()?.config();
    println!(
        "Super I/O: chip {:#06x} version {} at base {:#06x}",
        config.chip_id, config.version, config.base_address
    );
    println!("Fan headers: {FAN_CHANNELS}");
    Ok(())
}

fn temps() -> DiagResult {
    for mut source in sensors::default_sources() {
        match source.read() {
            Ok(readings) if readings.is_empty() => println!("{}: no sensors", source.name()),
            Ok(readings) => {
                for sensor in readings {
                    let celsius = sensor
                        .celsius
                        .map_or_else(|| "n/a".to_owned(), |celsius| format!("{celsius:.1} °C"));
                    println!("{:<32} {celsius:>9}  {}", sensor.label, sensor.id);
                }
            }
            Err(error) => println!("{}: {error}", source.name()),
        }
    }
    Ok(())
}

fn read() -> DiagResult {
    let mut hardware = Hardware::open(&paths::bios_snapshot_path())?;
    let readings = hardware.read()?;
    for sensor in &readings.sensors {
        match sensor.celsius {
            Some(celsius) => println!("{:<16} {celsius:>6.1} °C", sensor.label),
            None => println!("{:<16} {:>9}", sensor.label, "n/a"),
        }
    }
    for fan in &readings.fans {
        let rpm = fan
            .rpm
            .map_or_else(|| "n/a".to_owned(), |rpm| format!("{rpm} rpm"));
        let duty = fan
            .duty
            .map_or_else(|| "auto".to_owned(), |duty| duty.to_string());
        println!("{:<16} {rpm:>9} {duty:>6}", fan.label);
    }
    for warning in hardware.warnings() {
        println!("warning: {warning}");
    }
    Ok(())
}

fn dump(out: Option<&Path>) -> DiagResult {
    let json = serde_json::to_string_pretty(&attach()?.dump()?)?;
    match out {
        Some(path) => {
            fs::write(path, json)?;
            println!("wrote {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}

fn diff(before: &Path, after: &Path) -> DiagResult {
    let before: RegisterDump = serde_json::from_str(&fs::read_to_string(before)?)?;
    let after: RegisterDump = serde_json::from_str(&fs::read_to_string(after)?)?;
    let lines = before.diff(&after);
    if lines.is_empty() {
        println!("no differences");
    }
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

fn app_running() -> bool {
    InstanceMutex::exists(INSTANCE_MUTEX)
}

fn set_pwm(channel: usize, duty: f32, timeout: Duration, force: bool) -> DiagResult {
    if !(MIN_DIAG_DUTY..=100.0).contains(&duty) {
        return Err(format!("duty must be between {MIN_DIAG_DUTY} and 100").into());
    }
    if app_running() && !force {
        return Err("FanForge is running; quit it from the tray first or pass --force".into());
    }
    let snapshot = BiosSnapshot::load(&paths::bios_snapshot_path())?;
    let mut chip = open_chip()?;
    let current = chip.channel_states()?[channel];
    let saved = snapshot
        .channel(channel)
        .or_else(|| current.is_auto().then_some(current));

    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;

    chip.set_duty(channel, Duty::new(duty).to_raw())?;
    println!(
        "channel {} set to {duty:.0}% for up to {}s (Ctrl+C restores sooner)",
        channel + 1,
        timeout.as_secs()
    );
    let watched = watch(&mut chip, channel, &interrupted, timeout);
    let restored = chip.restore_patiently(channel, saved, RELEASE_BUDGET * 3);
    if restored.is_ok() {
        println!("channel {} returned to BIOS control", channel + 1);
    }
    watched?;
    restored?;
    Ok(())
}

fn watch(
    chip: &mut Chip,
    channel: usize,
    interrupted: &AtomicBool,
    timeout: Duration,
) -> Result<(), HwError> {
    let started = Instant::now();
    while started.elapsed() < timeout && !interrupted.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_secs(1));
        let rpm = chip.read()?.rpm[channel].map_or_else(|| "n/a".to_owned(), |rpm| rpm.to_string());
        println!("{:>3}s  rpm {rpm}", started.elapsed().as_secs());
    }
    Ok(())
}

fn restore() -> DiagResult {
    let snapshot = BiosSnapshot::load(&paths::bios_snapshot_path())?;
    force_all_auto(&mut open_chip()?, &snapshot, RELEASE_BUDGET * 3)?;
    println!("all {FAN_CHANNELS} channels returned to BIOS control");
    Ok(())
}
