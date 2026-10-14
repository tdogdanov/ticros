//! Низкоуровневый доступ к ATA-дискам через SG_IO (Linux).
//!
//! ⚠ Буферы для SG_IO выровнены на 512 байт (требование AHCI DMA).
//! Smartctl использует то же выравнивание.
//!
//! ⚠ HDIO_GET_IDENTITY возвращает struct hd_driveid, а не сырой ATA-ответ.
//! У него ДРУГИЕ смещения полей:
//!   - command_set_1 (SMART/TRIM): offset 164-165
//!   - sata_cap (SATA-интерфейс): offset 152-153 (word 76)
//!   - form_factor: offset 217-218 (может отсутствовать)
//!
//! SATA-интерфейс определяется по word 76 (sata_cap):
//! если поле != 0 — это SATA. cmd_set1 bit 15 для этого НЕ используется.
//!
//! ⚠ ioctl request: используем `as _` — компилятор сам подставит нужный
//! тип (c_ulong на glibc, c_int на musl). Это единственный переносимый способ.
//!
//! ⚠ Для USB-мостов ATA PASS-THROUGH обычно не работает.
//! См. модуль `scsi` — SCSI INQUIRY / READ CAPACITY / READ(10)/(16).

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::path::Path;

// ============================================================
// Константы SG_IO
// ============================================================
pub const SG_IO: u64 = 0x2285;
pub const SG_DXFER_NONE: i32 = -1;
pub const SG_DXFER_FROM_DEV: i32 = -3;
pub const SG_DXFER_TO_DEV: i32 = -2;

pub const SG_ALIGN: usize = 512;

// ============================================================
// ATA-команды
// ============================================================
pub const ATA_SMART: u8 = 0xB0;
pub const ATA_IDENTIFY: u8 = 0xEC;
pub const ATA_SET_FEATURES: u8 = 0xEF;

pub const SMART_READ_DATA: u8 = 0xD0;
pub const SMART_READ_THRESH: u8 = 0xD1;
pub const SMART_RETURN_STATUS: u8 = 0xDA;
pub const SMART_ENABLE: u8 = 0xD8;

pub const FEATURE_REASSIGN_SECTORS: u8 = 0x07;

pub const SMART_CYL_LOW: u8 = 0x4F;
pub const SMART_CYL_HI: u8 = 0xC2;

pub const PROTO_NON_DATA: u8 = 3;
pub const PROTO_PIO_IN: u8 = 4;
pub const PROTO_PIO_OUT: u8 = 5;
pub const PROTO_DMA_IN: u8 = 6;
pub const PROTO_DMA_OUT: u8 = 7;

pub const TL_NONE: u8 = 0;
pub const TL_28: u8 = 1;
pub const TL_48: u8 = 2;

// ============================================================
// SG_IO header
// ============================================================
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SgIoHdr {
    pub interface_id: i32,
    pub dxfer_direction: i32,
    pub cmd_len: u8,
    pub mx_sb_len: u8,
    pub iovec_count: u16,
    pub dxfer_len: u32,
    pub dxferp: *mut u8,
    pub cmdp: *mut u8,
    pub sbp: *mut u8,
    pub timeout: u32,
    pub flags: u32,
    pub pack_id: i32,
    pub usr_ptr: *mut u8,
    pub status: u8,
    pub masked_status: u8,
    pub msg_status: u8,
    pub sb_len_wr: u8,
    pub host_status: u16,
    pub driver_status: u16,
    pub resid: i32,
    pub duration: u32,
    pub info: u32,
}

// ============================================================
// Устройство
// ============================================================
pub struct AtaDevice {
    file: std::fs::File,
    path: std::path::PathBuf,
}

