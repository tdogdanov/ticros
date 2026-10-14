//! Красивый вывод в терминал + JSON-отчёты.

use crate::ata::IdentifyInfo;
use crate::nvme::{NvmeController, NvmeNamespace, NvmeSmart};
use crate::scan::ScanResult;
use crate::scsi::{Capacity, InquiryData};
use crate::smart::{Severity, SmartData, SmartHealth};
use crate::thresholds::{BlockClass, ClassCounts};
use serde_json::json;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

// ============================================================
// ANSI-константы
// ============================================================
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[92m";
const YELLOW: &str = "\x1b[93m";
const RED: &str = "\x1b[91m";
const RED_BOLD: &str = "\x1b[91;1m";
const CYAN: &str = "\x1b[96m";
const BLUE: &str = "\x1b[94m";

// ============================================================
// IDENTIFY DEVICE (ATA)
// ============================================================
pub fn print_identify(i: &IdentifyInfo) {
    println!("\n{BOLD}=== IDENTIFY DEVICE ==={RESET}");
    println!("  Модель:      {}", i.model);
    println!("  Серийник:    {}", i.serial);
    println!("  Прошивка:    {}", i.firmware);

    let sectors = i.max_lba();
    let bytes = i.size_bytes();
    let gb = bytes as f64 / 1_000_000_000.0;
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);

    if sectors > 0 {
        println!(
            "  Объём:       {:.1} ГБ ({:.1} ГиБ, {} секторов)",
            gb, gib, sectors
        );
    } else {
        println!("  Объём:       {YELLOW}неизвестен{RESET}");
    }

    match (i.is_ssd, i.rotation_rpm) {
        (true, _) => println!("  Тип:         {GREEN}SSD{RESET}"),
        (false, Some(rpm)) => println!("  Тип:         HDD {} RPM", rpm),
        _ => println!("  Тип:         {DIM}неизвестен{RESET}"),
    }

    match i
        .form_factor
        .or_else(|| guess_form_factor(&i.model, i.is_ssd))
    {
        Some(ff) => println!("  Форм-фактор: {}", ff),
        None => println!("  Форм-фактор: {DIM}не сообщается{RESET}"),
    }

    println!("  Интерфейс:   {}", i.interface);

    let smart = if i.smart_supported {
        format!("{GREEN}да{RESET}")
    } else {
        format!("{RED}нет{RESET}")
    };
    let trim = if i.trim_supported {
        format!("{GREEN}да{RESET}")
    } else {
        format!("{DIM}нет{RESET}")
    };
    let ncq = if i.ncq_supported {
        format!("{GREEN}да{RESET}")
    } else {
        format!("{DIM}нет{RESET}")
    };

    println!("  S.M.A.R.T.:  {}", smart);
    println!("  TRIM:        {}", trim);
    println!("  NCQ:         {}", ncq);
}

/// Определить форм-фактор по имени модели (если IDENTIFY не сообщил).
fn guess_form_factor(model: &str, is_ssd: bool) -> Option<&'static str> {
    let m = model.to_lowercase();

    if m.contains("m.2") || m.contains("m2 ") || m.contains("2280") {
        return Some("M.2");
    }
    if m.contains("msata") {
        return Some("mSATA");
    }
    if m.contains("2.5") || m.contains("2,5") {
        return Some("2.5\"");
    }
    if m.contains("3.5") || m.contains("3,5") {
        return Some("3.5\"");
    }
    if m.contains("1.8") {
        return Some("1.8\"");
    }

    if is_ssd {
        let nvme_markers = [
            "970 ",
            "980 ",
            "990 ",
            "pm9",
            "sm9",
            "sn570",
            "sn770",
            "sn850",
            "sn580",
            "nv1 ",
            "nv2 ",
            "kc3000",
            "a2000",
            "p1 ",
            "p2 ",
            "p3 ",
            "p5 ",
            "t500",
            "sx6000",
            "sx8200",
            "sx8800",
            "gammix",
            "660p",
            "665p",
            "670p",
            "firecuda",
            "barracuda 510",
        ];
        if nvme_markers.iter().any(|&marker| m.contains(marker)) {
            return Some("M.2");
        }
        return Some("2.5\"");
    }

    None
}

