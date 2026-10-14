//! Сканирование поверхности с O_DIRECT и классификацией блоков.
//!
//! Режим визуальной карты — как в DOS Victoria.
//!
//! Возвращает ClassCounts и метрики в main для печати через ui.
//!
//! Особенности:
//!   - O_DIRECT для чтения без кэша (475 МБ/с на SATA SSD).
//!   - Fallback на обычный read при EINVAL.
//!   - Классификация по порогам (Victoria / MHDD / fast / strict).
//!   - Лог CSV: offset_mb, elapsed_ms, class, io_status.
//!   - Дополнительный лог `<log>.bad` — только плохие блоки.

use crate::log::ScanLog;
use crate::thresholds::{BlockClass, ClassCounts, Thresholds};
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DIRECT_ALIGN: usize = 4096;
const SECTOR_SIZE: usize = 512;

/// Блоков в одной строке карты (для --map).
const MAP_LINE_WIDTH: usize = 60;

// ANSI для карты
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

// ============================================================
// Конфиг и результат
// ============================================================

pub struct ScanConfig {
    pub device: PathBuf,
    pub start_mb: u64,
    pub end_mb: Option<u64>,
    pub block_kb: usize,
    pub log_path: PathBuf,
    pub stop_on_bad: bool,
    pub thresholds: Thresholds,
    /// Показать визуальную карту блоков (DOS Victoria-style).
    pub show_map: bool,
}

/// Результат сканирования.
pub struct ScanResult {
    pub counts: ClassCounts,
    pub bytes_done: u64,
    pub elapsed: Duration,
    pub interrupted: bool,
    pub log_path: PathBuf,
    /// Путь к логу с плохими блоками (только Bad/Red).
    pub bad_log_path: PathBuf,
}

#[allow(dead_code)]
impl ScanResult {
    /// Средняя скорость в МБ/с.
    pub fn speed_mbps(&self) -> f64 {
        self.bytes_done as f64 / 1_048_576.0 / self.elapsed.as_secs_f64().max(0.001)
    }

    /// Всего блоков.
    pub fn total_blocks(&self) -> u64 {
        self.counts.total()
    }

    /// Проблемных блоков (Red + Bad).
    pub fn problem_blocks(&self) -> u64 {
        self.counts.red + self.counts.bad
    }
}

// ============================================================
// Точка входа
// ============================================================

