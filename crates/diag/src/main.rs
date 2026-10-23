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
use fanforge_core::telemetry::is_plausible;
use fanforge_hw::HwError;
use fanforge_hw::amd::AmdCpu;
use fanforge_hw::board::detect_identity;
use fanforge_hw::gpu_fans::nvidia::NvidiaFans;
use fanforge_hw::gpu_fans::{FanDriver, FanSample, GpuDesc};
use fanforge_hw::pawnio;
use fanforge_hw::sensors::cores::{CoreTempSource, enabled_slots};
use fanforge_hw::sensors::{self, SensorSource};
use fanforge_hw::smu::{self, CoreTempLayout, RyzenSmu};
use fanforge_hw::snapshot::BiosSnapshot;
use fanforge_hw::superio::RegisterDump;
use fanforge_hw::system::{
    Chip, Hardware, RELEASE_BUDGET, attach_chip, attach_second_chip, force_all_auto, open_chip,
};
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
        #[arg(
            long,
            default_value_t = 1,
            value_parser = clap::value_parser!(u8).range(1..=2),
            help = "Which fan chip: 1 (first) or 2 (the second chip on 0x4E)"
        )]
        slot: u8,
    },
    #[command(
        about = "Print the AMD SMU PM table version and the per-core temperatures it holds (no settings are changed)"
    )]
    PmTable {
        #[arg(long, help = "Also print every value in the table")]
        raw: bool,
    },
    #[command(about = "Compare two dumps")]
    Diff { before: PathBuf, after: PathBuf },
    #[command(about = "Temporarily set one fan channel to a fixed duty, then restore BIOS control")]
    SetPwm {
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=7))]
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
    #[command(about = "List NVIDIA GPU fans and their speeds without changing anything")]
    Gpu,
    #[command(
        about = "Temporarily set one NVIDIA GPU fan to a fixed speed, then return it to the driver"
    )]
    SetGpuFan {
        #[arg(long, default_value_t = 1, help = "GPU number from `gpu`")]
        gpu: usize,
        #[arg(long, help = "Fan number from `gpu`")]
        fan: u32,
        #[arg(
            long,
            help = "Speed in percent (the driver minimum or 30, whichever is higher, to 100)"
        )]
        duty: u32,
        #[arg(
            long,
            default_value_t = 30,
            help = "Seconds before the driver gets the fan back"
        )]
        timeout: u64,
        #[arg(long, help = "Run even if FanForge is running")]
        force: bool,
    },
    #[command(about = "Return every NVIDIA GPU fan to the driver")]
    RestoreGpu,
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Detect => detect(),
        Command::Read => read(),
        Command::Temps => temps(),
        Command::Dump { out, slot } => dump(out.as_deref(), slot),
        Command::PmTable { raw } => pm_table(raw),
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
        Command::Gpu => gpu(),
        Command::SetGpuFan {
            gpu,
            fan,
            duty,
            timeout,
            force,
        } => set_gpu_fan(gpu, fan, duty, Duration::from_secs(timeout), force),
        Command::RestoreGpu => restore_gpu(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn detect() -> DiagResult {
    let version = pawnio::installed_version().unwrap_or_else(|| "not installed".to_owned());
    println!("PawnIO version: {version}");
    let identity = detect_identity();
    println!("Board: {} {}", identity.manufacturer, identity.product);
    let mut chip = attach_chip()?;
    describe_chip(&mut chip)?;
    let second = attach_second_chip(&chip);
    println!("Second fan chip: {}", second.describe());
    if let Some(mut second) = second.chip() {
        describe_chip(&mut second)?;
    }
    Ok(())
}

fn describe_chip(chip: &mut Chip) -> DiagResult {
    let config = chip.config();
    println!(
        "Super I/O: {} (chip {:#06x} version {}) on port {:#04x}, base {:#06x}",
        config.chip.name,
        config.chip_id,
        config.version,
        config.slot.config_port(),
        config.base_address
    );
    println!(
        "Fan headers: {}, temperatures: {}",
        chip.fan_count(),
        chip.temp_names().len()
    );
    let wiring = chip.wiring();
    for channel in 0..chip.fan_count() {
        let note = match (wiring.has_tach(channel), wiring.has_pwm(channel)) {
            (true, true) => continue,
            (false, false) => "not wired as a fan header (FanForge hides it)",
            (true, false) => "speed only (the board uses its control pin for something else)",
            (false, true) => "control only (the board uses its speed pin for something else)",
        };
        println!("  Header {}: {note}", channel + 1);
    }
    let control = if !chip.chip().fan_control {
        "monitor only (FanForge doesn't support fan control on this chip)"
    } else if !chip.check_pwm_configuration()? {
        "monitor only (the BIOS set up the fan outputs with inverted polarity, a known BIOS fault that Linux it87 also won't drive; check the fan settings in the BIOS setup)"
    } else {
        "supported"
    };
    println!("Fan control: {control}");
    if chip.io_space_locked()? {
        println!(
            "Hardware monitor I/O space: locked by the firmware (FanForge, `read` and `set-pwm` clear it; `dump` skips the monitor registers until then)"
        );
    }
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
    match CoreTempSource::default().read() {
        Ok(cores) => {
            for core in cores {
                match core.celsius {
                    Some(celsius) => println!("{:<16} {celsius:>6.1} °C", core.label),
                    None => println!("{:<16} {:>9}", core.label, "n/a"),
                }
            }
        }
        Err(error) => println!("per-core temperatures: {error}"),
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

fn dump(out: Option<&Path>, slot: u8) -> DiagResult {
    let mut primary = attach_chip()?;
    let dump = if slot == 2 {
        let second = attach_second_chip(&primary);
        let described = second.describe();
        second
            .chip()
            .ok_or_else(|| format!("no second fan chip to dump: {described}"))?
            .dump()?
    } else {
        primary.dump()?
    };
    let json = serde_json::to_string_pretty(&dump)?;
    match out {
        Some(path) => {
            fs::write(path, json)?;
            println!("wrote {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}

fn pm_table(raw: bool) -> DiagResult {
    let smu = RyzenSmu::open()?;
    let info = smu.info();
    println!(
        "CPU: {} (family {:#x}, model {:#x})",
        info.brand, info.family, info.model
    );
    let code = smu.code_name()?;
    println!(
        "Code name: {} ({code})",
        smu::code_name(code).unwrap_or("unknown")
    );
    let version = smu.smu_version()?;
    println!(
        "SMU firmware: {} ({version:#010x})",
        smu::format_smu_version(version)
    );
    let table = smu.pm_table();
    println!(
        "PM table version: {:#08x}, base {:#x}",
        table.version, table.base
    );
    let values = smu.read_pm_table()?;
    let cpu = AmdCpu::open();
    match cpu
        .as_ref()
        .map_err(ToString::to_string)
        .and_then(|cpu| cpu.read().map_err(|error| error.to_string()))
    {
        Ok(temps) => {
            println!("{:<16} {:>8.1} °C", "Tctl", temps.tctl);
            for (index, celsius) in temps.ccds {
                println!("{:<16} {celsius:>8.1} °C", format!("CCD{}", index + 1));
            }
        }
        Err(error) => println!("Tctl: {error}"),
    }
    match CoreTempLayout::for_version(table.version) {
        Some(layout) => {
            let enabled = match &cpu {
                Ok(cpu) => enabled_slots(&layout, &smu, cpu).map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            match &enabled {
                Ok(slots) => println!(
                    "Enabled cores: {} of {} slots ({:?})",
                    slots.len(),
                    layout.cores,
                    slots
                ),
                Err(error) => println!("Enabled cores: unknown ({error})"),
            }
            match fanforge_win::topology::physical_cores() {
                Ok(cores) => {
                    let verdict = match &enabled {
                        Ok(slots) if slots.len() == cores.len() => {
                            "matches, so FanForge shows per-core temperatures"
                        }
                        Ok(_) => "doesn't match, so FanForge hides per-core temperatures",
                        Err(_) => "can't be compared",
                    };
                    println!("Windows reports {} physical cores; {verdict}", cores.len());
                }
                Err(error) => println!("Windows physical cores: unknown ({error})"),
            }
            println!(
                "Per-core temperatures by table slot (ryzen_monitor layout, index {}):",
                layout.offset
            );
            for core in layout
                .core_temps(&values, &layout.all_slots())
                .unwrap_or_default()
            {
                let label = match &enabled {
                    Ok(slots) => match slots.iter().position(|&slot| slot == core.core) {
                        Some(index) => format!("Core{index} (CCD{})", core.ccd),
                        None => format!("slot {} (CCD{}) fused off", core.core, core.ccd),
                    },
                    Err(_) => format!("slot {} (CCD{})", core.core, core.ccd),
                };
                let flag = if core.celsius == 0.0 {
                    "  zero"
                } else if is_plausible(core.celsius) {
                    ""
                } else {
                    "  out of range"
                };
                println!("{label:<28} {:>8.1} °C{flag}", core.celsius);
            }
        }
        None => println!(
            "No per-core layout is known for PM table version {:#08x}; run with --raw and share the output",
            table.version
        ),
    }
    if raw {
        for (index, value) in values.iter().enumerate() {
            println!("[{index:>4}] {value:>14.4}");
        }
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
    if !chip.fan_control() {
        return Err(HwError::NoFanControl(chip.chip().name).into());
    }
    if channel >= chip.fan_count() {
        return Err(format!(
            "the {} has {} fan channels",
            chip.chip().name,
            chip.fan_count()
        )
        .into());
    }
    let current = chip.channel_states()?[channel];
    let saved = snapshot
        .channel(channel)
        .filter(|_| snapshot.chip_id == chip.config().chip_id)
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
    let path = paths::bios_snapshot_path();
    let snapshot = BiosSnapshot::load(&path)?;
    let mut chip = open_chip()?;
    let primary = if chip.can_release() {
        force_all_auto(&mut chip, &snapshot, RELEASE_BUDGET * 3).map(|()| {
            println!("all {} channels returned to BIOS control", chip.fan_count());
        })
    } else {
        println!(
            "the {} is monitor-only; FanForge never drives its fans, so there is nothing to restore",
            chip.chip().name
        );
        Ok(())
    };
    if fanforge_hw::second::pending(&path) {
        match fanforge_hw::second::release_from_disk(Some(&chip), &path, RELEASE_BUDGET * 3) {
            Ok(()) => println!(
                "the second chip's fans and Gigabyte's fan controller are back under BIOS control"
            ),
            Err(error) => println!("the second chip could not be fully restored: {error}"),
        }
    }
    Ok(primary?)
}

fn describe(sample: Result<FanSample, String>) -> String {
    let Ok(sample) = sample else {
        return sample.err().unwrap_or_default();
    };
    let value = |value: Option<u32>, unit: &str| {
        value.map_or_else(|| "n/a".to_owned(), |value| format!("{value}{unit}"))
    };
    let mode = match sample.manual {
        Some(true) => "manual",
        Some(false) => "driver",
        None => "n/a",
    };
    format!(
        "speed {:>5}  target {:>5}  {:>9}  mode {mode}",
        value(sample.percent, "%"),
        value(sample.target, "%"),
        value(sample.rpm, " rpm"),
    )
}

fn gpu() -> DiagResult {
    let started = Instant::now();
    let fans = NvidiaFans::open()?;
    println!("NVML started in {} ms", started.elapsed().as_millis());
    if fans.gpus().is_empty() {
        println!("no NVIDIA GPUs");
    }
    for (index, gpu) in fans.gpus().iter().enumerate() {
        let range = gpu.range.map_or_else(
            || "unknown".to_owned(),
            |(min, max)| format!("{min}-{max}%"),
        );
        println!(
            "GPU {}: {} ({}), {} fans, range {range}",
            index + 1,
            gpu.label,
            gpu.key,
            gpu.fans
        );
        for fan in 0..gpu.fans {
            println!("  fan {}: {}", fan + 1, describe(fans.sample(gpu, fan)));
        }
    }
    Ok(())
}

fn set_gpu_fan(gpu: usize, fan: u32, duty: u32, timeout: Duration, force: bool) -> DiagResult {
    if app_running() && !force {
        return Err("FanForge is running; quit it from the tray first or pass --force".into());
    }
    let fans = NvidiaFans::open()?;
    let gpus = fans.gpus();
    let target: &GpuDesc = gpu
        .checked_sub(1)
        .and_then(|index| gpus.get(index))
        .ok_or_else(|| format!("there are {} NVIDIA GPUs", fans.gpus().len()))?;
    let index = fan
        .checked_sub(1)
        .filter(|&index| index < target.fans)
        .ok_or_else(|| format!("{} has {} fans", target.label, target.fans))?;
    let (min, max) = target.range.unwrap_or((MIN_DIAG_DUTY as u32, 100));
    let min = min.max(MIN_DIAG_DUTY as u32);
    if !(min..=max).contains(&duty) {
        return Err(format!("duty must be between {min} and {max}").into());
    }
    if fans.sample(target, index)?.manual == Some(true) {
        return Err(
            "this fan is already in manual mode; another program may be controlling it".into(),
        );
    }

    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;

    if let Err(error) = fans.set_speed(target, index, duty) {
        fans.restore(target, index).ok();
        return Err(error.into());
    }
    println!(
        "{} fan {fan} set to {duty}% for up to {}s (Ctrl+C returns it sooner)",
        target.label,
        timeout.as_secs()
    );
    let started = Instant::now();
    while started.elapsed() < timeout && !interrupted.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_secs(1));
        println!(
            "{:>3}s  {}",
            started.elapsed().as_secs(),
            describe(fans.sample(target, index))
        );
    }
    let restored = fans.restore(target, index);
    if restored.is_ok() {
        println!("{} fan {fan} returned to the driver", target.label);
        for second in 1..=3 {
            thread::sleep(Duration::from_secs(1));
            println!("+{second}s  {}", describe(fans.sample(target, index)));
        }
    }
    restored?;
    Ok(())
}

fn restore_gpu() -> DiagResult {
    let fans = NvidiaFans::open()?;
    let mut failed = false;
    for gpu in fans.gpus() {
        for fan in 0..gpu.fans {
            match fans.restore(&gpu, fan) {
                Ok(()) => println!("{} fan {} returned to the driver", gpu.label, fan + 1),
                Err(error) => {
                    failed = true;
                    println!("{} fan {}: {error}", gpu.label, fan + 1);
                }
            }
        }
    }
    if failed {
        return Err("some GPU fans could not be returned to the driver".into());
    }
    Ok(())
}