// ============================================================
// S.M.A.R.T. (ATA)
// ============================================================
pub fn print_smart(d: &SmartData, full: bool) {
    println!("\n{BOLD}=== S.M.A.R.T. ==={RESET}");
    println!("  Revision:    {:#06x}", d.revision);
    println!("  Источник:    {DIM}{}{RESET}", d.source);

    match d.temperature {
        Some(t) => {
            let color = temp_color(t);
            println!("  Температура: {color}{t}°C{RESET}");
        }
        None => println!("  Температура: {DIM}?{RESET}"),
    }

    let (color, name) = match d.health() {
        SmartHealth::Ok => (GREEN, "OK"),
        SmartHealth::Failed => (RED_BOLD, "FAILED"),
        SmartHealth::Unknown => (YELLOW, "неизвестно"),
    };
    println!("  Здоровье:    {color}{name}{RESET}");

    if full {
        println!(
            "               {DIM}(table={:?}, return={:?}){RESET}",
            d.health_table, d.health_return
        );
    }

    let problems = d.problems();
    if !problems.is_empty() {
        println!("\n  {RED_BOLD}Проблемы:{RESET}");
        for a in &problems {
            let color = if a.failed { RED_BOLD } else { RED };
            println!(
                "    {color}ID {:>3} {:<30} raw={} (val={}, thresh={}){RESET}",
                a.id, a.name, a.raw, a.value, a.threshold
            );
        }
    }

    if full {
        println!(
            "\n  {BOLD}{:>4}  {:<30}  {:>6}  {:>6}  {:>7}  {:>14}{RESET}",
            "ID", "Название", "Value", "Worst", "Thresh", "Raw"
        );
        println!(
            "  {DIM}{:>4}  {:<30}  {:>6}  {:>6}  {:>7}  {:>14}{RESET}",
            "----",
            "------------------------------",
            "------",
            "------",
            "-------",
            "--------------"
        );

        for a in &d.attributes {
            let color = severity_color(a.severity, a.raw, a.failed);
            println!(
                "  {color}{:>4}  {:<30}  {:>6}  {:>6}  {:>7}  {:>14}{RESET}",
                a.id,
                truncate(&a.name, 30),
                a.value,
                a.worst,
                a.threshold,
                format_raw(a.raw)
            );
        }
    }
}

fn temp_color(t: i16) -> &'static str {
    if t < 0 {
        CYAN
    } else if t < 45 {
        GREEN
    } else if t < 55 {
        YELLOW
    } else {
        RED
    }
}

fn severity_color(s: Severity, raw: u64, failed: bool) -> &'static str {
    if failed {
        return RED_BOLD;
    }
    match s {
        Severity::Critical if raw > 0 => RED,
        Severity::Critical => GREEN,
        Severity::Info => YELLOW,
        Severity::Cosmetic => RESET,
        Severity::Unknown => DIM,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{}…", truncated)
    }
}

fn format_raw(raw: u64) -> String {
    if raw < 1_000_000 {
        raw.to_string()
    } else {
        let exp = (raw as f64).log10().floor() as i32;
        let mantissa = raw as f64 / 10f64.powi(exp);
        format!("{:.2}e{}", mantissa, exp)
    }
}

