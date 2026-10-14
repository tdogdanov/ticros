//! Пороги классификации блоков при сканировании.
//!
//! Логика повторяет Victoria:
//!   <fast_ms   → Grey   (отлично)
//!   <good_ms   → Green  (хорошо)
//!   <slow_ms   → Yellow (медленно)
//!   <bad_ms    → Red    (плохо)
//!   ≥bad_ms    → Bad    (очень плохо)
//!   io_error   → Bad    (ошибка чтения)

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::BitOr;
use std::str::FromStr;

// ============================================================
// Ошибки валидации
// ============================================================

#[derive(Debug, Clone, PartialEq)]
pub enum ThresholdError {
    NotFinite(&'static str, f64),
    NotPositive(&'static str, f64),
    NotGreater {
        name: &'static str,
        value: f64,
        must_exceed_name: &'static str,
        must_exceed_value: f64,
    },
    BadFormat(String),
}

impl fmt::Display for ThresholdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFinite(name, v) => {
                write!(f, "{} должно быть конечным числом, получено {}", name, v)
            }
            Self::NotPositive(name, v) => {
                write!(f, "{} должно быть > 0, получено {}", name, v)
            }
            Self::NotGreater {
                name,
                value,
                must_exceed_name,
                must_exceed_value,
            } => write!(
                f,
                "{} ({}) должно быть > {} ({})",
                name, value, must_exceed_name, must_exceed_value
            ),
            Self::BadFormat(s) => write!(f, "Неверный формат порогов: {}", s),
        }
    }
}

impl std::error::Error for ThresholdError {}

// ============================================================
// Пороги
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    pub fast_ms: f64,
    pub good_ms: f64,
    pub slow_ms: f64,
    pub bad_ms: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            fast_ms: 10.0,
            good_ms: 50.0,
            slow_ms: 200.0,
            bad_ms: 1000.0,
        }
    }
}

impl Thresholds {
    // ─── Валидация ─────────────────────────────────────────

    pub fn validate(&self) -> Result<(), ThresholdError> {
        self.check("fast_ms", self.fast_ms, None)?;
        self.check("good_ms", self.good_ms, Some(("fast_ms", self.fast_ms)))?;
        self.check("slow_ms", self.slow_ms, Some(("good_ms", self.good_ms)))?;
        self.check("bad_ms", self.bad_ms, Some(("slow_ms", self.slow_ms)))?;
        Ok(())
    }

    fn check(
        &self,
        name: &'static str,
        value: f64,
        must_exceed: Option<(&'static str, f64)>,
    ) -> Result<(), ThresholdError> {
        if !value.is_finite() {
            return Err(ThresholdError::NotFinite(name, value));
        }
        if value <= 0.0 {
            return Err(ThresholdError::NotPositive(name, value));
        }
        if let Some((prev_name, prev_value)) = must_exceed {
            if value <= prev_value {
                return Err(ThresholdError::NotGreater {
                    name,
                    value,
                    must_exceed_name: prev_name,
                    must_exceed_value: prev_value,
                });
            }
        }
        Ok(())
    }

    // ─── Классификация ─────────────────────────────────────

    /// Классифицировать блок по времени чтения и флагу ошибки.
    ///
    /// Особые случаи:
    ///   - `io_error = true`         → Bad
    ///   - `NaN` или `-INF`          → Grey (считаем мгновенным)
    ///   - `+INF`                    → Bad (крайне медленно)
    pub fn classify(&self, dt_ms: f64, io_error: bool) -> BlockClass {
        if io_error {
            return BlockClass::Bad;
        }
        // NaN и отрицательные (включая -INF) → мгновенно
        if dt_ms.is_nan() || dt_ms < 0.0 {
            return BlockClass::Grey;
        }
        // +INF → крайне медленно → Bad
        if dt_ms.is_infinite() {
            return BlockClass::Bad;
        }
        if dt_ms < self.fast_ms {
            BlockClass::Grey
        } else if dt_ms < self.good_ms {
            BlockClass::Green
        } else if dt_ms < self.slow_ms {
            BlockClass::Yellow
        } else if dt_ms < self.bad_ms {
            BlockClass::Red
        } else {
            BlockClass::Bad
        }
    }

    // ─── Пресеты ───────────────────────────────────────────

    pub fn victoria() -> Self {
        Self::default()
    }

    pub fn mhdd() -> Self {
        Self {
            fast_ms: 5.0,
            good_ms: 20.0,
            slow_ms: 100.0,
            bad_ms: 500.0,
        }
    }

    pub fn fast() -> Self {
        Self {
            fast_ms: 20.0,
            good_ms: 100.0,
            slow_ms: 500.0,
            bad_ms: 2000.0,
        }
    }

    pub fn strict() -> Self {
        Self {
            fast_ms: 5.0,
            good_ms: 15.0,
            slow_ms: 50.0,
            bad_ms: 200.0,
        }
    }

    pub fn from_preset(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "victoria" | "default" => Some(Self::victoria()),
            "mhdd" => Some(Self::mhdd()),
            "fast" => Some(Self::fast()),
            "strict" => Some(Self::strict()),
            _ => None,
        }
    }

