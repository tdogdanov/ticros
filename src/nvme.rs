//! NVMe доступ через NVME_IOCTL_ADMIN_CMD (Linux).
//!
//! Работает с /dev/nvme0, /dev/nvme0n1.
//! Реализует:
//!   - Identify Controller (CNS=0x01)
//!   - Identify Namespace (CNS=0x00)
//!   - Get Log Page — SMART/Health Information (Log ID=0x02)
//!
//! ⚠ ioctl request: `as libc::c_int` работает и на glibc, и на musl.
//! Значение NVME_IOCTL_ADMIN_CMD (0xC0484E41) НЕ влезает в i32 как positive,
//! поэтому приводим через `as i32` (с сохранением битового представления).

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::path::Path;

// ============================================================
// Константы NVMe
// ============================================================

/// ioctl код NVME_IOCTL_ADMIN_CMD: _IOWR('N', 0x41, struct nvme_admin_cmd)
/// = 0xC0484E41 — битовое представление (unsigned).
/// Для ioctl нужно signed i32 — приведение через `as libc::c_int` сохраняет биты.
const NVME_IOCTL_ADMIN_CMD: u64 = 0xC0484E41;

/// CNS — Controller or Namespace Structure
const NVME_ID_CNS_NS: u32 = 0x00;
const NVME_ID_CNS_CTRL: u32 = 0x01;

/// Get Log Page — Log ID
const NVME_LOG_SMART: u32 = 0x02;

/// Admin opcodes
const NVME_ADMIN_IDENTIFY: u8 = 0x06;
const NVME_ADMIN_GET_LOG_PAGE: u8 = 0x02;

// ============================================================
// NVMe Admin Command (linux/nvme_ioctl.h)
// ============================================================
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct NvmeAdminCmd {
    pub opcode: u8,
    pub flags: u8,
    pub rsvd1: u16,
    pub nsid: u32,
    pub cdw2: u32,
    pub cdw3: u32,
    pub metadata: u64,
    pub addr: u64,
    pub metadata_len: u32,
    pub data_len: u32,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
    pub timeout_ms: u32,
    pub result: u32,
}

// ============================================================
// Устройство
// ============================================================
pub struct NvmeDevice {
    file: std::fs::File,
    path: std::path::PathBuf,
}

impl NvmeDevice {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("Не удалось открыть {:?}", path))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Базовый admin-запрос.
    fn admin_cmd(&self, mut cmd: NvmeAdminCmd, buf: &mut [u8]) -> Result<()> {
        cmd.addr = buf.as_mut_ptr() as u64;
        cmd.data_len = buf.len() as u32;

        // ⚠ ioctl request: `as libc::c_int` для совместимости glibc/musl.
        // 0xC0484E41 как i32 = отрицательное число, но биты сохраняются.
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                NVME_IOCTL_ADMIN_CMD as libc::c_int,
                &cmd,
            )
        };
        if ret < 0 {
            bail!(
                "ioctl(NVME_IOCTL_ADMIN_CMD) на {:?}: {}",
                self.path,
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    /// Identify Controller (CNS=0x01) → 4096 байт.
    pub fn identify_controller(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 4096];
        let cmd = NvmeAdminCmd {
            opcode: NVME_ADMIN_IDENTIFY,
            nsid: 0,
            cdw10: NVME_ID_CNS_CTRL,
            ..Default::default()
        };
        self.admin_cmd(cmd, &mut buf)?;
        Ok(buf)
    }

    /// Identify Namespace (CNS=0x00) для конкретного NSID → 4096 байт.
    pub fn identify_namespace(&self, nsid: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 4096];
        let cmd = NvmeAdminCmd {
            opcode: NVME_ADMIN_IDENTIFY,
            nsid,
            cdw10: NVME_ID_CNS_NS,
            ..Default::default()
        };
        self.admin_cmd(cmd, &mut buf)?;
        Ok(buf)
    }

    /// SMART / Health Information Log (Log ID=0x02) → 512 байт.
    pub fn smart_log(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 512];
        let cmd = NvmeAdminCmd {
            opcode: NVME_ADMIN_GET_LOG_PAGE,
            nsid: 0xFFFFFFFF, // 0xFFFFFFFF = контроллер
            // cdw10: LOG_ID (bits 7:0) | NUMD (bits 31:16) — NUMD в dwords минус 1
            cdw10: (NVME_LOG_SMART & 0xFF) | (((512u32 / 4) - 1) << 16),
            cdw11: 0,
            ..Default::default()
        };
        self.admin_cmd(cmd, &mut buf)?;
        Ok(buf)
    }
}

