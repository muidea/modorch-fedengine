//! Process-local memory pressure sampling and bounded temp-directory spooling.
use modorch_fedengine_contract::EngineMetrics;
use modorch_fedengine_core::{EngineError, Result};
use std::{
    fs::{File, OpenOptions},
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MemoryPressure {
    Normal,
    Elevated,
    Critical,
}

#[derive(Default)]
struct Resident {
    rss_kib: Option<u64>,
    peak_kib: Option<u64>,
}
impl Resident {
    fn read() -> Self {
        let mut resident = Self::default();
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                let Some((name, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value.split_whitespace().next().and_then(|v| v.parse().ok());
                match name {
                    "VmRSS" => resident.rss_kib = value,
                    "VmHWM" => resident.peak_kib = value,
                    _ => {}
                }
            }
        }
        resident
    }
}

pub(super) fn parse_memory_limit_bytes(spec: &str) -> Option<u64> {
    let s = spec.trim().to_ascii_uppercase();
    let (num, mult) = if let Some(v) = s.strip_suffix("GIB").or_else(|| s.strip_suffix("GB")) {
        (v, 1024_u64 * 1024 * 1024)
    } else if let Some(v) = s.strip_suffix("MIB").or_else(|| s.strip_suffix("MB")) {
        (v, 1024_u64 * 1024)
    } else if let Some(v) = s.strip_suffix("KIB").or_else(|| s.strip_suffix("KB")) {
        (v, 1024_u64)
    } else if let Some(v) = s.strip_suffix('B') {
        (v, 1_u64)
    } else {
        (s.as_str(), 1_u64)
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mult)
}

pub(super) fn pressure_for_usage(rss_bytes: u64, budget_bytes: u64) -> MemoryPressure {
    if budget_bytes == 0 {
        return MemoryPressure::Normal;
    }
    let ratio_pct = rss_bytes.saturating_mul(100) / budget_bytes;
    if ratio_pct >= 90 {
        MemoryPressure::Critical
    } else if ratio_pct >= 75 {
        MemoryPressure::Elevated
    } else {
        MemoryPressure::Normal
    }
}

pub(super) fn sample_pressure(
    budget_bytes: u64,
    metrics: Option<&EngineMetrics>,
) -> (MemoryPressure, Option<u64>) {
    let rss_bytes = Resident::read().rss_kib.map(|kib| kib.saturating_mul(1024));
    let pressure = rss_bytes
        .map(|rss| pressure_for_usage(rss, budget_bytes))
        .unwrap_or(MemoryPressure::Normal);
    if pressure != MemoryPressure::Normal
        && let Some(m) = metrics
    {
        m.memory_pressure_events.fetch_add(1, Ordering::Relaxed);
    }
    (pressure, rss_bytes)
}

pub(super) fn cleanup_stale_spools(temp_dir: &str) {
    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("fedengine-spool-") && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Bounded temporary disk spool inside `duckdb.temp_directory` with strict quota
/// accounting and RAII cleanup on completion or failure.
pub(super) struct BoundedTempSpool {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    limit: u64,
}

impl BoundedTempSpool {
    pub(super) fn new(temp_dir: impl AsRef<Path>, limit_bytes: u64) -> Self {
        let path = temp_dir
            .as_ref()
            .join(format!("fedengine-spool-{}.tmp", uuid::Uuid::new_v4()));
        Self {
            path,
            file: None,
            written: 0,
            limit: limit_bytes,
        }
    }

    pub(super) fn write_chunk(
        &mut self,
        bytes: &[u8],
        metrics: Option<&EngineMetrics>,
    ) -> Result<()> {
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| EngineError::new("54000", "temporary spool byte counter overflow"))?;
        if next > self.limit {
            return Err(EngineError::new(
                "54000",
                "temporary spool exceeded configured disk budget",
            ));
        }
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent).map_err(|_| {
                    EngineError::new("53300", "cannot create temporary spool directory")
                })?;
            }
            let f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&self.path)
                .map_err(|_| EngineError::new("53300", "cannot open temporary spool file"))?;
            self.file = Some(f);
        }
        let file = self.file.as_mut().unwrap();
        file.write_all(bytes)
            .map_err(|_| EngineError::new("53300", "failed writing temporary spool"))?;
        self.written = next;
        if let Some(m) = metrics {
            m.spool_bytes_written
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    pub(super) fn bytes_written(&self) -> u64 {
        self.written
    }

    pub(super) fn open_reader(&mut self) -> Result<BufReader<File>> {
        if let Some(f) = self.file.as_mut() {
            f.flush()
                .map_err(|_| EngineError::new("53300", "failed flushing temporary spool"))?;
        }
        let file = File::open(&self.path)
            .map_err(|_| EngineError::new("53300", "temporary spool is unavailable"))?;
        Ok(BufReader::new(file))
    }

    pub(super) fn read_all(&mut self) -> Result<Vec<u8>> {
        if self.written == 0 {
            return Ok(Vec::new());
        }
        let mut reader = self.open_reader()?;
        let mut buf = Vec::with_capacity(self.written as usize);
        reader
            .read_to_end(&mut buf)
            .map_err(|_| EngineError::new("53300", "failed reading temporary spool"))?;
        Ok(buf)
    }
}