    pub const PRESETS: &'static [&'static str] = &["victoria", "mhdd", "fast", "strict"];

    pub fn estimate_from_speed(avg_speed_mb_s: f64) -> Self {
        let k = (100.0 / avg_speed_mb_s.max(1.0)).clamp(0.2, 5.0);
        Self {
            fast_ms: 10.0 * k,
            good_ms: 50.0 * k,
            slow_ms: 200.0 * k,
            bad_ms: 1000.0 * k,
        }
    }
}

// ─── FromStr: "10,50,200,1000" ─────────────────────────────

impl FromStr for Thresholds {
    type Err = ThresholdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s
            .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .filter(|p| !p.is_empty())
            .collect();

        if parts.len() != 4 {
            return Err(ThresholdError::BadFormat(format!(
                "ожидается 4 числа через запятую (fast,good,slow,bad), получено {}: {:?}",
                parts.len(),
                s
            )));
        }

        let parse = |name: &'static str, v: &str| -> Result<f64, ThresholdError> {
            v.parse::<f64>()
                .map_err(|e| ThresholdError::BadFormat(format!("{}: {}", name, e)))
        };

        let t = Self {
            fast_ms: parse("fast_ms", parts[0])?,
            good_ms: parse("good_ms", parts[1])?,
            slow_ms: parse("slow_ms", parts[2])?,
            bad_ms: parse("bad_ms", parts[3])?,
        };
        t.validate()?;
        Ok(t)
    }
}

impl fmt::Display for Thresholds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{},{},{},{}",
            self.fast_ms, self.good_ms, self.slow_ms, self.bad_ms
        )
    }
}

// ============================================================
// Класс блока
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BlockClass {
    Grey,
    Green,
    Yellow,
    Red,
    Bad,
}

impl BlockClass {
    pub const ALL: [BlockClass; 5] = [
        BlockClass::Grey,
        BlockClass::Green,
        BlockClass::Yellow,
        BlockClass::Red,
        BlockClass::Bad,
    ];

    pub fn symbol(self) -> char {
        match self {
            Self::Grey => '·',
            Self::Green | Self::Yellow | Self::Red => '■',
            Self::Bad => 'X',
        }
    }

