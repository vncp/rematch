//! The lake (ADR 001): the ledger's journal and closed sessions' traces as immutable
//! Parquet under `REMATCH_ROOT/lake/`, one root per table, written by `rematch lake export`
//! and read only by historical commands (`lake rebuild`, `lake doctor`, `lake query`,
//! `report`). Bellwether indexes over it live in `REMATCH_ROOT/lake-index/` (L3, `index`,
//! `query`). Nothing in a session reads or writes it.

pub mod columns;
pub mod events;
pub mod index;
pub mod io;
pub mod ops;
pub mod query;
pub mod tables;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The Arrow and Parquet crates the lake is written with (readers and tests use the same).
pub use arrow;
pub use parquet;
use serde::{Deserialize, Serialize};

pub use ops::{doctor, export, import, rebuild, EventSource, ExportOpts};

/// Layout version written to `manifest.json`.
pub const LAKE_VERSION: u32 = 1;

/// `REMATCH_ROOT/lake`.
pub fn lake_dir(root: &Path) -> PathBuf {
    root.join("lake")
}

/// `lake/manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub lake_version: u32,
    /// The Bellwether version the indexes were built with (`null` until `lake index`, L3).
    pub bellwether_version_pinned: Option<String>,
    /// Exporter watermark per machine: the last journal `seq` whose rows are in the lake.
    #[serde(default)]
    pub watermarks: BTreeMap<String, i64>,
    /// Sessions already written under `events/` (from a trace or a harness transcript).
    #[serde(default)]
    pub events_exported: BTreeMap<String, bool>,
    /// Ledger sessions with neither a trace nor a harness transcript to read when last
    /// tried: skipped by bounded runs, retried by `lake export --all`.
    #[serde(default)]
    pub events_missing: BTreeSet<String>,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest {
            lake_version: LAKE_VERSION,
            bellwether_version_pinned: None,
            watermarks: BTreeMap::new(),
            events_exported: BTreeMap::new(),
            events_missing: BTreeSet::new(),
        }
    }
}

impl Manifest {
    pub fn path(lake: &Path) -> PathBuf {
        lake.join("manifest.json")
    }

    /// The manifest, or a fresh one when the lake has none yet.
    pub fn load(lake: &Path) -> Result<Manifest> {
        let p = Self::path(lake);
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
        }
    }

    /// Atomic replace (temp file + rename).
    pub fn save(&self, lake: &Path) -> Result<()> {
        std::fs::create_dir_all(lake).with_context(|| format!("create {}", lake.display()))?;
        let p = Self::path(lake);
        let tmp = lake.join(format!(".manifest.json.tmp-{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &p).with_context(|| format!("rename into {}", p.display()))?;
        Ok(())
    }
}