pub fn run(cfg: ScanConfig) -> Result<ScanResult> {
    // 1. Валидация порогов
    cfg.thresholds
        .validate()
        .map_err(|e| anyhow::anyhow!("Невалидные пороги: {}", e))?;

    // 2. Открываем диск
    let mut file = open_device(&cfg.device)?;

    let total_bytes = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;

    if total_bytes == 0 {
        anyhow::bail!("Диск {:?} сообщает нулевой размер", cfg.device);
    }

    // 3. Диапазон
    let start_off = cfg.start_mb * 1024 * 1024;
    let end_off = cfg
        .end_mb
        .map(|m| m * 1024 * 1024)
        .unwrap_or(total_bytes)
        .min(total_bytes);

    if end_off <= start_off {
        anyhow::bail!(
            "Пустой диапазон: start={} МБ end={} МБ",
            cfg.start_mb,
            end_off >> 20
        );
    }

    let block_sz = align_direct(cfg.block_kb.max(4) * 1024);
    let start_off = (start_off / DIRECT_ALIGN as u64) * DIRECT_ALIGN as u64;
    let end_off = (end_off / DIRECT_ALIGN as u64) * DIRECT_ALIGN as u64;

    if end_off <= start_off {
        anyhow::bail!("Пустой диапазон после выравнивания");
    }

    let total_blocks = (end_off - start_off) / block_sz as u64;

    // 4. Заголовок
    println!("\n{BOLD}=== Сканирование {:?} ==={RESET}", cfg.device);
    println!("  Диапазон:   {} — {} МБ", start_off >> 20, end_off >> 20);
    println!("  Блок:       {} КБ", block_sz >> 10);
    println!("  Блоков:     {}", total_blocks);
    println!();

    // 5. Ctrl+C
    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        let _ = ctrlc::set_handler(move || s.store(true, Ordering::SeqCst));
    }

    // 6. Прогресс-бар (только если не карта)
    let pb = if cfg.show_map {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(total_blocks);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
            )?
            .progress_chars("█▉▊▋▌▍▎▏  "),
        );
        pb
    };

    // 7. Основной лог
    let mut log = ScanLog::create(&cfg.log_path)
        .with_context(|| format!("Не удалось создать лог {:?}", cfg.log_path))?;

    // 8. Дополнительный лог — только плохие блоки
    let bad_log_path = bad_log_path(&cfg.log_path);
    let mut bad_log = ScanLog::create(&bad_log_path)
        .with_context(|| format!("Не удалось создать bad-лог {:?}", bad_log_path))?;

    // 9. Буфер, выровненный на 4096 (O_DIRECT)
    let layout =
        std::alloc::Layout::from_size_align(block_sz, DIRECT_ALIGN).context("Невалидный Layout")?;
    let buf_ptr = unsafe { std::alloc::alloc(layout) };
    if buf_ptr.is_null() {
        anyhow::bail!("Не удалось выделить {} байт", block_sz);
    }
    let _guard = BufGuard {
        ptr: buf_ptr,
        layout,
    };

    file.seek(SeekFrom::Start(start_off))
        .with_context(|| format!("seek({})", start_off))?;

    // 10. Главный цикл
    let mut counts = ClassCounts::default();
    let t_start = Instant::now();
    let mut bytes_done: u64 = 0;
    let mut interrupted = false;

    // Состояние карты
    let mut line_lba: u64 = start_off / SECTOR_SIZE as u64;
    let mut line_col: usize = 0;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if cfg.show_map {
        writeln!(out, "  {BOLD}LBA        Карта{RESET}")?;
        write!(out, "  {:08x}   ", line_lba)?;
        out.flush()?;
    }

    for i in 0..total_blocks {
        if stop.load(Ordering::SeqCst) {
            interrupted = true;
            break;
        }

        let cur_offset = start_off + i * block_sz as u64;
        let off_mb = cur_offset >> 20;

        let slice = unsafe { std::slice::from_raw_parts_mut(buf_ptr, block_sz) };

        let t0 = Instant::now();
        let res = file.read(slice);
        let dt = t0.elapsed();

        // Успех = нет ошибки И прочитан полный блок
        let io_ok = match &res {
            Ok(n) => *n == block_sz,
            Err(_) => false,
        };

        let class = cfg.thresholds.classify(dt.as_secs_f64() * 1000.0, !io_ok);
        counts.add(class);

        let elapsed_ms = dt.as_secs_f64() * 1000.0;

        // Основной лог — все блоки
        log.write(off_mb, elapsed_ms, class.as_str(), io_ok)?;

        // Дополнительный лог — только плохие
        if class.is_problem() || !io_ok {
            bad_log.write(off_mb, elapsed_ms, class.as_str(), io_ok)?;
        }

        bytes_done += block_sz as u64;

        if cfg.show_map {
            let sym = class.symbol();
            let color = class.ansi_color();
            let reset = BlockClass::ansi_reset();
            write!(out, "{color}{sym}{reset}")?;
            line_col += 1;

            if line_col >= MAP_LINE_WIDTH {
                writeln!(out)?;
                line_lba += (MAP_LINE_WIDTH * block_sz) as u64 / SECTOR_SIZE as u64;
                write!(out, "  {:08x}   ", line_lba)?;
                line_col = 0;
            }
            out.flush()?;
        } else {
            let elapsed = t_start.elapsed().as_secs_f64().max(0.001);
            let speed = bytes_done as f64 / 1_048_576.0 / elapsed;
            pb.set_position(i + 1);
            pb.set_message(format!("{:.1} МБ/с", speed));
        }

        if cfg.stop_on_bad && class.is_critical() {
            if cfg.show_map {
                writeln!(out)?;
            } else {
                pb.abandon_with_message(format!(
                    "Остановка: {} на LBA ~{}",
                    class.as_str(),
                    cur_offset / SECTOR_SIZE as u64
                ));
            }
            interrupted = true;
            break;
        }
    }

    if cfg.show_map && line_col > 0 {
        writeln!(out)?;
    }
    if !cfg.show_map {
        pb.finish_and_clear();
    }

    log.flush()?;
    bad_log.flush()?;

    Ok(ScanResult {
        counts,
        bytes_done,
        elapsed: t_start.elapsed(),
        interrupted,
        log_path: cfg.log_path,
        bad_log_path,
    })
}

