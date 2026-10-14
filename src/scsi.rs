//! SCSI-команды для USB-мостов (USB Mass Storage, UAS).
//!
//! USB-мосты не пропускают ATA PASS-THROUGH (0x85).
//! Вместо этого используется SCSI-транспорт:
//!   - INQUIRY (0x12) — паспорт устройства
//!   - READ CAPACITY (10) (0x25) — размер до 2 ТБ
//!   - READ CAPACITY (16) (0x9E) — размер для больших дисков
//!   - READ(10) (0x28) — чтение до 2^32 LBA
//!   - READ(16) (0x88) — чтение больших дисков
//!   - TEST UNIT READY (0x00) — проверка готовности
//!
//! Все команды через SG_IO — те же структуры, что в `ata.rs`.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::ata::{format_sense, SgIoHdr, SG_DXFER_FROM_DEV, SG_IO};

/// Размер сектора по умолчанию (если READ CAPACITY вернул 0).
pub const SECTOR_SIZE_DEFAULT: u32 = 512;

// ============================================================
// SCSI opcodes
// ============================================================
pub const SCSI_TEST_UNIT_READY: u8 = 0x00;
pub const SCSI_REQUEST_SENSE: u8 = 0x03;
pub const SCSI_INQUIRY: u8 = 0x12;
pub const SCSI_READ_CAPACITY_10: u8 = 0x25;
pub const SCSI_READ_10: u8 = 0x28;
pub const SCSI_READ_16: u8 = 0x88;
pub const SCSI_READ_CAPACITY_16: u8 = 0x9E;

// ============================================================
// Устройство
// ============================================================
pub struct ScsiDevice {
    file: std::fs::File,
    path: std::path::PathBuf,
}

