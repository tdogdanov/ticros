//! Remap через ATA SET FEATURES / REASSIGN_SECTORS.
//!
//! ⚠ ВНИМАНИЕ: НЕОБРАТИМО!
//! Используйте только после копирования данных с диска.
//!
//! ATA-команда REASSIGN_SECTORS (SET FEATURES с feature=0x07) просит
//! контроллер переназначить указанные LBA на резервные сектора из spare area.
//! После remap исходные данные на этих LBA теряются.
//!
//! Ограничения:
//!   - Sector Count регистр — 16 бит, максимум 65535 секторов за команду.
//!   - Для LBA > 2^28 нужен 48-bit режим (LBA48), иначе ABORTED COMMAND.
//!   - На SSD spare area ограничена, remap может вернуть ILLEGAL REQUEST.
//!   - USB-мосты не поддерживают REASSIGN_SECTORS.

use crate::ata::{identify, AtaDevice, IdentifyInfo};
use crate::scsi;
use anyhow::{bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ============================================================
// Константы
// ============================================================

/// Пауза между командами REASSIGN (SSD могут тормозить при записи в spare area).
const REASSIGN_DELAY_MS: u64 = 50;

/// Максимум секторов за одну команду. Sector Count — 16 бит (65535),
/// но многие диски не принимают count > 1, поэтому безопасное значение = 1.
const MAX_CHUNK: u32 = 1;

// ============================================================
// Remap одного диапазона
// ============================================================

/// Переназначить `count` секторов начиная с `lba`.
///
/// Если `count == 0` — переназначается один сектор (`lba`).
pub fn reassign(device: &Path, lba: u64, count: u32, yes: bool) -> Result<()> {
    // ─── 1. Подтверждение ──────────────────────────────────
    require_yes(yes)?;

    // ─── 2. Проверка USB ───────────────────────────────────
    if scsi::is_usb_device(device) {
        bail!("Remap не поддерживается для USB-мостов (нет ATA PASS-THROUGH).");
    }

    let count_u64 = if count == 0 { 1 } else { count as u64 };

    // ─── 3. Паспорт диска — проверка границ ────────────────
    let info = identify_device_info(device)?;
    validate_range(&info, lba, count_u64)?;
    warn_if_ssd(&info);

    // ─── 4. Открываем на запись ────────────────────────────
    let dev =
        AtaDevice::open(device, true).context("Не удалось открыть диск на запись (нужен root)")?;

    eprintln!(
        "[*] REASSIGN start: device={:?}\n    model={}\n    lba={} count={}",
        device, info.model, lba, count_u64
    );

    // ─── 5. Ctrl+C ─────────────────────────────────────────
    let stop = install_ctrlc_handler();

    // ─── 6. Прогресс-бар ───────────────────────────────────
    let pb = ProgressBar::new(count_u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.yellow} [{elapsed_precise}] [{bar:40.red/blue}] {pos}/{len} ({eta}) {msg}",
        )?
        .progress_chars("█▉▊▋▌▍▎▏  "),
    );
    pb.set_message(format!("LBA={}", lba));

    // ─── 7. Цикл remap ─────────────────────────────────────
    let mut remaining = count_u64;
    let mut cur_lba = lba;
    let mut ok_count = 0u64;
    let mut err_count = 0u64;
    let t0 = Instant::now();

    while remaining > 0 {
        if stop.load(Ordering::SeqCst) {
            eprintln!("\n[!] Прервано пользователем");
            break;
        }

        let chunk = remaining.min(MAX_CHUNK as u64) as u16;

        match dev.reassign_sectors(cur_lba, chunk) {
            Ok(_) => ok_count += chunk as u64,
            Err(e) => {
                eprintln!(
                    "\n[!] REASSIGN_SECTORS failed at LBA={} count={}: {}",
                    cur_lba, chunk, e
                );
                err_count += chunk as u64;
            }
        }

        std::thread::sleep(Duration::from_millis(REASSIGN_DELAY_MS));

        cur_lba += chunk as u64;
        remaining -= chunk as u64;
        pb.set_position(ok_count + err_count);
        pb.set_message(format!("LBA={}", cur_lba));
    }

    pb.finish_and_clear();

    // ─── 8. SMART-статус после remap ───────────────────────
    match dev.smart_return_status() {
        Ok(true) => eprintln!("[+] SMART RETURN STATUS: OK"),
        Ok(false) => eprintln!("[!] SMART RETURN STATUS: FAILED"),
        Err(e) => eprintln!("[?] Не удалось прочитать SMART RETURN STATUS: {}", e),
    }

    let elapsed = t0.elapsed().as_secs_f64();
    eprintln!(
        "[+] REASSIGN complete: OK={} ERR={} из {} ({:.1} с)",
        ok_count, err_count, count_u64, elapsed
    );

    if err_count > 0 {
        bail!("{} секторов не переназначено (см. лог выше)", err_count);
    }
    Ok(())
}