impl AtaDevice {
    pub fn open(path: &Path, write: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .open(path)
            .with_context(|| format!("Не удалось открыть {:?}", path))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn sg_io(
        &self,
        cdb: &[u8; 16],
        direction: i32,
        buf_size: usize,
        timeout_ms: u32,
    ) -> Result<Vec<u8>> {
        let alloc_size = if buf_size == 0 {
            SG_ALIGN
        } else {
            buf_size.div_ceil(SG_ALIGN) * SG_ALIGN
        };

        let mut data_ptr: *mut libc::c_void = std::ptr::null_mut();
        let ret = unsafe { libc::posix_memalign(&mut data_ptr, SG_ALIGN, alloc_size) };
        if ret != 0 {
            bail!(
                "posix_memalign(data, {}, {}) = {}",
                SG_ALIGN,
                alloc_size,
                ret
            );
        }
        unsafe {
            std::ptr::write_bytes(data_ptr as *mut u8, 0, alloc_size);
        }

        let mut sense_ptr: *mut libc::c_void = std::ptr::null_mut();
        let ret = unsafe { libc::posix_memalign(&mut sense_ptr, SG_ALIGN, 32) };
        if ret != 0 {
            unsafe {
                libc::free(data_ptr);
            }
            bail!("posix_memalign(sense) = {}", ret);
        }
        unsafe {
            std::ptr::write_bytes(sense_ptr as *mut u8, 0, 32);
        }

        let mut cdb_ptr: *mut libc::c_void = std::ptr::null_mut();
        let ret = unsafe { libc::posix_memalign(&mut cdb_ptr, SG_ALIGN, 16) };
        if ret != 0 {
            unsafe {
                libc::free(data_ptr);
                libc::free(sense_ptr);
            }
            bail!("posix_memalign(cdb) = {}", ret);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(cdb.as_ptr(), cdb_ptr as *mut u8, 16);
        }

        let mut hdr = SgIoHdr {
            interface_id: b'S' as i32,
            dxfer_direction: direction,
            cmd_len: 16,
            mx_sb_len: 32,
            dxfer_len: if buf_size == 0 { 0 } else { buf_size as u32 },
            dxferp: if buf_size == 0 {
                std::ptr::null_mut()
            } else {
                data_ptr as *mut u8
            },
            cmdp: cdb_ptr as *mut u8,
            sbp: sense_ptr as *mut u8,
            timeout: timeout_ms,
            ..Default::default()
        };

        // ⚠ `as _` — компилятор сам определит тип request:
        //   - glibc: Ioctl = c_ulong
        //   - musl:  Ioctl = c_int
        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO as _, &mut hdr) };
        let ioctl_err = if ret < 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };

        let result: Result<Vec<u8>> = (|| {
            if let Some(e) = ioctl_err {
                bail!("ioctl(SG_IO): {}", e);
            }
            if hdr.status != 0 || hdr.host_status != 0 || hdr.driver_status != 0 {
                let sense_slice = unsafe { std::slice::from_raw_parts(sense_ptr as *const u8, 32) };
                bail!(
                    "SCSI error на {:?}: status={:#x} host={:#x} driver={:#x} resid={}\n{}",
                    self.path,
                    hdr.status,
                    hdr.host_status,
                    hdr.driver_status,
                    hdr.resid,
                    format_sense(sense_slice, hdr.sb_len_wr as usize)
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
            if buf_size > 0 {
                Ok(unsafe { std::slice::from_raw_parts(data_ptr as *const u8, buf_size).to_vec() })
            } else {
                Ok(Vec::new())
            }
        })();

        unsafe {
            libc::free(data_ptr);
            libc::free(sense_ptr);
            libc::free(cdb_ptr);
        }

        result
    }

    #[allow(clippy::too_many_arguments)]
    fn make_cdb(
        command: u8,
        feature: u8,
        sector_count: u16,
        lba: u64,
        device: u8,
        protocol: u8,
        t_dir: bool,
        t_length: u8,
    ) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x85;
        cdb[1] = (protocol & 0x0F) << 1;
        cdb[2] = (t_length & 0b11) | 0b0100 | (if t_dir { 0b1000 } else { 0 });
        cdb[3] = feature;
        cdb[4] = (sector_count & 0xFF) as u8;
        cdb[5] = (sector_count >> 8) as u8;
        cdb[6] = (lba & 0xFF) as u8;
        cdb[7] = ((lba >> 8) & 0xFF) as u8;
        cdb[8] = ((lba >> 16) & 0xFF) as u8;
        cdb[9] = device | (((lba >> 24) & 0x0F) as u8);
        cdb[10] = command;
        cdb
    }

    pub fn identify_device(&self) -> Result<[u8; 512]> {
        let variants: &[(&str, u8, u8)] = &[
            ("dev=0xA0 cdb2=0x0E", 0xA0, 0x0E),
            ("dev=0x00 cdb2=0x0E", 0x00, 0x0E),
            ("dev=0x40 cdb2=0x0E", 0x40, 0x0E),
            ("dev=0xA0 cdb2=0x1E", 0xA0, 0x1E),
            ("dev=0xA0 cdb2=0x2E", 0xA0, 0x2E),
            ("dev=0x00 cdb2=0x0C", 0x00, 0x0C),
            ("dev=0xA0 cdb2=0x0C", 0xA0, 0x0C),
        ];

        for (name, device, cdb2) in variants {
            let mut cdb = [0u8; 16];
            cdb[0] = 0x85;
            cdb[1] = (PROTO_PIO_IN & 0x0F) << 1;
            cdb[2] = *cdb2;
            cdb[3] = 0;
            cdb[4] = 1;
            cdb[5] = 0;
            cdb[6] = 0;
            cdb[7] = 0;
            cdb[8] = 0;
            cdb[9] = *device;
            cdb[10] = ATA_IDENTIFY;

            if let Ok(out) = self.sg_io(&cdb, SG_DXFER_FROM_DEV, 512, 10_000) {
                let word0 = u16::from_le_bytes([out[0], out[1]]);
                if word0 != 0x0000 && word0 != 0xFFFF {
                    eprintln!("[i] IDENTIFY через SG_IO: {} — OK", name);
                    let mut arr = [0u8; 512];
                    arr.copy_from_slice(&out[..512]);
                    return Ok(arr);
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        anyhow::bail!("Ни один вариант CDB для IDENTIFY DEVICE не сработал")
    }

    pub fn identify_hdio(&self) -> Result<[u8; 512]> {
        const HDIO_GET_IDENTITY: u64 = 0x030d;
        let mut buf = [0u8; 512];

        // ⚠ `as _` — компилятор сам определит тип request.
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                HDIO_GET_IDENTITY as _,
                buf.as_mut_ptr(),
            )
        };
        if ret < 0 {
            bail!(
                "ioctl(HDIO_GET_IDENTITY): {}",
                std::io::Error::last_os_error()
            );
        }
        let word0 = u16::from_le_bytes([buf[0], buf[1]]);
        if word0 == 0 || word0 == 0xFFFF {
            bail!("HDIO_GET_IDENTITY вернул мусор (word0={:#06x})", word0);
        }
        Ok(buf)
    }

    pub fn smart_enable(&self) -> Result<()> {
        let smart_lba = ((SMART_CYL_HI as u64) << 8) | (SMART_CYL_LOW as u64);
        let cdb = Self::make_cdb(
            ATA_SMART,
            SMART_ENABLE,
            0,
            smart_lba,
            0x40,
            PROTO_NON_DATA,
            false,
            TL_48,
        );
        self.sg_io(&cdb, SG_DXFER_NONE, 0, 30_000)?;
        Ok(())
    }

    pub fn smart_read_data(&self) -> Result<[u8; 512]> {
        let smart_lba = ((SMART_CYL_HI as u64) << 8) | (SMART_CYL_LOW as u64);
        let cdb = Self::make_cdb(
            ATA_SMART,
            SMART_READ_DATA,
            1,
            smart_lba,
            0x40,
            PROTO_PIO_IN,
            true,
            TL_48,
        );
        let out = self.sg_io(&cdb, SG_DXFER_FROM_DEV, 512, 30_000)?;
        let mut arr = [0u8; 512];
        arr.copy_from_slice(&out[..512]);
        Ok(arr)
    }

    pub fn smart_read_thresholds(&self) -> Result<[u8; 512]> {
        let smart_lba = ((SMART_CYL_HI as u64) << 8) | (SMART_CYL_LOW as u64);
        let cdb = Self::make_cdb(
            ATA_SMART,
            SMART_READ_THRESH,
            1,
            smart_lba,
            0x40,
            PROTO_PIO_IN,
            true,
            TL_48,
        );
        let out = self.sg_io(&cdb, SG_DXFER_FROM_DEV, 512, 30_000)?;
        let mut arr = [0u8; 512];
        arr.copy_from_slice(&out[..512]);
        Ok(arr)
    }

    pub fn smart_return_status(&self) -> Result<bool> {
        let smart_lba = ((SMART_CYL_HI as u64) << 8) | (SMART_CYL_LOW as u64);
        let cdb = Self::make_cdb(
            ATA_SMART,
            SMART_RETURN_STATUS,
            0,
            smart_lba,
            0x40,
            PROTO_NON_DATA,
            false,
            TL_48,
        );
        match self.sg_io(&cdb, SG_DXFER_NONE, 0, 30_000) {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    pub fn reassign_sectors(&self, lba: u64, count: u16) -> Result<()> {
        let cdb = Self::make_cdb(
            ATA_SET_FEATURES,
            FEATURE_REASSIGN_SECTORS,
            count,
            lba,
            0x40,
            PROTO_NON_DATA,
            false,
            TL_48,
        );
        self.sg_io(&cdb, SG_DXFER_NONE, 0, 60_000)?;
        Ok(())
    }
}

// ============================================================
// Sense-парсер
// ============================================================
pub fn format_sense(sense: &[u8], len: usize) -> String {
    let len = len.min(sense.len());
    if len < 14 {
        return format!("  sense_data={} (len={})", hex::encode(&sense[..len]), len);
    }
    let response = sense[0] & 0x7F;
    let key = sense[2] & 0x0F;
    let asc = sense[12];
    let ascq = sense[13];

    let key_str = match key {
        0x0 => "NO SENSE",
        0x1 => "RECOVERED ERROR",
        0x2 => "NOT READY",
        0x3 => "MEDIUM ERROR",
        0x4 => "HARDWARE ERROR",
        0x5 => "ILLEGAL REQUEST",
        0x6 => "UNIT ATTENTION",
        0x7 => "DATA PROTECT",
        0x8 => "BLANK CHECK",
        0x9 => "VENDOR SPECIFIC",
        0xA => "COPY ABORTED",
        0xB => "ABORTED COMMAND",
        0xC => "EQUAL",
        0xD => "VOLUME OVERFLOW",
        0xE => "MISCOMPARE",
        _ => "UNKNOWN",
    };
    let asc_str = match (asc, ascq) {
        (0x00, 0x00) => "OK",
        (0x20, 0x00) => "Invalid command operation code",
        (0x24, 0x00) => "Invalid field in CDB",
        (0x25, 0x00) => "Logical unit not supported",
        (0x29, 0x00) => "Power on, reset, or bus device reset occurred",
        (0x3E, 0x00) => "Logical unit has not self-configured yet",
        (0x4E, 0x00) => "Overlapped commands attempted",
        _ => "Unknown ASC/ASCQ",
    };
    format!(
        "  sense response={:#x}\n  sense_key={:#x} ({})\n  asc={:#x} ascq={:#x} ({})\n  raw={}",
        response,
        key,
        key_str,
        asc,
        ascq,
        asc_str,
        hex::encode(&sense[..len])
    )
}

// ============================================================
// IDENTIFY DEVICE — разбор
// ============================================================
#[derive(Debug, Clone)]
pub struct IdentifyInfo {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub lba28_sectors: u64,
    pub lba48_sectors: u64,
    pub rotation_rpm: Option<u16>,
    pub is_ssd: bool,
    pub smart_supported: bool,
    pub trim_supported: bool,
    pub form_factor: Option<&'static str>,
    pub interface: &'static str,
    pub ncq_supported: bool,
}

impl IdentifyInfo {
    pub fn max_lba(&self) -> u64 {
        if self.lba48_sectors > 0 {
            self.lba48_sectors
        } else {
            self.lba28_sectors
        }
    }
    pub fn size_bytes(&self) -> u64 {
        self.max_lba().saturating_mul(512)
    }
    pub fn kind(&self) -> &'static str {
        if self.is_ssd {
            "SSD"
        } else if self.rotation_rpm.is_some() {
            "HDD"
        } else {
            "Unknown"
        }
    }
}

pub fn identify(dev: &Path) -> Result<IdentifyInfo> {
    let d = AtaDevice::open(dev, false)?;

    match d.identify_hdio() {
        Ok(data) => {
            eprintln!("[i] Использован ioctl(HDIO_GET_IDENTITY)");
            return parse_identify_plain(&data);
        }
        Err(e) => {
            eprintln!("[i] HDIO_GET_IDENTITY: {} (пробуем SG_IO)", e);
        }
    }

    let data = d.identify_device()?;
    parse_identify(&data)
}

pub fn parse_identify(data: &[u8; 512]) -> Result<IdentifyInfo> {
    let model = ata_string_swapped(&data[54..94]);
    let serial = ata_string_swapped(&data[20..40]);
    let firmware = ata_string_swapped(&data[46..54]);

    let lba28_lo = u16::from_le_bytes([data[120], data[121]]) as u64;
    let lba28_hi = u16::from_le_bytes([data[122], data[123]]) as u64;
    let lba28 = lba28_lo | (lba28_hi << 16);

    let lba48 = u64::from_le_bytes([
        data[200], data[201], data[202], data[203], data[204], data[205], data[206], data[207],
    ]);

    let rotation = u16::from_le_bytes([data[434], data[435]]);
    let is_ssd = rotation == 1;

    let cmd_set1 = u16::from_le_bytes([data[338], data[339]]);
    let smart_supported = (cmd_set1 & 0x0001) != 0;
    let trim_bit = (cmd_set1 & 0x0020) != 0;
    let trim_supported = is_ssd || trim_bit;

    let sata_caps = u16::from_le_bytes([data[152], data[153]]);
    let is_sata = sata_caps != 0;

    let interface = if is_sata {
        "SATA"
    } else if cmd_set1 & 0x0040 != 0 {
        "PATA"
    } else {
        "Unknown"
    };

    let ncq_supported = (sata_caps & 0x0100) != 0;

    let ff_word = u16::from_le_bytes([data[336], data[337]]);
    let form_factor = form_factor_from_word(ff_word);

    let word0 = u16::from_le_bytes([data[0], data[1]]);
    if word0 == 0x0000 || word0 == 0xFFFF {
        bail!("IDENTIFY DEVICE: невалидный ответ (word0 = {:#06x})", word0);
    }

    Ok(IdentifyInfo {
        model,
        serial,
        firmware,
        lba28_sectors: lba28,
        lba48_sectors: lba48,
        rotation_rpm: if is_ssd {
            None
        } else if rotation != 0 {
            Some(rotation)
        } else {
            None
        },
        is_ssd,
        smart_supported,
        trim_supported,
        form_factor,
        interface,
        ncq_supported,
    })
}

pub fn parse_identify_plain(data: &[u8; 512]) -> Result<IdentifyInfo> {
    let model = ata_string_plain(&data[54..94]);
    let serial = ata_string_plain(&data[20..40]);
    let firmware = ata_string_plain(&data[46..54]);

    let lba28 = u16::from_le_bytes([data[120], data[121]]) as u64;

    let lba48 = u64::from_le_bytes([
        data[200], data[201], data[202], data[203], data[204], data[205], data[206], data[207],
    ]);

    let rotation = u16::from_le_bytes([data[434], data[435]]);
    let is_ssd = rotation == 1;

    let cmd_set1 = u16::from_le_bytes([data[164], data[165]]);
    let smart_supported = (cmd_set1 & 0x0001) != 0;
    let trim_bit = (cmd_set1 & 0x0020) != 0;
    let trim_supported = is_ssd || trim_bit;

    let sata_caps = u16::from_le_bytes([data[152], data[153]]);
    let is_sata = sata_caps != 0;

    let interface = if is_sata {
        "SATA"
    } else if cmd_set1 & 0x0040 != 0 {
        "PATA"
    } else {
        "Unknown"
    };

    let ncq_supported = (sata_caps & 0x0100) != 0;

    let ff_word = u16::from_le_bytes([data[217], data[218]]);
    let form_factor = form_factor_from_word(ff_word);

    let word0 = u16::from_le_bytes([data[0], data[1]]);
    if word0 == 0x0000 || word0 == 0xFFFF {
        bail!("IDENTIFY DEVICE: невалидный ответ (word0 = {:#06x})", word0);
    }

    Ok(IdentifyInfo {
        model,
        serial,
        firmware,
        lba28_sectors: lba28,
        lba48_sectors: lba48,
        rotation_rpm: if is_ssd {
            None
        } else if rotation != 0 {
            Some(rotation)
        } else {
            None
        },
        is_ssd,
        smart_supported,
        trim_supported,
        form_factor,
        interface,
        ncq_supported,
    })
}

fn form_factor_from_word(ff_word: u16) -> Option<&'static str> {
    match ff_word {
        0x0001 => Some("5.25\""),
        0x0002 => Some("3.5\""),
        0x0003 => Some("2.5\""),
        0x0004 => Some("1.8\""),
        0x0005 => Some("< 1.8\""),
        0x0006 => Some("mSATA"),
        0x0007 => Some("M.2"),
        0x0008 => Some("MicroSSD"),
        0x0009 => Some("CFast"),
        0x000A => Some("Embedded"),
        _ => None,
    }
}