impl ScsiDevice {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| format!("Не удалось открыть SCSI {:?}", path))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Базовый SG_IO с произвольным CDB (6/10/12/16 байт).
    ///
    /// Возвращает буфер длиной `buf_size`.
    /// Для команд без данных (`TEST UNIT READY`) — `buf_size = 0`.
    pub fn sg_io(
        &self,
        cdb: &[u8],
        direction: i32,
        buf_size: usize,
        timeout_ms: u32,
    ) -> Result<Vec<u8>> {
        let mut sense = [0u8; 32];
        let mut buf = vec![0u8; buf_size];

        let mut hdr = SgIoHdr {
            interface_id: b'S' as i32,
            dxfer_direction: direction,
            cmd_len: cdb.len() as u8,
            mx_sb_len: sense.len() as u8,
            dxfer_len: buf_size as u32,
            dxferp: if buf_size > 0 {
                buf.as_mut_ptr()
            } else {
                std::ptr::null_mut()
            },
            cmdp: cdb.as_ptr() as *mut u8,
            sbp: sense.as_mut_ptr(),
            timeout: timeout_ms,
            ..Default::default()
        };

        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO as _, &mut hdr) };
        if ret < 0 {
            bail!("ioctl(SG_IO): {}", std::io::Error::last_os_error());
        }
        if hdr.status != 0 || hdr.host_status != 0 || hdr.driver_status != 0 {
            bail!(
                "SCSI error на {:?}: status={:#x} host={:#x} driver={:#x} resid={}\n{}",
                self.path,
                hdr.status,
                hdr.host_status,
                hdr.driver_status,
                hdr.resid,
                format_sense(&sense, hdr.sb_len_wr as usize)
            );
        }
        if hdr.resid > 0 && buf_size > 0 {
            bail!(
                "SG_IO: непередано {} из {} байт (resid={})",
                hdr.resid,
                buf_size,
                hdr.resid
            );
        }

        Ok(buf)
    }

    /// SCSI INQUIRY (0x12) — паспорт устройства (36 байт).
    pub fn inquiry(&self) -> Result<InquiryData> {
        let mut cdb = [0u8; 6];
        cdb[0] = SCSI_INQUIRY;
        cdb[4] = 36;

        let buf = self.sg_io(&cdb, SG_DXFER_FROM_DEV, 36, 10_000)?;
        parse_inquiry(&buf)
    }

    /// READ CAPACITY (10) — размер до 2 ТБ.
    pub fn read_capacity_10(&self) -> Result<Capacity> {
        let mut cdb = [0u8; 10];
        cdb[0] = SCSI_READ_CAPACITY_10;

        let buf = self.sg_io(&cdb, SG_DXFER_FROM_DEV, 8, 10_000)?;
        parse_capacity_10(&buf)
    }

    /// READ CAPACITY (16) — размер больших дисков (>2 ТБ).
    pub fn read_capacity_16(&self) -> Result<Capacity> {
        let mut cdb = [0u8; 16];
        cdb[0] = SCSI_READ_CAPACITY_16;
        cdb[13] = 32;

        let buf = self.sg_io(&cdb, SG_DXFER_FROM_DEV, 32, 10_000)?;
        parse_capacity_16(&buf)
    }

    /// Универсальный READ: выбирает READ(10) или READ(16) по LBA.
    pub fn read_big(&self, lba: u64, blocks: u32, block_size: u32) -> Result<Vec<u8>> {
        let total_bytes = blocks as u64 * block_size as u64;
        if total_bytes > 64 * 1024 * 1024 {
            bail!(
                "read_big: запрошено слишком много данных ({} МБ). Максимум — 64 МБ.",
                total_bytes / 1024 / 1024
            );
        }

        if lba < u32::MAX as u64 && blocks <= u16::MAX as u32 {
            self.read_10(lba as u32, blocks as u16, block_size)
        } else {
            self.read_16(lba, blocks, block_size)
        }
    }

    /// READ(10).
    pub fn read_10(&self, lba: u32, blocks: u16, block_size: u32) -> Result<Vec<u8>> {
        let mut cdb = [0u8; 10];
        cdb[0] = SCSI_READ_10;
        cdb[2] = (lba >> 24) as u8;
        cdb[3] = (lba >> 16) as u8;
        cdb[4] = (lba >> 8) as u8;
        cdb[5] = lba as u8;
        cdb[7] = (blocks >> 8) as u8;
        cdb[8] = blocks as u8;

        let total = (blocks as u32) * block_size;
        self.sg_io(&cdb, SG_DXFER_FROM_DEV, total as usize, 30_000)
    }

    /// READ(16).
    pub fn read_16(&self, lba: u64, blocks: u32, block_size: u32) -> Result<Vec<u8>> {
        let mut cdb = [0u8; 16];
        cdb[0] = SCSI_READ_16;
        cdb[2] = (lba >> 56) as u8;
        cdb[3] = (lba >> 48) as u8;
        cdb[4] = (lba >> 40) as u8;
        cdb[5] = (lba >> 32) as u8;
        cdb[6] = (lba >> 24) as u8;
        cdb[7] = (lba >> 16) as u8;
        cdb[8] = (lba >> 8) as u8;
        cdb[9] = lba as u8;
        cdb[10] = (blocks >> 24) as u8;
        cdb[11] = (blocks >> 16) as u8;
        cdb[12] = (blocks >> 8) as u8;
        cdb[13] = blocks as u8;

        let total = blocks as u64 * block_size as u64;
        self.sg_io(&cdb, SG_DXFER_FROM_DEV, total as usize, 30_000)
    }

    /// TEST UNIT READY (0x00).
    pub fn test_unit_ready(&self) -> Result<()> {
        let cdb = [SCSI_TEST_UNIT_READY, 0, 0, 0, 0, 0];
        self.sg_io(&cdb, -1, 0, 5_000)?;
        Ok(())
    }
}

// ============================================================
// Данные
// ============================================================

#[derive(Debug, Clone)]
pub struct InquiryData {
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub device_type: u8,
    pub is_removable: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Capacity {
    pub total_sectors: u64,
    pub sector_size: u32,
}

impl Capacity {
    pub fn size_bytes(&self) -> u64 {
        self.total_sectors.saturating_mul(self.sector_size as u64)
    }

    pub fn size_gb(&self) -> f64 {
        self.size_bytes() as f64 / 1_000_000_000.0
    }

