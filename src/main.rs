//! ticros — Victoria-like диагностика дисков на Rust.

mod ata;
mod log;
mod nvme;
mod remap;
mod scan;
mod scsi;
mod smart;
mod thresholds;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use crate::thresholds::Thresholds;

// ============================================================
// CLI
// ============================================================

#[derive(Parser, Debug)]
#[command(
    name = "ticros",
    version,
    about = "Victoria-like HDD/SSD diagnostic tool (Rust)",
    long_about = None,
)]
struct Cli {
    /// Устройство, например /dev/sda или /dev/nvme0
    #[arg(short, long, global = true)]
    device: Option<PathBuf>,

    /// Отключить цветной вывод
    #[arg(long, global = true)]
    no_color: bool,

    /// Сохранить результат в JSON-файл
    #[arg(long, global = true)]
    report: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Информация о диске (IDENTIFY DEVICE / Identify Controller / SCSI INQUIRY)
    Info,

    /// Читать S.M.A.R.T.
    Smart {
        /// Вывести полную таблицу атрибутов
        #[arg(long)]
        full: bool,
    },

    /// Сканировать поверхность
    Scan {
        #[arg(long, default_value_t = 0)]
        start_mb: u64,
        #[arg(long)]
        end_mb: Option<u64>,
        #[arg(long, default_value_t = 256)]
        block_kb: usize,
        #[arg(long, default_value = "ticros.log")]
        log: PathBuf,
        #[arg(long)]
        stop_on_bad: bool,
        #[arg(long, default_value = "victoria")]
        preset: String,
        #[arg(long)]
        thresholds: Option<String>,
        /// Показать визуальную карту блоков (DOS Victoria-style)
        #[arg(long)]
        map: bool,
    },

    /// Remap одного LBA (ОПАСНО, необратимо!)
    Remap {
        #[arg(long)]
        lba: u64,
        #[arg(long, default_value_t = 1)]
        count: u32,
        #[arg(long)]
        yes: bool,
    },

    /// Remap всех плохих LBA из CSV-лога сканирования (ОПАСНО!)
    RemapLog {
        #[arg(long)]
        log: PathBuf,
        #[arg(long)]
        yes: bool,
        /// Показать, что будет сделано, без выполнения
        #[arg(long)]
        dry_run: bool,
    },

    /// Список доступных пресетов порогов
    Presets,

    /// Список всех дисков в системе
    ListDevices,
}

// ============================================================
// main
// ============================================================

fn main() {
    if let Err(e) = run() {
        ui::print_error(&format!("{:#}", e));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.no_color {
        std::env::set_var("NO_COLOR", "1");
    }

    // Команды, не требующие --device и root
    match cli.cmd {
        Cmd::Presets => {
            print_presets();
            return Ok(());
        }
        Cmd::ListDevices => {
            return cmd_list_devices();
        }
        _ => {}
    }

    let dev = cli
        .device
        .as_ref()
        .context("Укажите --device /dev/sdX или /dev/nvme0")?;

    if !dev.exists() {
        anyhow::bail!("Устройство {:?} не найдено", dev);
    }

    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("Нужны права root. Запустите через sudo.");
    }

    let report = cli.report.as_deref();

    match cli.cmd {
        Cmd::Info => cmd_info(dev, report),
        Cmd::Smart { full } => cmd_smart(dev, full, report),
        Cmd::Scan {
            start_mb,
            end_mb,
            block_kb,
            log,
            stop_on_bad,
            preset,
            thresholds,
            map,
        } => cmd_scan(
            dev,
            start_mb,
            end_mb,
            block_kb,
            log,
            stop_on_bad,
            preset,
            thresholds,
            map,
            report,
        ),
        Cmd::Remap { lba, count, yes } => cmd_remap(dev, lba, count, yes),
        Cmd::RemapLog { log, yes, dry_run } => cmd_remap_log(dev, &log, yes, dry_run),
        Cmd::Presets | Cmd::ListDevices => unreachable!(),
    }
}

// ============================================================
// Определение типа устройства
// ============================================================

fn is_nvme(dev: &Path) -> bool {
    dev.to_string_lossy().contains("nvme")
}

fn is_usb(dev: &Path) -> bool {
    scsi::is_usb_device(dev)
}

// ============================================================
// ListDevices
// ============================================================