fn ata_string_swapped(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for chunk in bytes.chunks(2) {
        if chunk.len() == 2 {
            let c0 = chunk[1];
            let c1 = chunk[0];
            if c0 != 0 {
                s.push(c0 as char);
            }
            if c1 != 0 {
                s.push(c1 as char);
            }
        }
    }
    s.trim().to_string()
}

fn ata_string_plain(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if b == 0 {
            continue;
        }
        if (0x20..0x7F).contains(&b) {
            s.push(b as char);
        }
    }
    s.trim().to_string()
}

// ============================================================
// Тесты
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ata_string_swapped() {
        let bytes: Vec<u8> = vec![
            b'D', b'A', b'T', b'A', b' ', b'A', b'U', b'S', b'5', b'6', b'0', 0, 0, 0, 0, 0,
        ];
        assert_eq!(ata_string_swapped(&bytes), "ADATA SU650");
    }

    #[test]
    fn test_ata_string_swapped_trailing_spaces() {
        let bytes: Vec<u8> = vec![
            b'D', b'A', b'T', b'A', b' ', b'A', b'U', b'S', b'5', b'6', b'0', 0, b' ', 0, b' ', 0,
        ];
        assert_eq!(ata_string_swapped(&bytes), "ADATA SU650");
    }

    #[test]
    fn test_ata_string_plain() {
        let bytes: Vec<u8> = vec![
            b'A', b'D', b'A', b'T', b'A', b' ', b'S', b'U', b'6', b'5', b'0', b' ', b' ', b' ',
            b' ', b' ',
        ];
        assert_eq!(ata_string_plain(&bytes), "ADATA SU650");
    }

    #[test]
    fn test_ata_string_plain_empty() {
        assert_eq!(ata_string_plain(&vec![0u8; 20]), "");
    }

    #[test]
    fn test_form_factor() {
        assert_eq!(form_factor_from_word(0x0002), Some("3.5\""));
        assert_eq!(form_factor_from_word(0x0003), Some("2.5\""));
        assert_eq!(form_factor_from_word(0x0007), Some("M.2"));
        assert_eq!(form_factor_from_word(0x0006), Some("mSATA"));
        assert_eq!(form_factor_from_word(0xFFFF), None);
    }

    #[test]
    fn test_parse_identify_invalid() {
        assert!(parse_identify(&[0u8; 512]).is_err());
    }

    #[test]
    fn test_parse_identify_plain_invalid() {
        assert!(parse_identify_plain(&[0u8; 512]).is_err());
    }

    #[test]
    fn test_parse_identify_ff() {
        let mut d = [0u8; 512];
        d[0] = 0xFF;
        d[1] = 0xFF;
        assert!(parse_identify(&d).is_err());
        assert!(parse_identify_plain(&d).is_err());
    }

    #[test]
    fn test_identify_info_max_lba() {
        let info = IdentifyInfo {
            model: "T".into(),
            serial: "S".into(),
            firmware: "F".into(),
            lba28_sectors: 1000,
            lba48_sectors: 2000,
            rotation_rpm: None,
            is_ssd: true,
            smart_supported: true,
            trim_supported: true,
            form_factor: Some("2.5\""),
            interface: "SATA",
            ncq_supported: true,
        };
        assert_eq!(info.max_lba(), 2000);
        let info2 = IdentifyInfo {
            lba48_sectors: 0,
            ..info.clone()
        };
        assert_eq!(info2.max_lba(), 1000);
    }

    #[test]
    fn test_identify_info_size_bytes() {
        let info = IdentifyInfo {
            model: "T".into(),
            serial: "S".into(),
            firmware: "F".into(),
            lba28_sectors: 0,
            lba48_sectors: 234441648,
            rotation_rpm: None,
            is_ssd: true,
            smart_supported: true,
            trim_supported: true,
            form_factor: Some("M.2"),
            interface: "SATA",
            ncq_supported: true,
        };
        assert_eq!(info.size_bytes(), 234441648 * 512);
        assert_eq!(info.kind(), "SSD");
    }
}