// ============================================================
// Итоги сканирования
// ============================================================
pub fn print_scan_summary(
    counts: &ClassCounts,
    bytes_done: u64,
    elapsed: Duration,
    log_path: &Path,
    bad_log_path: &Path,
    interrupted: bool,
) {
    let speed = bytes_done as f64 / 1_048_576.0 / elapsed.as_secs_f64().max(0.001);
    let total = counts.total();

    println!();
    if interrupted {
        println!("{BOLD}=== Итог (прервано) ==={RESET}");
    } else {
        println!("{BOLD}=== Итог ==={RESET}");
    }
    println!("  Время:      {:.1} с", elapsed.as_secs_f64());
    println!("  Скорость:   {:.1} МБ/с", speed);
    println!("  Проверено:  {} блоков", total);
    println!();

    for class in BlockClass::ALL {
        let n = counts.get(class);
        let pct = if total > 0 {
            n as f64 * 100.0 / total as f64
        } else {
            0.0
        };
        let color = class.ansi_color();
        let reset = BlockClass::ansi_reset();
        println!(
            "  {color}{:<8}{reset} {:>10}  ({:>5.1}%)",
            class.as_str(),
            n,
            pct
        );
    }

    let problem_pct = counts.problem_percent();
    if problem_pct > 0.0 {
        println!("\n  {RED_BOLD}Проблемных: {:.2}%{RESET}", problem_pct);
    }
    let critical_pct = counts.critical_percent();
    if critical_pct > 0.0 {
        println!("  {RED_BOLD}Критичных:  {:.2}%{RESET}", critical_pct);
    }

    println!("  Лог:        {:?}", log_path);
    println!("  Bad-лог:    {:?}", bad_log_path);
}

// ============================================================
// NVMe вывод
// ============================================================
pub fn print_nvme_controller(ctrl: &NvmeController, ns: Option<&NvmeNamespace>) {
    println!("\n{BOLD}=== NVMe Identify Controller ==={RESET}");
    println!("  Модель:      {}", ctrl.model);
    println!("  Серийник:    {}", ctrl.serial);
    println!("  Прошивка:    {}", ctrl.firmware);
    println!("  VID:SSVID:   {}", ctrl.vendor_id_string());
    println!("  Max NS:      {}", ctrl.max_ns);
    println!("  RAB:         {}", ctrl.rab);
    println!("  OACS:        {:#06x}", ctrl.oacs);
    println!("  Форм-фактор: M.2");
    println!("  Интерфейс:   {BLUE}NVMe{RESET}");

    if let Some(n) = ns {
        let bytes = n.size_bytes();
        let gb = bytes as f64 / 1_000_000_000.0;
        let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        println!("\n  {BOLD}Namespace {}{RESET}", n.nsid);
        println!("    Объём:     {:.1} ГБ ({:.1} ГиБ)", gb, gib);
        println!("    Секторов:  {}", n.nsze);
        println!("    LBA size:  {} байт", n.lba_size);
        println!("    Использ.:  {} секторов", n.nuse);
    }
}

pub fn print_nvme_smart(s: &NvmeSmart) {
    println!("\n{BOLD}=== NVMe S.M.A.R.T. ==={RESET}");

    let health = if s.health_ok() {
        format!("{GREEN}OK{RESET}")
    } else {
        format!("{RED_BOLD}WARNING {:#04x}{RESET}", s.critical_warning)
    };
    println!("  Здоровье:          {}", health);

    let t = s.temperature_celsius();
    let color = temp_color(t);
    println!("  Температура:       {color}{t}°C{RESET}");

    let used_color = if s.percent_used < 80 {
        GREEN
    } else if s.percent_used < 95 {
        YELLOW
    } else {
        RED
    };
    println!(
        "  Износ:             {used_color}{}%{RESET}",
        s.percent_used
    );

    let spare_color = if s.available_spare > 20 {
        GREEN
    } else if s.available_spare > 5 {
        YELLOW
    } else {
        RED
    };
    println!(
        "  Available Spare:   {spare_color}{}%{RESET} (порог {}%)",
        s.available_spare, s.spare_thresh
    );

    println!("\n  {BOLD}Счётчики:{RESET}");
    println!("    Data Units Read:     {}", s.data_units_read);
    println!("    Data Units Written:  {}", s.data_units_written);
    println!("    Read Commands:       {}", s.host_read_commands);
    println!("    Write Commands:      {}", s.host_write_commands);
    println!("    Controller Busy:     {} мин", s.controller_busy_time);
    println!("    Power Cycles:        {}", s.power_cycles);
    println!("    Power On Hours:      {}", s.power_on_hours);
    println!("    Unsafe Shutdowns:    {}", s.unsafe_shutdowns);
    println!("    Media Errors:        {}", s.media_errors);
    println!("    Error Log Entries:   {}", s.num_err_log_entries);
    println!("    Warning Temp Time:   {} мин", s.warning_temp_time);
    println!("    Critical Comp Time:  {} мин", s.critical_comp_time);

    let gb_read = s.bytes_read() as f64 / 1_000_000_000.0;
    let gb_written = s.bytes_written() as f64 / 1_000_000_000.0;
    println!("\n  {DIM}Прочитано: {:.1} ГБ{RESET}", gb_read);
    println!("  {DIM}Записано:  {:.1} ГБ{RESET}", gb_written);
}

