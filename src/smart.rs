//! Разбор SMART.
//!
//! Основной путь: SG_IO напрямую к диску.
//! Fallback: если SG_IO не сработал (AHCI блокирует на некоторых SSD),
//! читаем через внешний smartctl (-a -j) и парсим JSON.
//!
//! Атрибуты сгруппированы по контроллерам:
//!   - Silicon Motion (ADATA, Kingston, Apacer, Netac, DEXP, MSI, Hikvision,
//!     Silicon Power, Transcend, SmartBuy, Goodram, TeamGroup)
//!   - Phison (Kingston NV2, GIGABYTE, HP, Lenovo, Patriot, Lexar, некоторые Crucial)
//!   - Marvell (Samsung 870, WD Blue, SanDisk)
//!   - Samsung собственные (860/870/970/980/990)
//!   - Intel / Solidigm (540s, 545s, 660p, 665p, 670p)
//!   - Micron / Crucial (MX500, BX500, P1/P3/P5)
//!   - WD / SanDisk
//!   - Seagate (Barracuda, IronWolf, SkyHawk, Exos)
//!   - Toshiba / Kioxia (TR200, MG, BG)
//!   - HP (S700, S750, EX900, EX950)
//!   - Lenovo (SL700, ThinkLife)
//!   - ADATA XPG NVMe (SX6000, SX8200, S11, Gammix)
//!   - Realtek (некоторые Netac, SmartBuy)

#![allow(dead_code)]

use crate::ata::AtaDevice;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;

// ============================================================
// Публичные типы
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Critical,
    Info,
    Cosmetic,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartHealth {
    Ok,
    Failed,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Attribute {
    pub id: u8,
    pub name: String,
    pub value: u8,
    pub worst: u8,
    pub threshold: u8,
    pub raw: u64,
    pub flags: u16,
    pub prefail: bool,
    pub online: bool,
    pub failed: bool,
    pub severity: Severity,
}

#[derive(Debug)]
pub struct SmartData {
    pub attributes: Vec<Attribute>,
    pub temperature: Option<i16>,
    pub health_table: SmartHealth,
    pub health_return: SmartHealth,
    pub revision: u16,
    pub source: &'static str,
}

impl SmartData {
    pub fn health(&self) -> SmartHealth {
        match (self.health_table, self.health_return) {
            (SmartHealth::Failed, _) | (_, SmartHealth::Failed) => SmartHealth::Failed,
            (SmartHealth::Ok, SmartHealth::Ok) => SmartHealth::Ok,
            (SmartHealth::Ok, SmartHealth::Unknown) => SmartHealth::Ok,
            (SmartHealth::Unknown, SmartHealth::Ok) => SmartHealth::Ok,
            _ => SmartHealth::Unknown,
        }
    }

    pub fn attr(&self, id: u8) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.id == id)
    }

    /// Проблемные: Critical + raw > 0, либо уже сработавший порог.
    pub fn problems(&self) -> Vec<&Attribute> {
        self.attributes
            .iter()
            .filter(|a| (a.severity == Severity::Critical && a.raw > 0) || a.failed)
            .collect()
    }
}

// ============================================================
// Публичные функции
// ============================================================

/// Прочитать SMART: сначала SG_IO, при неудаче — smartctl.
pub fn read(device: &Path) -> Result<SmartData> {
    match read_via_sgio(device) {
        Ok(d) => {
            eprintln!("[i] SMART прочитан напрямую через SG_IO");
            return Ok(d);
        }
        Err(e) => {
            eprintln!("[i] Прямой SG_IO не сработал: {}", e);
            eprintln!("[i] Fallback на smartctl...");
        }
    }

    read_via_smartctl(device)
}

fn read_via_sgio(device: &Path) -> Result<SmartData> {
    let dev = AtaDevice::open(device, false).context("Не удалось открыть устройство для SMART")?;

    if let Err(e) = dev.smart_enable() {
        eprintln!("[i] SMART ENABLE: {} (продолжаем)", e);
    }

    let data = dev
        .smart_read_data()
        .context("SMART READ DATA (0xB0/0xD0) failed")?;

    let thresholds = dev.smart_read_thresholds().ok();
    let health_return = match dev.smart_return_status() {
        Ok(true) => SmartHealth::Ok,
        Ok(false) => SmartHealth::Failed,
        Err(_) => SmartHealth::Unknown,
    };

    let mut d = parse_with(&data, thresholds.as_ref(), health_return)?;
    d.source = "sgio";
    Ok(d)
}