// ============================================================
// Dry-run: показать, что будет сделано
// ============================================================

/// Показать, какие LBA будут переназначены, без выполнения.
pub fn dry_run(device: &Path, log_path: &Path) -> Result<()> {
    if scsi::is_usb_device(device) {
        bail!("Remap не поддерживается для USB-мостов.");
    }

    let bad_lbas = parse_bad_lbas_from_log(log_path)?;
    if bad_lbas.is_empty() {
        eprintln!("[i] Плохих блоков не найдено в {:?}", log_path);
        return Ok(());
    }

    let info = identify_device_info(device)?;
    let max_lba = info.max_lba();

    println!("\x1b[1m=== Dry-run: что будет переназначено ===\x1b[0m");
    println!("  Устройство: {:?}", device);
    println!("  Модель:     {}", info.model);
    println!("  Всего LBA:  {}", bad_lbas.len());
    println!();

    let mut valid = 0u64;
    let mut out_of_range = 0u64;

    for (i, &lba) in bad_lbas.iter().take(20).enumerate() {
        if lba >= max_lba {
            println!("  {}. LBA {:>10}  [ВНЕ ДИАПАЗОНА]", i + 1, lba);
            out_of_range += 1;
        } else {
            println!("  {}. LBA {:>10}  [OK]", i + 1, lba);
            valid += 1;
        }
    }

    if bad_lbas.len() > 20 {
        println!("  ... ещё {} LBA", bad_lbas.len() - 20);
    }

    println!();
    println!("  Валидных:        {}", valid);
    println!("  Вне диапазона:   {}", out_of_range);
    println!();
    println!("  \x1b[93mЗапустить remap:\x1b[0m");
    println!(
        "  sudo ticros --device {:?} remap-log --log {:?} --yes",
        device, log_path
    );

    Ok(())
}

// ============================================================
// Remap из CSV-лога
// ============================================================