// ============================================================
// SCSI / USB
// ============================================================
pub fn print_scsi_info(inq: &InquiryData, cap: &Capacity) {
    println!("\n{BOLD}=== SCSI INQUIRY (USB bridge) ==={RESET}");
    println!("  Vendor:      {}", inq.vendor);
    println!("  Product:     {}", inq.product);
    println!("  Revision:    {}", inq.revision);
    println!("  Device type: {:#04x}", inq.device_type);
    println!(
        "  Removable:   {}",
        if inq.is_removable { "да" } else { "нет" }
    );

    let bytes = cap.size_bytes();
    let gb = bytes as f64 / 1_000_000_000.0;
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    println!("\n  {BOLD}Ёмкость:{RESET}");
    println!("    Объём:      {:.1} ГБ ({:.1} ГиБ)", gb, gib);
    println!("    Секторов:   {}", cap.total_sectors);
    println!("    Размер сек: {} байт", cap.sector_size);

    println!("\n  {DIM}Примечание: USB-мосты часто не поддерживают{RESET}");
    println!("  {DIM}S.M.A.R.T. и ATA PASS-THROUGH.{RESET}");
    println!("  {DIM}Для S.M.A.R.T. используйте: smartctl -a {RESET}");
}

// ============================================================
// Список устройств
// ============================================================
pub fn print_device_list(devices: &[String]) {
    println!("\n{BOLD}=== Диски в системе ==={RESET}\n");
    if devices.is_empty() {
        println!("  {DIM}не найдено{RESET}");
    } else {
        for d in devices {
            println!("  {}", d);
        }
    }
    println!("\n  {DIM}Для информации о диске:{RESET}");
    println!("  sudo ticros --device /dev/sda info");
}

// ============================================================
// Ошибки / предупреждения / инфо
// ============================================================
pub fn print_error(msg: &str) {
    eprintln!("{RED_BOLD}Ошибка:{RESET} {}", msg);
}

pub fn print_warn(msg: &str) {
    eprintln!("{YELLOW}Предупреждение:{RESET} {}", msg);
}

pub fn print_info(msg: &str) {
    eprintln!("{CYAN}Инфо:{RESET} {}", msg);
}

// ============================================================
// JSON-отчёты
// ============================================================
pub fn save_identify_json(i: &IdentifyInfo, path: &Path) -> anyhow::Result<()> {
    let ff = i
        .form_factor
        .or_else(|| guess_form_factor(&i.model, i.is_ssd));

    let obj = json!({
        "type": "ata",
        "model": i.model,
        "serial": i.serial,
        "firmware": i.firmware,
        "size_bytes": i.size_bytes(),
        "lba48_sectors": i.lba48_sectors,
        "lba28_sectors": i.lba28_sectors,
        "is_ssd": i.is_ssd,
        "rotation_rpm": i.rotation_rpm,
        "form_factor": ff,
        "form_factor_from_identify": i.form_factor,
        "interface": i.interface,
        "ncq_supported": i.ncq_supported,
        "smart_supported": i.smart_supported,
        "trim_supported": i.trim_supported,
    });
    write_json(path, &obj)
}