fn cmd_list_devices() -> Result<()> {
    use std::fs;
    let dev_dir = Path::new("/dev");
    if !dev_dir.exists() {
        anyhow::bail!("/dev не существует");
    }

    let mut devices: Vec<String> = Vec::new();
    for entry in fs::read_dir(dev_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let is_sd = name_str.starts_with("sd")
            && name_str.len() == 3
            && name_str.chars().last().unwrap().is_ascii_alphabetic();

        let is_nvme_disk =
            name_str.starts_with("nvme") && name_str.contains('n') && name_str.len() >= 7;

        if is_sd || is_nvme_disk {
            let path = entry.path();
            let kind = if is_usb(&path) {
                "usb"
            } else if is_nvme_disk {
                "nvme"
            } else {
                "sata"
            };
            devices.push(format!("{}  [{}]", path.display(), kind));
        }
    }

    devices.sort();
    ui::print_device_list(&devices);
    Ok(())
}

// ============================================================
// Info
// ============================================================

fn cmd_info(dev: &Path, report: Option<&Path>) -> Result<()> {
    if is_usb(dev) {
        ui::print_info("Обнаружен USB-мост, используем SCSI INQUIRY");
        return cmd_info_scsi(dev, report);
    }
    if is_nvme(dev) {
        cmd_info_nvme(dev, report)
    } else {
        cmd_info_ata(dev, report)
    }
}

fn cmd_info_ata(dev: &Path, report: Option<&Path>) -> Result<()> {
    let info =
        ata::identify(dev).with_context(|| format!("IDENTIFY DEVICE failed для {:?}", dev))?;
    ui::print_identify(&info);

    if let Some(path) = report {
        ui::save_identify_json(&info, path)?;
        ui::print_info(&format!("JSON-отчёт сохранён: {:?}", path));
    }
    Ok(())
}

fn cmd_info_nvme(dev: &Path, report: Option<&Path>) -> Result<()> {
    let dev_handle = nvme::NvmeDevice::open(dev)
        .with_context(|| format!("Не удалось открыть NVMe {:?}", dev))?;

    let ctrl_data = dev_handle
        .identify_controller()
        .context("NVMe Identify Controller failed")?;
    let ctrl = nvme::parse_identify_controller(&ctrl_data)?;

    let ns = match dev_handle.identify_namespace(1) {
        Ok(ns_data) => nvme::parse_identify_namespace(&ns_data, 1).ok(),
        Err(_) => None,
    };

    ui::print_nvme_controller(&ctrl, ns.as_ref());

    if let Some(path) = report {
        ui::save_nvme_controller_json(&ctrl, ns.as_ref(), path)?;
        ui::print_info(&format!("JSON-отчёт сохранён: {:?}", path));
    }
    Ok(())
}

fn cmd_info_scsi(dev: &Path, report: Option<&Path>) -> Result<()> {
    let scsi_dev = scsi::ScsiDevice::open(dev)
        .with_context(|| format!("Не удалось открыть SCSI {:?}", dev))?;

    let inq = scsi_dev.inquiry().context("SCSI INQUIRY failed")?;
    let cap = scsi_dev
        .read_capacity_10()
        .or_else(|_| scsi_dev.read_capacity_16())
        .context("READ CAPACITY (10/16) failed")?;

    ui::print_scsi_info(&inq, &cap);

    if let Some(path) = report {
        ui::save_scsi_json(&inq, &cap, path)?;
        ui::print_info(&format!("JSON-отчёт сохранён: {:?}", path));
    }
    Ok(())
}

// ============================================================
// Smart
// ============================================================

fn cmd_smart(dev: &Path, full: bool, report: Option<&Path>) -> Result<()> {
    if is_usb(dev) {
        ui::print_info("USB-мост: пробуем smartctl (SG_IO ATA может не работать)");
    }
    if is_nvme(dev) {
        cmd_smart_nvme(dev, full, report)
    } else {
        cmd_smart_ata(dev, full, report)
    }
}

fn cmd_smart_ata(dev: &Path, full: bool, report: Option<&Path>) -> Result<()> {
    let data = smart::read(dev).with_context(|| format!("SMART READ failed для {:?}", dev))?;
    ui::print_smart(&data, full);

    if let Some(path) = report {
        ui::save_smart_json(&data, path)?;
        ui::print_info(&format!("JSON-отчёт сохранён: {:?}", path));
    }

    let problems = data.problems();
    if !problems.is_empty() {
        ui::print_warn(&format!(
            "Обнаружено {} проблемных атрибутов.",
            problems.len()
        ));
    }
    Ok(())
}

fn cmd_smart_nvme(dev: &Path, _full: bool, report: Option<&Path>) -> Result<()> {
    let dev_handle = nvme::NvmeDevice::open(dev)
        .with_context(|| format!("Не удалось открыть NVMe {:?}", dev))?;

    let smart_data = dev_handle
        .smart_log()
        .context("NVMe SMART/Health Log failed")?;
    let smart = nvme::parse_smart_log(&smart_data)?;

    ui::print_nvme_smart(&smart);

    if let Some(path) = report {
        ui::save_nvme_smart_json(&smart, path)?;
        ui::print_info(&format!("JSON-отчёт сохранён: {:?}", path));
    }

    if !smart.health_ok() {
        ui::print_warn(&format!(
            "NVMe critical_warning = {:#04x}.",
            smart.critical_warning
        ));
    }
    Ok(())
}