impl Drop for BoundedTempSpool {
    fn drop(&mut self) {
        self.file.take();
        if self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(super) struct Stage {
    name: &'static str,
    started: Instant,
    before: Resident,
}
impl Stage {
    pub(super) fn new(name: &'static str) -> Self {
        let before = Resident::read();
        tracing::debug!(
            phase = name,
            rss_kib = before.rss_kib,
            peak_rss_kib = before.peak_kib,
            "metadata phase started"
        );
        Self {
            name,
            started: Instant::now(),
            before,
        }
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let after = Resident::read();
        let delta = after
            .rss_kib
            .zip(self.before.rss_kib)
            .map(|(a, b)| a as i64 - b as i64);
        tracing::debug!(
            phase = self.name,
            elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.,
            rss_kib = after.rss_kib,
            peak_rss_kib = after.peak_kib,
            rss_delta_kib = delta,
            "metadata phase finished"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_limit_parsing_and_pressure_thresholds_behave_deterministically() {
        assert_eq!(parse_memory_limit_bytes("1GB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_memory_limit_bytes("512MB"), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory_limit_bytes("256KiB"), Some(256 * 1024));
        assert_eq!(pressure_for_usage(700, 1000), MemoryPressure::Normal);
        assert_eq!(pressure_for_usage(800, 1000), MemoryPressure::Elevated);
        assert_eq!(pressure_for_usage(920, 1000), MemoryPressure::Critical);
    }

    #[test]
    fn bounded_temp_spool_enforces_quota_and_cleans_up_on_drop() {
        let dir = std::env::temp_dir().join(format!("fedengine-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("fedengine-spool-stale.tmp");
        std::fs::write(&stale, b"stale").unwrap();
        cleanup_stale_spools(&dir.to_string_lossy());
        assert!(!stale.exists());

        let metrics = EngineMetrics::default();
        let path;
        {
            let mut spool = BoundedTempSpool::new(&dir, 10);
            path = spool.path.clone();
            spool.write_chunk(b"hello", Some(&metrics)).unwrap();
            spool.write_chunk(b"12345", Some(&metrics)).unwrap();
            assert_eq!(spool.bytes_written(), 10);
            assert_eq!(
                spool
                    .write_chunk(b"x", Some(&metrics))
                    .unwrap_err()
                    .sqlstate,
                "54000"
            );
            assert_eq!(spool.read_all().unwrap(), b"hello12345");
            assert!(path.exists());
        }
        assert!(!path.exists());
        assert_eq!(metrics.snapshot().spool_bytes_written, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