pub fn save_nvme_controller_json(
    ctrl: &NvmeController,
    ns: Option<&NvmeNamespace>,
    path: &Path,
) -> anyhow::Result<()> {
    let ns_obj = ns.map(|n| {
        json!({
            "nsid": n.nsid,
            "size_bytes": n.size_bytes(),
            "nsze": n.nsze,
            "ncap": n.ncap,
            "nuse": n.nuse,
            "lba_size": n.lba_size,
        })
    });

    let obj = json!({
        "type": "nvme",
        "model": ctrl.model,
        "serial": ctrl.serial,
        "firmware": ctrl.firmware,
        "vid": ctrl.vid,
        "ssvid": ctrl.ssvid,
        "max_ns": ctrl.max_ns,
        "rab": ctrl.rab,
        "oacs": ctrl.oacs,
        "form_factor": "M.2",
        "interface": "NVMe",
        "namespace": ns_obj,
    });
    write_json(path, &obj)
}

pub fn save_scsi_json(inq: &InquiryData, cap: &Capacity, path: &Path) -> anyhow::Result<()> {
    let obj = json!({
        "type": "scsi",
        "transport": "usb",
        "vendor": inq.vendor,
        "product": inq.product,
        "revision": inq.revision,
        "device_type": inq.device_type,
        "is_removable": inq.is_removable,
        "size_bytes": cap.size_bytes(),
        "total_sectors": cap.total_sectors,
        "sector_size": cap.sector_size,
    });
    write_json(path, &obj)
}

pub fn save_smart_json(d: &SmartData, path: &Path) -> anyhow::Result<()> {
    let attrs: Vec<_> = d
        .attributes
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "value": a.value,
                "worst": a.worst,
                "threshold": a.threshold,
                "raw": a.raw,
                "prefail": a.prefail,
                "online": a.online,
                "failed": a.failed,
                "severity": match a.severity {
                    Severity::Critical => "critical",
                    Severity::Info => "info",
                    Severity::Cosmetic => "cosmetic",
                    Severity::Unknown => "unknown",
                },
            })
        })
        .collect();

    let obj = json!({
        "type": "ata_smart",
        "source": d.source,
        "revision": format!("{:#06x}", d.revision),
        "health": match d.health() {
            SmartHealth::Ok => "OK",
            SmartHealth::Failed => "FAILED",
            SmartHealth::Unknown => "unknown",
        },
        "health_table": match d.health_table {
            SmartHealth::Ok => "OK",
            SmartHealth::Failed => "FAILED",
            SmartHealth::Unknown => "unknown",
        },
        "health_return": match d.health_return {
            SmartHealth::Ok => "OK",
            SmartHealth::Failed => "FAILED",
            SmartHealth::Unknown => "unknown",
        },
        "temperature_c": d.temperature,
        "attributes": attrs,
    });
    write_json(path, &obj)
}

pub fn save_nvme_smart_json(s: &NvmeSmart, path: &Path) -> anyhow::Result<()> {
    let obj = json!({
        "type": "nvme_smart",
        "health_ok": s.health_ok(),
        "critical_warning": s.critical_warning,
        "temperature_c": s.temperature_celsius(),
        "available_spare": s.available_spare,
        "spare_thresh": s.spare_thresh,
        "percent_used": s.percent_used,
        "data_units_read": s.data_units_read.to_string(),
        "data_units_written": s.data_units_written.to_string(),
        "host_read_commands": s.host_read_commands.to_string(),
        "host_write_commands": s.host_write_commands.to_string(),
        "controller_busy_time": s.controller_busy_time.to_string(),
        "power_cycles": s.power_cycles.to_string(),
        "power_on_hours": s.power_on_hours.to_string(),
        "unsafe_shutdowns": s.unsafe_shutdowns.to_string(),
        "media_errors": s.media_errors.to_string(),
        "num_err_log_entries": s.num_err_log_entries.to_string(),
        "warning_temp_time": s.warning_temp_time,
        "critical_comp_time": s.critical_comp_time,
        "bytes_read": s.bytes_read(),
        "bytes_written": s.bytes_written(),
    });
    write_json(path, &obj)
}