// ============================================================
// Разбор Identify Controller
// ============================================================

#[derive(Debug, Clone)]
pub struct NvmeController {
    pub vid: u16,
    pub ssvid: u16,
    pub serial: String,
    pub model: String,
    pub firmware: String,
    pub rab: u8,
    pub oacs: u16,
    pub max_ns: u32,
}

impl NvmeController {
    pub fn vendor_id_string(&self) -> String {
        format!("{:04x}:{:04x}", self.vid, self.ssvid)
    }
}

pub fn parse_identify_controller(data: &[u8]) -> Result<NvmeController> {
    if data.len() < 4096 {
        bail!("Identify Controller: data too short ({} < 4096)", data.len());
    }

    let vid = u16::from_le_bytes([data[0], data[1]]);
    let ssvid = u16::from_le_bytes([data[2], data[3]]);
    let serial = nvme_string(&data[4..24]);
    let model = nvme_string(&data[24..64]);
    let firmware = nvme_string(&data[64..72]);
    let rab = data[72];
    let oacs = u16::from_le_bytes([data[256], data[257]]);
    let max_ns = u32::from_le_bytes([data[516], data[517], data[518], data[519]]);

    Ok(NvmeController {
        vid,
        ssvid,
        serial,
        model,
        firmware,
        rab,
        oacs,
        max_ns,
    })
}

// ============================================================
// Разбор Identify Namespace
// ============================================================

#[derive(Debug, Clone)]
pub struct NvmeNamespace {
    pub nsid: u32,
    pub nsze: u64,
    pub ncap: u64,
    pub nuse: u64,
    pub lba_format: u8,
    pub lba_size: u32,
}

impl NvmeNamespace {
    pub fn size_bytes(&self) -> u64 {
        self.nsze.saturating_mul(self.lba_size as u64)
    }
}

pub fn parse_identify_namespace(data: &[u8], nsid: u32) -> Result<NvmeNamespace> {
    if data.len() < 4096 {
        bail!("Identify Namespace: data too short");
    }

    let nsze = u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    let ncap = u64::from_le_bytes([
        data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
    ]);
    let nuse = u64::from_le_bytes([
        data[16], data[17], data[18], data[19], data[20], data[21], data[22], data[23],
    ]);
    let lba_format = data[24];

    // Word 26 (offset 104..108): LBA Format — Dword 26 в спецификации
    // Биты 23:16 = LBADS (log2 размера LBA)
    let lbaf_dw = u32::from_le_bytes([data[104], data[105], data[106], data[107]]);
    // lbaf_dw уже u32, `as u32` избыточен
    let lbads = (lbaf_dw >> 16) & 0xFF;
    // LBADS не может быть больше 31 — защита от мусора
    let lba_size = if lbads < 32 { 1u32 << lbads } else { 512 };

    Ok(NvmeNamespace {
        nsid,
        nsze,
        ncap,
        nuse,
        lba_format,
        lba_size,
    })
}

// ============================================================
// Разбор SMART / Health Log
// ============================================================

#[derive(Debug, Clone)]
pub struct NvmeSmart {
    pub critical_warning: u8,
    pub temperature_k: u16,
    pub available_spare: u8,
    pub spare_thresh: u8,
    pub percent_used: u8,
    pub data_units_read: u128,
    pub data_units_written: u128,
    pub host_read_commands: u128,
    pub host_write_commands: u128,
    pub controller_busy_time: u128,
    pub power_cycles: u128,
    pub power_on_hours: u128,
    pub unsafe_shutdowns: u128,
    pub media_errors: u128,
    pub num_err_log_entries: u128,
    pub warning_temp_time: u32,
    pub critical_comp_time: u32,
}

impl NvmeSmart {
    pub fn temperature_celsius(&self) -> i16 {
        (self.temperature_k as i32 - 273) as i16
    }

    pub fn health_ok(&self) -> bool {
        self.critical_warning == 0
    }

    /// Прочитано в байтах: data_units_read × 1000 × 512
    pub fn bytes_read(&self) -> u64 {
        let units = self.data_units_read.min(u64::MAX as u128) as u64;
        units.saturating_mul(512_000)
    }

    /// Записано в байтах.
    pub fn bytes_written(&self) -> u64 {
        let units = self.data_units_written.min(u64::MAX as u128) as u64;
        units.saturating_mul(512_000)
    }
}