    pub fn ansi_color(self) -> &'static str {
        match self {
            Self::Grey => "\x1b[90m",
            Self::Green => "\x1b[92m",
            Self::Yellow => "\x1b[93m",
            Self::Red => "\x1b[91m",
            Self::Bad => "\x1b[91;1m",
        }
    }

    pub fn ansi_reset() -> &'static str {
        "\x1b[0m"
    }

    pub fn is_problem(self) -> bool {
        matches!(self, Self::Red | Self::Bad)
    }

    pub fn is_critical(self) -> bool {
        matches!(self, Self::Bad)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Grey => "Grey",
            Self::Green => "Green",
            Self::Yellow => "Yellow",
            Self::Red => "Red",
            Self::Bad => "Bad",
        }
    }

    pub fn hex_color(self) -> u32 {
        match self {
            Self::Grey => 0x808080,
            Self::Green => 0x00C000,
            Self::Yellow => 0xE0C000,
            Self::Red => 0xE04000,
            Self::Bad => 0xFF0000,
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Grey => "Отлично — блок читается мгновенно",
            Self::Green => "Хорошо — нормальная скорость",
            Self::Yellow => "Медленно — блок замедлился",
            Self::Red => "Плохо — сильное замедление",
            Self::Bad => "Бэд — ошибка чтения или крайне медленно",
        }
    }
}

impl fmt::Display for BlockClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BlockClass {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "grey" | "gray" => Ok(Self::Grey),
            "green" => Ok(Self::Green),
            "yellow" => Ok(Self::Yellow),
            "red" => Ok(Self::Red),
            "bad" | "err" | "error" => Ok(Self::Bad),
            _ => Err(format!("Неизвестный класс блока: {}", s)),
        }
    }
}

impl BitOr for BlockClass {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.max(other)
    }
}

// ============================================================
// Счётчики по классам
// ============================================================

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClassCounts {
    pub grey: u64,
    pub green: u64,
    pub yellow: u64,
    pub red: u64,
    pub bad: u64,
}

impl ClassCounts {
    pub fn add(&mut self, class: BlockClass) {
        match class {
            BlockClass::Grey => self.grey += 1,
            BlockClass::Green => self.green += 1,
            BlockClass::Yellow => self.yellow += 1,
            BlockClass::Red => self.red += 1,
            BlockClass::Bad => self.bad += 1,
        }
    }

    pub fn get(&self, class: BlockClass) -> u64 {
        match class {
            BlockClass::Grey => self.grey,
            BlockClass::Green => self.green,
            BlockClass::Yellow => self.yellow,
            BlockClass::Red => self.red,
            BlockClass::Bad => self.bad,
        }
    }

    pub fn total(&self) -> u64 {
        self.grey + self.green + self.yellow + self.red + self.bad
    }

    pub fn problem_percent(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        (self.red + self.bad) as f64 * 100.0 / total as f64
    }

    pub fn critical_percent(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        self.bad as f64 * 100.0 / total as f64
    }
}