// ============================================================
// Вспомогательные
// ============================================================

/// Открыть диск: с O_DIRECT, при EINVAL — обычный режим.
fn open_device(path: &PathBuf) -> Result<std::fs::File> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
    {
        Ok(f) => Ok(f),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            eprintln!(
                "\x1b[93m[!] O_DIRECT не поддерживается для {:?}, работаем через кэш\x1b[0m",
                path
            );
            OpenOptions::new()
                .read(true)
                .open(path)
                .with_context(|| format!("Не удалось открыть {:?}", path))
        }
        Err(e) => Err(anyhow::Error::from(e).context(format!("Не удалось открыть {:?}", path))),
    }
}

/// O_DIRECT требует выравнивания на 4096.
fn align_direct(size: usize) -> usize {
    size.div_ceil(DIRECT_ALIGN) * DIRECT_ALIGN
}

/// Путь к логу с плохими блоками: `<log>.bad`.
fn bad_log_path(log_path: &Path) -> PathBuf {
    let mut p = log_path.to_path_buf();
    let ext = p
        .extension()
        .map(|e| format!("{}.bad", e.to_string_lossy()))
        .unwrap_or_else(|| "bad".to_string());
    p.set_extension(ext);
    p
}

// ============================================================
// RAII-обёртка для буфера
// ============================================================
struct BufGuard {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl Drop for BufGuard {
    fn drop(&mut self) {
        unsafe {
            std::alloc::dealloc(self.ptr, self.layout);
        }
    }
}

// Проверка на этапе компиляции
const _: () = assert!(DIRECT_ALIGN % SECTOR_SIZE == 0);

// ============================================================
// Тесты
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_direct_1k() {
        assert_eq!(align_direct(1024), 4096);
    }

    #[test]
    fn test_align_direct_4k() {
        assert_eq!(align_direct(4096), 4096);
    }

    #[test]
    fn test_align_direct_5k() {
        assert_eq!(align_direct(5120), 8192);
    }

    #[test]
    fn test_align_direct_0() {
        assert_eq!(align_direct(0), 0);
    }

    #[test]
    fn test_bad_log_path_with_ext() {
        let p = bad_log_path(Path::new("/tmp/scan.log"));
        assert_eq!(p, PathBuf::from("/tmp/scan.log.bad"));
    }

    #[test]
    fn test_bad_log_path_without_ext() {
        let p = bad_log_path(Path::new("/tmp/scan"));
        assert_eq!(p, PathBuf::from("/tmp/scan.bad"));
    }

    #[test]
    fn test_bad_log_path_csv() {
        let p = bad_log_path(Path::new("/tmp/scan.csv"));
        assert_eq!(p, PathBuf::from("/tmp/scan.csv.bad"));
    }

    #[test]
    fn test_scan_result_speed() {
        let r = ScanResult {
            counts: ClassCounts::default(),
            bytes_done: 1_048_576_000,
            elapsed: Duration::from_secs(2),
            interrupted: false,
            log_path: PathBuf::from("/tmp/x.log"),
            bad_log_path: PathBuf::from("/tmp/x.log.bad"),
        };
        assert!((r.speed_mbps() - 500.0).abs() < 0.1);
    }

    #[test]
    fn test_scan_result_problem_blocks() {
        let mut counts = ClassCounts::default();
        counts.red = 5;
        counts.bad = 3;
        let r = ScanResult {
            counts,
            bytes_done: 0,
            elapsed: Duration::from_secs(1),
            interrupted: false,
            log_path: PathBuf::from("/tmp/x.log"),
            bad_log_path: PathBuf::from("/tmp/x.log.bad"),
        };
        assert_eq!(r.problem_blocks(), 8);
    }
}