pub fn parse_smart_log(data: &[u8]) -> Result<NvmeSmart> {
    if data.len() < 512 {
        bail!("SMART Log: data too short ({} < 512)", data.len());
    }

    let critical_warning = data[0];
    let temperature_k = u16::from_le_bytes([data[1], data[2]]);
    let available_spare = data[3];
    let spare_thresh = data[4];
    let percent_used = data[5];

    // 128-битные поля, LE, по 16 байт начиная с offset 32
    let data_units_read = read_u128(data, 32);
    let data_units_written = read_u128(data, 48);
    let host_read_commands = read_u128(data, 64);
    let host_write_commands = read_u128(data, 80);
    let controller_busy_time = read_u128(data, 96);
    let power_cycles = read_u128(data, 112);
    let power_on_hours = read_u128(data, 128);
    let unsafe_shutdowns = read_u128(data, 144);
    let media_errors = read_u128(data, 160);
    let num_err_log_entries = read_u128(data, 176);

    let warning_temp_time = u32::from_le_bytes([data[192], data[193], data[194], data[195]]);
    let critical_comp_time = u32::from_le_bytes([data[196], data[197], data[198], data[199]]);

    Ok(NvmeSmart {
        critical_warning,
        temperature_k,
        available_spare,
        spare_thresh,
        percent_used,
        data_units_read,
        data_units_written,
        host_read_commands,
        host_write_commands,
        controller_busy_time,
        power_cycles,
        power_on_hours,
        unsafe_shutdowns,
        media_errors,
        num_err_log_entries,
        warning_temp_time,
        critical_comp_time,
    })
}

// ============================================================
// Утилиты
// ============================================================

/// Прочитать u128 LE из data[offset..offset+16].
fn read_u128(data: &[u8], offset: usize) -> u128 {
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&data[offset..offset + 16]);
    u128::from_le_bytes(buf)
}

/// NVMe строки — ASCII, без swap байт.
fn nvme_string(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if b == 0 {
            break;
        }
        if (0x20..0x7F).contains(&b) {
            s.push(b as char);
        }
    }
    s.trim().to_string()
}

/// Определить, что путь — NVMe-устройство.
pub fn is_nvme_path(path: &Path) -> bool {
    path.to_string_lossy().contains("nvme")
}

// ============================================================
// Тесты
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nvme_string() {
        let bytes = b"Samsung SSD 970\0\0\0";
        assert_eq!(nvme_string(bytes), "Samsung SSD 970");
    }

    #[test]
    fn test_nvme_string_empty() {
        assert_eq!(nvme_string(&[0u8; 10]), "");
    }

    #[test]
    fn test_is_nvme_path() {
        assert!(is_nvme_path(Path::new("/dev/nvme0n1")));
        assert!(is_nvme_path(Path::new("/dev/nvme0")));
        assert!(!is_nvme_path(Path::new("/dev/sda")));
    }

    #[test]
    fn test_read_u128() {
        let mut data = [0u8; 32];
        data[0] = 1;
        data[15] = 0x80;
        let v = read_u128(&data, 0);
        assert_eq!(v, 1 + ((0x80u128) << 120));
    }

    #[test]
    fn test_parse_identify_controller_short() {
        let data = vec![0u8; 100];
        assert!(parse_identify_controller(&data).is_err());
    }

    #[test]
    fn test_parse_smart_log_short() {
        let data = vec![0u8; 100];
        assert!(parse_smart_log(&data).is_err());
    }

    #[test]
    fn test_parse_smart_log_valid() {
        let mut data = [0u8; 512];
        data[0] = 0x00;
        data[1] = 0x12; data[2] = 0x01;
        data[3] = 0x64;
        data[4] = 0x0A;
        data[5] = 0x05;
        data[32] = 0xE8; data[33] = 0x03;
        data[128] = 0x64;

        let s = parse_smart_log(&data).unwrap();
        assert_eq!(s.critical_warning, 0);
        assert!(s.health_ok());
        assert_eq!(s.available_spare, 100);
        assert_eq!(s.percent_used, 5);
        assert_eq!(s.data_units_read, 1000);
        assert_eq!(s.power_on_hours, 100);
    }

    #[test]
    fn test_temperature_celsius() {
        let mut s = NvmeSmart {
            critical_warning: 0,
            temperature_k: 300,
            available_spare: 100,
            spare_thresh: 10,
            percent_used: 5,
            data_units_read: 0,
            data_units_written: 0,
            host_read_commands: 0,
            host_write_commands: 0,
            controller_busy_time: 0,
            power_cycles: 0,
            power_on_hours: 0,
            unsafe_shutdowns: 0,
            media_errors: 0,
            num_err_log_entries: 0,
            warning_temp_time: 0,
            critical_comp_time: 0,
        };
        assert_eq!(s.temperature_celsius(), 27);
        s.critical_warning = 0x04;
        assert!(!s.health_ok());
    }
}