// ============================================================
// Scan
// ============================================================

#[allow(clippy::too_many_arguments)]
fn cmd_scan(
    dev: &Path,
    start_mb: u64,
    end_mb: Option<u64>,
    block_kb: usize,
    log: PathBuf,
    stop_on_bad: bool,
    preset: String,
    thresholds_str: Option<String>,
    show_map: bool,
    report: Option<&Path>,
) -> Result<()> {
    let thresholds = if let Some(s) = thresholds_str {
        s.parse::<Thresholds>()
            .map_err(|e| anyhow::anyhow!("Неверный формат --thresholds: {}", e))?
    } else {
        Thresholds::from_preset(&preset).ok_or_else(|| {
            anyhow::anyhow!(
                "Неизвестный пресет: {}. Доступные: {}",
                preset,
                Thresholds::PRESETS.join(", ")
            )
        })?
    };

    thresholds
        .validate()
        .map_err(|e| anyhow::anyhow!("Пороги невалидны: {}", e))?;

    if is_usb(dev) {
        ui::print_info("USB-мост: сканирование через pread (не SG_IO)");
    }

    ui::print_info(&format!(
        "Пороги: fast={}ms good={}ms slow={}ms bad={}ms",
        thresholds.fast_ms, thresholds.good_ms, thresholds.slow_ms, thresholds.bad_ms
    ));

    if stop_on_bad {
        ui::print_info("Остановка при первом бэде включена");
    }

    if show_map {
        ui::print_info("Режим визуальной карты (DOS Victoria-style)");
    }

    let cfg = scan::ScanConfig {
        device: dev.to_path_buf(),
        start_mb,
        end_mb,
        block_kb,
        log_path: log,
        stop_on_bad,
        thresholds,
        show_map,
    };

    let result = scan::run(cfg)?;

    ui::print_scan_summary(
        &result.counts,
        result.bytes_done,
        result.elapsed,
        &result.log_path,
        &result.bad_log_path,
        result.interrupted,
    );

    if let Some(path) = report {
        ui::save_scan_json(&result, &result.log_path, path)?;
        ui::print_info(&format!("JSON-отчёт сохранён: {:?}", path));
    }

    if result.counts.bad > 0 {
        ui::print_warn(&format!(
            "Обнаружено {} бэд-блоков. Рекомендуется remap.",
            result.counts.bad
        ));
    }

    Ok(())
}

// ============================================================
// Remap
// ============================================================

fn cmd_remap(dev: &Path, lba: u64, count: u32, yes: bool) -> Result<()> {
    if is_nvme(dev) {
        anyhow::bail!("Remap не поддерживается для NVMe.");
    }
    if is_usb(dev) {
        anyhow::bail!("Remap не поддерживается для USB-мостов.");
    }
    if !yes {
        ui::print_warn("Remap необратим! Добавьте --yes для подтверждения.");
        std::process::exit(1);
    }
    ui::print_warn(&format!(
        "Remap LBA={} count={} на {:?}. Данные будут потеряны.",
        lba, count, dev
    ));
    remap::reassign(dev, lba, count, yes)?;
    ui::print_info(&format!("Remap завершён: LBA={} count={}", lba, count));
    Ok(())
}

fn cmd_remap_log(dev: &Path, log: &Path, yes: bool, dry_run: bool) -> Result<()> {
    if is_nvme(dev) {
        anyhow::bail!("Remap не поддерживается для NVMe.");
    }
    if is_usb(dev) {
        anyhow::bail!("Remap не поддерживается для USB-мостов.");
    }

    if dry_run {
        return remap::dry_run(dev, log);
    }

    if !yes {
        ui::print_warn("Remap необратим! Добавьте --yes.");
        std::process::exit(1);
    }
    ui::print_warn(&format!("Remap всех плохих LBA из {:?}.", log));
    remap::reassign_from_log(dev, log, yes)?;
    Ok(())
}

// ============================================================
// Presets
// ============================================================

fn print_presets() {
    println!("Доступные пресеты порогов (--preset):\n");
    for name in Thresholds::PRESETS {
        let t = Thresholds::from_preset(name).unwrap();
        println!(
            "  {:<10}  fast={:<6} good={:<6} slow={:<6} bad={:<6} мс",
            name, t.fast_ms, t.good_ms, t.slow_ms, t.bad_ms
        );
    }
    println!();
    println!("Свои пороги: --thresholds 10,50,200,1000");
}