    pub fn size_gib(&self) -> f64 {
        self.size_bytes() as f64 / (1024.0 * 1024.0 * 1024.0)
    }
}

// ============================================================
// Парсинг
// ============================================================

pub fn parse_inquiry(buf: &[u8]) -> Result<InquiryData> {
    if buf.len() < 36 {
        bail!("INQUIRY: короткий ответ ({} < 36)", buf.len());
    }

    let device_type = buf[0] & 0x1F;
    let is_removable = (buf[1] & 0x80) != 0;
    let vendor = scsi_string(&buf[8..16]);
    let product = scsi_string(&buf[16..32]);
    let revision = scsi_string(&buf[32..36]);

    Ok(InquiryData {
        vendor,
        product,
        revision,
        device_type,
        is_removable,
    })
}

pub fn parse_capacity_10(buf: &[u8]) -> Result<Capacity> {
    if buf.len() < 8 {
        bail!("READ CAPACITY (10): короткий ответ ({} < 8)", buf.len());
    }

    let last_lba = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let block_size = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

    if block_size == 0 {
        bail!("READ CAPACITY (10): block_size = 0");
    }

    Ok(Capacity {
        total_sectors: last_lba as u64 + 1,
        sector_size: block_size,
    })
}

pub fn parse_capacity_16(buf: &[u8]) -> Result<Capacity> {
    if buf.len() < 32 {
        bail!("READ CAPACITY (16): короткий ответ ({} < 32)", buf.len());
    }

    let last_lba = u64::from_be_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    let block_size = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);

    if block_size == 0 {
        bail!("READ CAPACITY (16): block_size = 0");
    }

    Ok(Capacity {
        total_sectors: last_lba + 1,
        sector_size: block_size,
    })
}

fn scsi_string(bytes: &[u8]) -> String {
    let s: String = bytes
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as char)
        .collect();
    s.trim().to_string()
}

// ============================================================
// Определение USB
// ============================================================

/// Проверить, что устройство находится за USB-мостом.
pub fn is_usb_device(device: &Path) -> bool {
    let name = match device.file_name() {
        Some(n) => n.to_string_lossy().to_string(),
        None => return false,
    };

    let paths = [
        format!("/sys/block/{}/device/../../../transport", name),
        format!("/sys/block/{}/../../../../transport", name),
        format!("/sys/block/{}/device/../../transport", name),
    ];

    for p in &paths {
        if let Ok(s) = std::fs::read_to_string(p) {
            if s.trim() == "usb" {
                return true;
            }
        }
    }

    let removable = format!("/sys/block/{}/removable", name);
    if let Ok(s) = std::fs::read_to_string(&removable) {
        if s.trim() == "1" {
            return true;
        }
    }

    let block_link = format!("/sys/block/{}", name);
    if let Ok(real) = std::fs::read_link(&block_link) {
        if real.to_string_lossy().contains("/usb") {
            return true;
        }
    }

    false
}

// ============================================================
// Тесты
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_inquiry() {
        let mut buf = [0u8; 36];
        buf[0] = 0x00;
        buf[1] = 0x80;
        buf[8..16].copy_from_slice(b"ADATA   ");
        buf[16..32].copy_from_slice(b"SU650           ");
        buf[32..36].copy_from_slice(b"1.00");

        let inq = parse_inquiry(&buf).unwrap();
        assert_eq!(inq.vendor, "ADATA");
        assert_eq!(inq.product, "SU650");
        assert_eq!(inq.revision, "1.00");
        assert!(inq.is_removable);
    }

    #[test]
    fn test_parse_inquiry_short() {
        let buf = [0u8; 10];
        assert!(parse_inquiry(&buf).is_err());
    }

    #[test]
    fn test_parse_capacity_10() {
        let mut buf = [0u8; 8];
        buf[0..4].copy_from_slice(&234_441_647u32.to_be_bytes());
        buf[4..8].copy_from_slice(&512u32.to_be_bytes());

        let cap = parse_capacity_10(&buf).unwrap();
        assert_eq!(cap.total_sectors, 234_441_648);
        assert_eq!(cap.sector_size, 512);
    }

    #[test]
    fn test_parse_capacity_16() {
        let last_lba: u64 = 8_000_000_000;
        let mut buf = [0u8; 32];
        buf[0..8].copy_from_slice(&last_lba.to_be_bytes());
        buf[8..12].copy_from_slice(&4096u32.to_be_bytes());

        let cap = parse_capacity_16(&buf).unwrap();
        assert_eq!(cap.total_sectors, 8_000_000_001);
        assert_eq!(cap.sector_size, 4096);
    }

    #[test]
    fn test_scsi_string() {
        let bytes = b"ADATA   \0\0\0";
        assert_eq!(scsi_string(bytes), "ADATA");
    }

    #[test]
    fn test_capacity_size_bytes() {
        let c = Capacity {
            total_sectors: 1_000_000,
            sector_size: 512,
        };
        assert_eq!(c.size_bytes(), 512_000_000);
    }

    #[test]
    fn test_is_usb_device_nonexistent() {
        assert!(!is_usb_device(Path::new("/dev/sdzzz")));
    }
}