/// Remap всех плохих LBA из CSV-файла, сгенерированного `scan`.
pub fn reassign_from_log(device: &Path, log_path: &Path, yes: bool) -> Result<()> {
    require_yes(yes)?;

    if scsi::is_usb_device(device) {
        bail!("Remap не поддерживается для USB-мостов.");
    }

    let bad_lbas = parse_bad_lbas_from_log(log_path)?;
    if bad_lbas.is_empty() {
        eprintln!("[i] Плохих блоков не найдено в {:?}", log_path);
        return Ok(());
    }

    eprintln!(
        "[*] Найдено {} уникальных плохих LBA в {:?}",
        bad_lbas.len(),
        log_path
    );

    let info = identify_device_info(device)?;
    let max_lba = info.max_lba();
    warn_if_ssd(&info);

    let dev =
        AtaDevice::open(device, true).context("Не удалось открыть диск на запись (нужен root)")?;

    let stop = install_ctrlc_handler();

    let pb = ProgressBar::new(bad_lbas.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.yellow} [{elapsed_precise}] [{bar:40.red/blue}] {pos}/{len} ({eta}) {msg}",
        )?
        .progress_chars("█▉▊▋▌▍▎▏  "),
    );

    let t0 = Instant::now();
    let mut ok_count = 0u64;
    let mut err_count = 0u64;
    let mut skip_count = 0u64;

    for (i, &lba) in bad_lbas.iter().enumerate() {
        if stop.load(Ordering::SeqCst) {
            eprintln!("\n[!] Прервано пользователем");
            break;
        }

        pb.set_position(i as u64);
        pb.set_message(format!("LBA={}", lba));

        if lba >= max_lba {
            eprintln!(
                "\n[{}/{}] LBA {} вне диапазона (max {}), пропускаем",
                i + 1,
                bad_lbas.len(),
                lba,
                max_lba - 1
            );
            skip_count += 1;
            continue;
        }

        match dev.reassign_sectors(lba, 1) {
            Ok(_) => ok_count += 1,
            Err(e) => {
                eprintln!("\n[!] Ошибка на LBA {}: {}", lba, e);
                err_count += 1;
            }
        }

        std::thread::sleep(Duration::from_millis(REASSIGN_DELAY_MS));
    }

    pb.finish_and_clear();

    let elapsed = t0.elapsed().as_secs_f64();
    eprintln!(
        "[+] Remap из лога завершён: OK={} ERR={} SKIP={} ({:.1} с)",
        ok_count, err_count, skip_count, elapsed
    );

    if err_count > 0 {
        bail!("{} LBA не переназначено", err_count);
    }
    Ok(())
}

// ============================================================
// Парсинг CSV
// ============================================================

/// Извлечь уникальные плохие LBA из CSV-лога сканирования.
///
/// Поддерживает:
///   - BOM в начале файла
///   - Пустые строки
///   - `#`-комментарии
///   - Разделители `,`, `;`, таб
pub fn parse_bad_lbas_from_log(log_path: &Path) -> Result<Vec<u64>> {
    let raw =
        std::fs::read(log_path).with_context(|| format!("Не удалось прочитать {:?}", log_path))?;

    // Убираем UTF-8 BOM
    let text = String::from_utf8_lossy(&raw);
    let text = text.trim_start_matches('\u{FEFF}');

    let mut bad: Vec<u64> = Vec::new();

    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Пропускаем заголовок
        if lineno == 0 && line.starts_with("offset_mb") {
            continue;
        }

        // Разделители: запятая, точка с запятой, таб
        let parts: Vec<&str> = line.split([',', ';', '\t']).map(|s| s.trim()).collect();

        if parts.len() < 4 {
            continue;
        }

        let offset_mb: u64 = match parts[0].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let class = parts[2];
        let io_status = parts[3];

        let is_bad = matches!(class, "Bad" | "Red") || io_status == "ERR";
        if !is_bad {
            continue;
        }

        // offset_mb → LBA. 1 МБ = 2048 секторов по 512 байт.
        let lba = offset_mb.saturating_mul(2048);
        bad.push(lba);
    }

    // Дедупликация с сохранением порядка
    bad.sort_unstable();
    bad.dedup();
    Ok(bad)
}

// ============================================================
// Вспомогательные
// ============================================================

fn require_yes(yes: bool) -> Result<()> {
    if !yes {
        bail!(
            "Отказано: remap необратим. Передайте --yes для подтверждения.\n\
             \n\
             ⚠ После remap данные на указанных LBA будут потеряны.\n\
             ⚠ Убедитесь, что важные данные уже скопированы с диска."
        );
    }
    Ok(())
}

fn identify_device_info(device: &Path) -> Result<IdentifyInfo> {
    identify(device).context("Не удалось прочитать IDENTIFY DEVICE")
}

fn validate_range(info: &IdentifyInfo, lba: u64, count: u64) -> Result<()> {
    let max_lba = info.max_lba();
    if max_lba == 0 {
        bail!("Диск вернул нулевой размер в IDENTIFY DEVICE");
    }
    if lba >= max_lba {
        bail!("LBA {} вне диапазона диска (max = {})", lba, max_lba - 1);
    }
    let end = lba.checked_add(count).context("Overflow: lba + count")?;
    if end > max_lba {
        bail!(
            "Диапазон LBA {}..{} выходит за пределы диска (max = {})",
            lba,
            end - 1,
            max_lba - 1
        );
    }
    Ok(())
}

