//! Формат лога: offset_mb,elapsed_ms,class,status
//! Совместим с Victoria CSV-экспортом (можно парсить теми же скриптами).

use anyhow::Result;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct ScanLog {
    writer: BufWriter<File>,
}

impl ScanLog {
    pub fn create(path: &Path) -> Result<Self> {
        let f = File::create(path)?;
        let mut w = BufWriter::new(f);
        writeln!(w, "offset_mb,elapsed_ms,class,io_status")?;
        Ok(Self { writer: w })
    }

    pub fn write(
        &mut self,
        offset_mb: u64,
        elapsed_ms: f64,
        class: &str,
        io_ok: bool,
    ) -> Result<()> {
        writeln!(
            self.writer,
            "{},{:.3},{},{}",
            offset_mb,
            elapsed_ms,
            class,
            if io_ok { "OK" } else { "ERR" }
        )?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}