pub fn read_via_smartctl(device: &Path) -> Result<SmartData> {
    use std::process::Command;

    let output = Command::new("smartctl")
        .args(["-a", "-j"])
        .arg(device)
        .output()
        .context("smartctl не найден. Установите smartmontools.")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        bail!(
            "smartctl не вернул JSON (stderr: {})",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut d = parse_smartctl_json(&stdout)?;
    d.source = "smartctl";
    Ok(d)
}

pub fn parse(data: &[u8; 512]) -> Result<SmartData> {
    let mut d = parse_with(data, None, SmartHealth::Unknown)?;
    d.source = "manual";
    Ok(d)
}

// ============================================================
// Парсер SG_IO ответа
// ============================================================

fn parse_with(
    data: &[u8; 512],
    thresholds_raw: Option<&[u8; 512]>,
    health_return: SmartHealth,
) -> Result<SmartData> {
    let revision = u16::from_le_bytes([data[0], data[1]]);
    if revision == 0 {
        bail!("SMART READ DATA: revision = 0, таблица пустая или невалидная");
    }

    let thresholds = thresholds_raw.map(parse_thresholds).unwrap_or_default();

    let mut attributes = Vec::with_capacity(30);
    let mut temperature: Option<i16> = None;

    for i in 0..30 {
        let off = 2 + i * 12;
        if off + 12 > 362 {
            break;
        }
        let id = data[off];
        if id == 0 {
            continue;
        }

        let flags = u16::from_le_bytes([data[off + 1], data[off + 2]]);
        let value = data[off + 3];
        let worst = data[off + 4];
        let raw = u64::from_le_bytes([
            data[off + 5],
            data[off + 6],
            data[off + 7],
            data[off + 8],
            data[off + 9],
            data[off + 10],
            data[off + 11],
            0,
        ]);
        let threshold = thresholds.get(&id).copied().unwrap_or(0);
        let prefail = (flags & 0x0001) != 0;
        let online = (flags & 0x0002) != 0;
        let failed = prefail && threshold > 0 && value <= threshold;

        if let Some(t) = extract_temperature(id, raw) {
            temperature = Some(t);
        }

        attributes.push(Attribute {
            id,
            name: attr_name(id).to_string(),
            value,
            worst,
            threshold,
            raw,
            flags,
            prefail,
            online,
            failed,
            severity: severity(id),
        });
    }

    let health_table = if data[362] & 0x01 == 0 {
        SmartHealth::Ok
    } else {
        SmartHealth::Failed
    };

    Ok(SmartData {
        attributes,
        temperature,
        health_table,
        health_return,
        revision,
        source: "sgio",
    })
}

// ============================================================
// Парсер JSON от smartctl
// ============================================================

#[derive(serde::Deserialize)]
struct SmartctlJson {
    #[serde(default)]
    smart_status: SmartctlStatus,
    #[serde(default)]
    temperature: SmartctlTemp,
    #[serde(default)]
    ata_smart_attributes: SmartctlAttrs,
}

#[derive(serde::Deserialize, Default)]
struct SmartctlStatus {
    #[serde(default)]
    passed: Option<bool>,
}

#[derive(serde::Deserialize, Default)]
struct SmartctlTemp {
    #[serde(default)]
    current: Option<i64>,
}

#[derive(serde::Deserialize, Default)]
struct SmartctlAttrs {
    #[serde(default)]
    table: Vec<SmartctlAttr>,
}

#[derive(serde::Deserialize)]
struct SmartctlAttr {
    id: u8,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<u8>,
    #[serde(default)]
    worst: Option<u8>,
    #[serde(default)]
    thresh: Option<u8>,
    #[serde(default)]
    flags: SmartctlFlags,
    #[serde(default)]
    raw: SmartctlRaw,
}

#[derive(serde::Deserialize, Default)]
struct SmartctlFlags {
    #[serde(default)]
    prefailure: bool,
    #[serde(default)]
    updated_online: bool,
}

#[derive(serde::Deserialize, Default)]
struct SmartctlRaw {
    #[serde(default)]
    value: Option<u64>,
}

fn parse_smartctl_json(json: &str) -> Result<SmartData> {
    let j: SmartctlJson =
        serde_json::from_str(json).context("Не удалось распарсить JSON от smartctl")?;

    let mut attributes = Vec::new();
    let mut temperature = None;

    for a in &j.ata_smart_attributes.table {
        let id = a.id;
        let value = a.value.unwrap_or(0);
        let worst = a.worst.unwrap_or(0);
        let threshold = a.thresh.unwrap_or(0);
        let raw = a.raw.value.unwrap_or(0);
        let prefail = a.flags.prefailure;
        let online = a.flags.updated_online;
        let failed = prefail && threshold > 0 && value <= threshold;

        let name = a.name.clone().unwrap_or_else(|| attr_name(id).to_string());

        if let Some(t) = extract_temperature(id, raw) {
            temperature = Some(t);
        }

        attributes.push(Attribute {
            id,
            name,
            value,
            worst,
            threshold,
            raw,
            flags: 0,
            prefail,
            online,
            failed,
            severity: severity(id),
        });
    }

    let health_table = match j.smart_status.passed {
        Some(true) => SmartHealth::Ok,
        Some(false) => SmartHealth::Failed,
        None => SmartHealth::Unknown,
    };
    let health_return = health_table;

    let temperature = temperature.or_else(|| j.temperature.current.map(|t| t as i16));

    Ok(SmartData {
        attributes,
        temperature,
        health_table,
        health_return,
        revision: 0x0010,
        source: "smartctl",
    })
}

// ============================================================
// Утилиты
// ============================================================

fn extract_temperature(id: u8, raw: u64) -> Option<i16> {
    match id {
        190 | 194 => {
            let t = (raw & 0xFF) as i16;
            if (-60..=150).contains(&t) {
                Some(t)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_thresholds(data: &[u8; 512]) -> HashMap<u8, u8> {
    let mut map = HashMap::new();
    for i in 0..30 {
        let off = 2 + i * 12;
        if off + 12 > 362 {
            break;
        }
        let id = data[off];
        if id == 0 {
            continue;
        }
        let threshold = data[off + 3];
        map.insert(id, threshold);
    }
    map
}

fn severity(id: u8) -> Severity {
    match id {
        5 | 187 | 196 | 197 | 198 => Severity::Critical,
        177 | 179 | 181 | 182 | 199 | 201 | 202 | 231 | 233 => Severity::Info,
        4 | 9 | 12 | 190 | 194 | 241 | 242 => Severity::Cosmetic,
        _ => Severity::Unknown,
    }
}

fn attr_name(id: u8) -> &'static str {
    match id {
        // ─── Общие ATA ────────────────────────────────────
        1 => "Read_Error_Rate",
        2 => "Throughput_Performance",
        3 => "Spin_Up_Time",
        4 => "Start_Stop_Count",
        5 => "Reallocated_Sector_Ct",
        6 => "Read_Channel_Margin",
        7 => "Seek_Error_Rate",
        8 => "Seek_Time_Performance",
        9 => "Power_On_Hours",
        10 => "Spin_Retry_Count",
        11 => "Calibration_Retry_Count",
        12 => "Power_Cycle_Count",
        13 => "Read_Soft_Error_Rate",
        22 => "Helium_Level",

        // ─── Silicon Motion SM2258/SM2259 ─────────────────
        160 => "Uncorrectable_Error_Cnt",
        161 => "Valid_Spare_Block_Cnt",
        162 => "Chip_Erase_Cnt",
        163 => "Initial_Bad_Block_Cnt",
        164 => "Total_Erase_Count",
        165 => "Max_Erase_Count",
        166 => "Min_Erase_Count",
        167 => "Average_Erase_Count",
        168 => "Max_Erase_Count_of_Spec",
        169 => "Remaining_Lifetime_Perc",

        // ─── Phison S11/S12/S13 ───────────────────────────
        170 => "Bad_Block_Rate",
        171 => "Program_Fail_Count",
        172 => "Erase_Fail_Count",

        // ─── Samsung / Intel / Marvell ────────────────────
        173 => "Wear_Leveling_Count",
        174 => "Unexpect_Power_Loss_Ct",
        175 => "Program_Fail_Count_Chip",
        176 => "Erase_Fail_Count_Chip",
        177 => "Wear_Leveling_Count",
        178 => "Used_Rsvd_Blk_Cnt_Chip",
        179 => "Used_Rsvd_Blk_Cnt_Tot",
        180 => "Unused_Rsvd_Blk_Cnt_Tot",
        181 => "Program_Fail_Cnt_Total",
        182 => "Erase_Fail_Count_Total",
        183 => "Runtime_Bad_Block",
        184 => "End-to-End_Error",
        185 => "Reported_Uncorrect",
        186 => "Induced_Rewrite_Count",
        187 => "Reported_Uncorrectable",
        188 => "Command_Timeout",
        189 => "High_Fly_Writes",
        190 => "Airflow_Temperature",
        191 => "G-Sense_Error_Rate",
        192 => "Power-Off_Retract_Count",
        193 => "Load_Cycle_Count",
        194 => "Temperature_Celsius",
        195 => "Hardware_ECC_Recovered",
        196 => "Reallocation_Event_Count",
        197 => "Current_Pending_Sector",
        198 => "Offline_Uncorrectable",
        199 => "UDMA_CRC_Error_Count",
        200 => "Multi_Zone_Error_Rate",
        201 => "Soft_Read_Error_Rate",
        202 => "Data_Address_Mark_Errs",
        203 => "Run_Out_Cancel",
        204 => "Soft_ECC_Correction",
        205 => "Thermal_Asperity_Rate",
        206 => "Flying_Height",
        207 => "Spin_High_Current",
        208 => "Spin_Buzz",
        209 => "Offline_Seek_Performnce",

        // ─── Micron / Crucial ─────────────────────────────
        210 => "Success_RAIN_Recov_Cnt",
        211 => "Vendor_Specific_211",
        212 => "Vendor_Specific_212",
        213 => "Vendor_Specific_213",

        // ─── Intel / Solidigm ─────────────────────────────
        214 => "Write_Error_Rate",
        215 => "Unstable_Power_Loss_Cnt",
        216 => "Unsafe_Shutdown_Count",
        217 => "Media_Wearout_Indicator",
        218 => "Read_Error_Retry_Rate",
        219 => "Write_Error_Retry_Rate",

        // ─── Seagate / Toshiba / HGST (HDD) ──────────────
        220 => "Disk_Shift",
        221 => "G-Sense_Error_Rate_Alt",
        222 => "Loaded_Hours",
        223 => "Load_Retry_Count",
        224 => "Load_Friction",
        225 => "Load_Cycle_Count_Alt",
        226 => "Load-in_Time",
        227 => "Torq-amp_Count",
        228 => "Power-off_Retract_Count_Alt",
        229 => "Vendor_Specific_229",
        230 => "Head_Amplitude",

        // ─── SSD-специфичные ──────────────────────────────
        231 => "SSD_Life_Left",
        232 => "Available_Rsvd_Space",
        233 => "Media_Wearout_Indicator",
        234 => "Thermal_Throttle_Status",
        235 => "Power_Loss_Protection",
        236 => "Power_Loss_Protection_Cnt",
        237 => "Vendor_Specific_237",
        238 => "Vendor_Specific_238",
        239 => "Vendor_Specific_239",
        240 => "Head_Flying_Hours",
        241 => "Total_LBAs_Written",
        242 => "Total_LBAs_Read",
        243 => "Total_LBAs_Written_Alt",
        244 => "Total_LBAs_Read_Alt",
        245 => "TLC_Writes_32MiB",
        246 => "Total_Host_Sector_Write",
        247 => "Total_Host_Sector_Read",
        248 => "Total_Host_Sector_Write_Alt",
        249 => "Total_Host_Sector_Read_Alt",

        // ─── Patriot / Lexar / Transcend / Silicon Power ──
        250 => "Read_Error_Retry_Rate",
        251 => "Read_Error_Retry_Rate_Alt",
        252 => "Added_Bad_Flash_Blk_Ct",
        253 => "Erase_Fail_Count_Alt",
        254 => "Free_Fall_Sensor",

        _ => "Unknown",
    }
}

// ============================================================
// Тесты
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_temperature_194() {
        assert_eq!(extract_temperature(194, 35), Some(35));
        assert_eq!(extract_temperature(194, 0), Some(0));
        assert_eq!(extract_temperature(194, 0xFF), None);
    }

    #[test]
    fn test_extract_temperature_out_of_range() {
        assert_eq!(extract_temperature(194, 200), None);
    }

    #[test]
    fn test_severity_critical() {
        assert_eq!(severity(5), Severity::Critical);
        assert_eq!(severity(197), Severity::Critical);
        assert_eq!(severity(198), Severity::Critical);
    }

    #[test]
    fn test_severity_info() {
        assert_eq!(severity(199), Severity::Info);
        assert_eq!(severity(201), Severity::Info);
        assert_eq!(severity(177), Severity::Info);
    }

    #[test]
    fn test_severity_cosmetic() {
        assert_eq!(severity(9), Severity::Cosmetic);
        assert_eq!(severity(194), Severity::Cosmetic);
    }

    #[test]
    fn test_attr_name() {
        assert_eq!(attr_name(5), "Reallocated_Sector_Ct");
        assert_eq!(attr_name(194), "Temperature_Celsius");
        // Согласно smartctl: "Remaining_Lifetime_Perc"
        assert_eq!(attr_name(169), "Remaining_Lifetime_Perc");
        assert_eq!(attr_name(255), "Unknown");
    }

    #[test]
    fn test_empty_revision_fails() {
        assert!(parse(&[0u8; 512]).is_err());
    }

    #[test]
    fn test_parse_valid_empty() {
        let mut data = [0u8; 512];
        data[0] = 0x10;
        let smart = parse(&data).unwrap();
        assert_eq!(smart.revision, 0x0010);
        assert!(smart.attributes.is_empty());
    }

    #[test]
    fn test_problems_filter_199_not_critical() {
        let mut d = [0u8; 512];
        d[0] = 0x10;
        let off = 2;
        d[off] = 199;
        d[off + 3] = 100;
        d[off + 4] = 100;
        d[off + 5] = 0x19;

        let smart = parse(&d).unwrap();
        assert!(smart.problems().is_empty());
    }

    #[test]
    fn test_problems_filter_5_is_critical() {
        let mut d = [0u8; 512];
        d[0] = 0x10;
        let off = 2;
        d[off] = 5;
        d[off + 1] = 0x01;
        d[off + 3] = 100;
        d[off + 4] = 100;
        d[off + 5] = 10;

        let smart = parse(&d).unwrap();
        let problems = smart.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].id, 5);
    }

    #[test]
    fn test_parse_smartctl_json_with_status() {
        let json = r#"{
            "smart_status": {"passed": true},
            "temperature": {"current": 40},
            "ata_smart_attributes": {
                "table": [
                    {"id": 5, "name": "Reallocated_Sector_Ct",
                     "value": 100, "worst": 100, "thresh": 10,
                     "flags": {"prefailure": true, "updated_online": true},
                     "raw": {"value": 0}},
                    {"id": 199, "name": "UDMA_CRC_Error_Count",
                     "value": 100, "worst": 100, "thresh": 0,
                     "flags": {"prefailure": false, "updated_online": true},
                     "raw": {"value": 1561}}
                ]
            }
        }"#;

        let smart = parse_smartctl_json(json).unwrap();
        assert_eq!(smart.health_table, SmartHealth::Ok);
        assert_eq!(smart.health_return, SmartHealth::Ok);
        assert_eq!(smart.temperature, Some(40));
        assert_eq!(smart.attributes.len(), 2);
        assert!(smart.problems().is_empty());
    }

    #[test]
    fn test_parse_smartctl_json_health_failed() {
        let json = r#"{
            "smart_status": {"passed": false},
            "ata_smart_attributes": {"table": []}
        }"#;
        let smart = parse_smartctl_json(json).unwrap();
        assert_eq!(smart.health_table, SmartHealth::Failed);
        assert_eq!(smart.health_return, SmartHealth::Failed);
        assert_eq!(smart.health(), SmartHealth::Failed);
    }
}