fn warn_if_ssd(info: &IdentifyInfo) {
    if info.is_ssd {
        eprintln!(
            "[!] Внимание: диск — SSD. Spare area может быть ограничена,\n\
             [!] remap не всегда проходит — возможен ILLEGAL REQUEST."
        );
    }
}

fn install_ctrlc_handler() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        let _ = ctrlc::set_handler(move || s.store(true, Ordering::SeqCst));
    }
    stop
}

// ============================================================
// Тесты
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn temp_log(name: &str, content: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ticros_remap_{}_{}.log", name, std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        drop(f);
        p
    }

    #[test]
    fn test_parse_bad_lbas_basic() {
        let path = temp_log(
            "basic",
            "offset_mb,elapsed_ms,class,io_status\n\
             0,0.5,Grey,OK\n\
             10,45.0,Green,OK\n\
             100,2000.0,Bad,OK\n\
             200,150.0,Red,OK\n\
             300,500.0,Yellow,OK\n",
        );

        let bad = parse_bad_lbas_from_log(&path).unwrap();
        assert_eq!(bad.len(), 2);
        assert!(bad.contains(&(100 * 2048)));
        assert!(bad.contains(&(200 * 2048)));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_bad_lbas_io_error() {
        let path = temp_log(
            "io_err",
            "offset_mb,elapsed_ms,class,io_status\n\
             0,0.5,Grey,OK\n\
             10,5000.0,Grey,ERR\n\
             20,3.0,Green,OK\n",
        );

        let bad = parse_bad_lbas_from_log(&path).unwrap();
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0], 10 * 2048);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_bad_lbas_dedup() {
        let path = temp_log(
            "dedup",
            "offset_mb,elapsed_ms,class,io_status\n\
             100,2000.0,Bad,OK\n\
             100,2000.0,Bad,OK\n\
             100,2000.0,Bad,OK\n",
        );

        let bad = parse_bad_lbas_from_log(&path).unwrap();
        assert_eq!(bad.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_bad_lbas_empty() {
        let path = temp_log(
            "empty",
            "offset_mb,elapsed_ms,class,io_status\n\
             0,0.5,Grey,OK\n\
             10,3.0,Green,OK\n",
        );

        let bad = parse_bad_lbas_from_log(&path).unwrap();
        assert!(bad.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_bad_lbas_comments() {
        let path = temp_log(
            "comments",
            "offset_mb,elapsed_ms,class,io_status\n\
             # comment line\n\
             100,2000.0,Bad,OK\n\
             \n",
        );

        let bad = parse_bad_lbas_from_log(&path).unwrap();
        assert_eq!(bad.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_bad_lbas_semicolon() {
        let path = temp_log(
            "semicolon",
            "offset_mb;elapsed_ms;class;io_status\n\
             100;2000.0;Bad;OK\n",
        );

        let bad = parse_bad_lbas_from_log(&path).unwrap();
        assert_eq!(bad.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_validate_range_ok() {
        let info = IdentifyInfo {
            model: "T".into(),
            serial: "S".into(),
            firmware: "F".into(),
            lba28_sectors: 0,
            lba48_sectors: 1000,
            rotation_rpm: None,
            is_ssd: true,
            smart_supported: true,
            trim_supported: true,
            form_factor: None,
            interface: "SATA",
            ncq_supported: true,
        };
        assert!(validate_range(&info, 100, 50).is_ok());
    }

    #[test]
    fn test_validate_range_out_of_bounds() {
        let info = IdentifyInfo {
            model: "T".into(),
            serial: "S".into(),
            firmware: "F".into(),
            lba28_sectors: 0,
            lba48_sectors: 1000,
            rotation_rpm: None,
            is_ssd: true,
            smart_supported: true,
            trim_supported: true,
            form_factor: None,
            interface: "SATA",
            ncq_supported: true,
        };
        assert!(validate_range(&info, 999, 10).is_err());
        assert!(validate_range(&info, 2000, 1).is_err());
    }
}
