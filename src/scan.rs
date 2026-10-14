//! Сканирование поверхности с O_DIRECT и классификацией блоков.
//! Режим визуальной карты — как в DOS Victoria.
//! Возвращает ClassCounts и метрики в main для печати через ui.

use crate::log::ScanLog;
use crate::thresholds::{BlockClass, ClassCounts, Thresholds};
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
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

/// Результат сканирования — для печати в ui::print_scan_summary.
pub struct ScanResult {
    pub counts: ClassCounts,
    pub bytes_done: u64,
    pub elapsed: Duration,
    pub interrupted: bool,
    pub log_path: PathBuf,
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

    // 7. Лог
    let mut log = ScanLog::create(&cfg.log_path)
        .with_context(|| format!("Не удалось создать лог {:?}", cfg.log_path))?;

    // 8. Буфер, выровненный на 4096 (O_DIRECT)
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

    // 9. Главный цикл
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

        log.write(off_mb, dt.as_secs_f64() * 1000.0, class.as_str(), io_ok)?;

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

    Ok(ScanResult {
        counts,
        bytes_done,
        elapsed: t_start.elapsed(),
        interrupted,
        log_path: cfg.log_path,
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
///
/// Clippy-fix: `div_ceil` вместо `(size + align - 1) / align * align`.
/// `div_ceil` для `usize` стабилен с Rust 1.73.
fn align_direct(size: usize) -> usize {
    size.div_ceil(DIRECT_ALIGN) * DIRECT_ALIGN
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