// ============================================================
// Тесты
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_validates() {
        Thresholds::default().validate().unwrap();
    }

    #[test]
    fn test_invalid_thresholds() {
        let t = Thresholds {
            fast_ms: 100.0,
            good_ms: 50.0,
            slow_ms: 200.0,
            bad_ms: 1000.0,
        };
        assert!(t.validate().is_err());
    }

    #[test]
    fn test_classify() {
        let t = Thresholds::default();
        assert_eq!(t.classify(5.0, false), BlockClass::Grey);
        assert_eq!(t.classify(30.0, false), BlockClass::Green);
        assert_eq!(t.classify(100.0, false), BlockClass::Yellow);
        assert_eq!(t.classify(500.0, false), BlockClass::Red);
        assert_eq!(t.classify(2000.0, false), BlockClass::Bad);
        assert_eq!(t.classify(5.0, true), BlockClass::Bad);
    }

    #[test]
    fn test_classify_nan_inf() {
        let t = Thresholds::default();
        // NaN и -INF → Grey (мгновенно)
        assert_eq!(t.classify(f64::NAN, false), BlockClass::Grey);
        assert_eq!(t.classify(-1.0, false), BlockClass::Grey);
        assert_eq!(t.classify(f64::NEG_INFINITY, false), BlockClass::Grey);
        // +INF → Bad (крайне медленно)
        assert_eq!(t.classify(f64::INFINITY, false), BlockClass::Bad);
    }

    #[test]
    fn test_presets() {
        for p in Thresholds::PRESETS {
            Thresholds::from_preset(p).unwrap().validate().unwrap();
        }
    }

    #[test]
    fn test_from_preset_case_insensitive() {
        assert!(Thresholds::from_preset("VICTORIA").is_some());
        assert!(Thresholds::from_preset("MHDD").is_some());
        assert!(Thresholds::from_preset("nope").is_none());
    }

    #[test]
    fn test_block_class_str() {
        assert_eq!(BlockClass::Bad.as_str(), "Bad");
        assert_eq!("red".parse::<BlockClass>().unwrap(), BlockClass::Red);
        assert!("unknown".parse::<BlockClass>().is_err());
    }

    #[test]
    fn test_thresholds_from_str() {
        let t: Thresholds = "10,50,200,1000".parse().unwrap();
        assert_eq!(t, Thresholds::default());

        let t2: Thresholds = "10 50 200 1000".parse().unwrap();
        assert_eq!(t2, Thresholds::default());

        let t3: Thresholds = "10;50;200;1000".parse().unwrap();
        assert_eq!(t3, Thresholds::default());

        assert!("10,50,200".parse::<Thresholds>().is_err());
        assert!("10,50,200,abc".parse::<Thresholds>().is_err());
        assert!("100,50,200,1000".parse::<Thresholds>().is_err());
    }

    #[test]
    fn test_thresholds_display_roundtrip() {
        let t = Thresholds::strict();
        let s = t.to_string();
        let t2: Thresholds = s.parse().unwrap();
        assert_eq!(t, t2);
    }

    #[test]
    fn test_bitor() {
        assert_eq!(BlockClass::Grey | BlockClass::Bad, BlockClass::Bad);
        assert_eq!(BlockClass::Green | BlockClass::Yellow, BlockClass::Yellow);
        assert_eq!(BlockClass::Red | BlockClass::Red, BlockClass::Red);
    }

    #[test]
    fn test_class_counts() {
        let mut c = ClassCounts::default();
        c.add(BlockClass::Grey);
        c.add(BlockClass::Grey);
        c.add(BlockClass::Bad);
        assert_eq!(c.grey, 2);
        assert_eq!(c.bad, 1);
        assert_eq!(c.total(), 3);
        assert!((c.problem_percent() - 33.333).abs() < 0.1);
        assert!((c.critical_percent() - 33.333).abs() < 0.1);
    }

    #[test]
    fn test_estimate_from_speed() {
        let t = Thresholds::estimate_from_speed(100.0);
        t.validate().unwrap();
        let t2 = Thresholds::estimate_from_speed(500.0);
        t2.validate().unwrap();
        assert!(t2.fast_ms < t.fast_ms);
    }

    #[test]
    fn test_block_class_symbol() {
        assert_eq!(BlockClass::Grey.symbol(), '·');
        assert_eq!(BlockClass::Bad.symbol(), 'X');
        assert_eq!(BlockClass::Green.symbol(), '■');
    }

    #[test]
    fn test_block_class_hex_color() {
        assert_eq!(BlockClass::Bad.hex_color(), 0xFF0000);
        assert_eq!(BlockClass::Grey.hex_color(), 0x808080);
    }

    #[test]
    fn test_block_class_is_problem() {
        assert!(!BlockClass::Grey.is_problem());
        assert!(!BlockClass::Green.is_problem());
        assert!(!BlockClass::Yellow.is_problem());
        assert!(BlockClass::Red.is_problem());
        assert!(BlockClass::Bad.is_problem());
    }

    #[test]
    fn test_block_class_is_critical() {
        assert!(!BlockClass::Red.is_critical());
        assert!(BlockClass::Bad.is_critical());
    }

    #[test]
    fn test_block_class_description() {
        assert!(!BlockClass::Grey.description().is_empty());
        assert!(!BlockClass::Bad.description().is_empty());
    }
}