pub fn save_scan_json(r: &ScanResult, log_path: &Path, path: &Path) -> anyhow::Result<()> {
    let obj = json!({
        "type": "scan",
        "log_path": log_path.to_string_lossy(),
        "bytes_done": r.bytes_done,
        "elapsed_sec": r.elapsed.as_secs_f64(),
        "speed_mbps": r.bytes_done as f64 / 1_048_576.0 / r.elapsed.as_secs_f64().max(0.001),
        "interrupted": r.interrupted,
        "counts": {
            "grey": r.counts.grey,
            "green": r.counts.green,
            "yellow": r.counts.yellow,
            "red": r.counts.red,
            "bad": r.counts.bad,
            "total": r.counts.total(),
        },
        "problem_percent": r.counts.problem_percent(),
        "critical_percent": r.counts.critical_percent(),
    });
    write_json(path, &obj)
}

fn write_json(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    let mut file =
        File::create(path).map_err(|e| anyhow::anyhow!("Не удалось создать {:?}: {}", path, e))?;
    let s = serde_json::to_string_pretty(value)?;
    file.write_all(s.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

// ============================================================
// Тесты
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn test_truncate_long() {
        let result = truncate("very long attribute name here", 15);
        assert_eq!(result.chars().count(), 15);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_truncate_multibyte() {
        let result = truncate("МодельДискаСуперДлинная", 10);
        assert_eq!(result.chars().count(), 10);
    }

    #[test]
    fn test_temp_color() {
        assert_eq!(temp_color(-5), CYAN);
        assert_eq!(temp_color(20), GREEN);
        assert_eq!(temp_color(50), YELLOW);
        assert_eq!(temp_color(60), RED);
    }

    #[test]
    fn test_format_raw() {
        assert_eq!(format_raw(0), "0");
        assert_eq!(format_raw(999), "999");
        assert_eq!(format_raw(1_000_000), "1.00e6");
    }

    #[test]
    fn test_severity_color_ok() {
        assert_eq!(severity_color(Severity::Critical, 0, false), GREEN);
        assert_eq!(severity_color(Severity::Critical, 10, false), RED);
        assert_eq!(severity_color(Severity::Cosmetic, 0, true), RED_BOLD);
    }

    #[test]
    fn test_guess_form_factor_m2() {
        assert_eq!(
            guess_form_factor("WD Green M.2 2280 240GB", true),
            Some("M.2")
        );
        assert_eq!(guess_form_factor("Samsung SSD 980 1TB", true), Some("M.2"));
    }

    #[test]
    fn test_guess_form_factor_nvme_series() {
        assert_eq!(guess_form_factor("Kingston NV2 1TB", true), Some("M.2"));
        assert_eq!(guess_form_factor("WD Blue SN570 500GB", true), Some("M.2"));
        assert_eq!(guess_form_factor("Crucial P3 Plus 1TB", true), Some("M.2"));
        assert_eq!(guess_form_factor("ADATA XPG Gammix S11", true), Some("M.2"));
    }

    #[test]
    fn test_guess_form_factor_25() {
        assert_eq!(guess_form_factor("ADATA SU650", true), Some("2.5\""));
        assert_eq!(guess_form_factor("Kingston A400 2.5", true), Some("2.5\""));
    }

    #[test]
    fn test_guess_form_factor_35() {
        assert_eq!(
            guess_form_factor("WD Blue 3.5 Desktop", false),
            Some("3.5\"")
        );
    }

    #[test]
    fn test_guess_form_factor_msata() {
        assert_eq!(guess_form_factor("Samsung mSATA 850", true), Some("mSATA"));
    }

    #[test]
    fn test_guess_form_factor_unknown_hdd() {
        assert_eq!(guess_form_factor("WDC WD3200BEVT", false), None);
    }
}
