//! Parquet files: written once to a temp name and renamed (never rewritten), read back
//! as journal rows for `lake rebuild`, and listed per table root.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use arrow::array::{Array, Int64Array, StringArray, TimestampMillisecondArray};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use rematch_core::ledger::JournalRow;

/// Writer settings shared by every lake file: full (untruncated) min/max statistics so
/// Bellwether can prune on exact string bounds; snappy pages.
fn props() -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_statistics_truncate_length(None)
        .set_column_index_truncate_length(None)
        .build()
}

/// Writes `batch` to `path` unless a file is already there (files are never rewritten).
/// Returns whether a file was written.
pub fn write_once(path: &Path, batch: &RecordBatch) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let dir = path.parent().ok_or_else(|| anyhow!("no parent for {}", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let res = (|| -> Result<()> {
        let f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let mut w = ArrowWriter::try_new(f, batch.schema(), Some(props()))?;
        w.write(batch)?;
        let f = w.into_inner()?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context(format!("write {}", path.display())));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(true)
}

/// Copies `src` to `dst` through a temp name unless `dst` exists. Returns whether it copied.
pub fn copy_once(src: &Path, dst: &Path) -> Result<bool> {
    if dst.exists() {
        return Ok(false);
    }
    let dir = dst.parent().ok_or_else(|| anyhow!("no parent for {}", dst.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = dst.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    std::fs::copy(src, &tmp).with_context(|| format!("copy {} → {}", src.display(), tmp.display()))?;
    std::fs::rename(&tmp, dst).with_context(|| format!("rename into {}", dst.display()))?;
    Ok(true)
}

/// Every record batch of a parquet file.
pub fn read_batches(path: &Path) -> Result<Vec<RecordBatch>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(f)
        .with_context(|| format!("read {}", path.display()))?
        .build()?;
    reader.collect::<std::result::Result<Vec<_>, _>>().with_context(|| format!("decode {}", path.display()))
}

fn column<'a, T: 'static>(b: &'a RecordBatch, name: &str, path: &Path) -> Result<&'a T> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<T>())
        .ok_or_else(|| anyhow!("{}: column {name} missing or mistyped", path.display()))
}

/// The journal rows stored in one append-table file.
pub fn read_journal_rows(path: &Path, table: &str) -> Result<Vec<JournalRow>> {
    let mut out = Vec::new();
    for b in read_batches(path)? {
        let seq = column::<Int64Array>(&b, "seq", path)?;
        let known = column::<TimestampMillisecondArray>(&b, "known", path)?;
        let machine = column::<StringArray>(&b, "machine", path)?;
        let op = column::<StringArray>(&b, "op", path)?;
        let row_key = column::<StringArray>(&b, "row_key", path)?;
        let json = column::<StringArray>(&b, "json", path)?;
        for r in 0..b.num_rows() {
            if seq.is_null(r) || json.is_null(r) {
                continue;
            }
            out.push(JournalRow {
                seq: seq.value(r),
                known: known.value(r),
                machine: machine.value(r).to_string(),
                table: table.to_string(),
                row_key: row_key.value(r).to_string(),
                op: op.value(r).to_string(),
                json: serde_json::from_str(json.value(r))
                    .with_context(|| format!("{}: row seq {} json", path.display(), seq.value(r)))?,
            });
        }
    }
    Ok(out)
}

/// Every file under `dir` (recursively, sorted), skipping nothing; callers classify.
pub fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", d.display())),
        };
        for e in rd {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columns::{batch, opt, req, Cell, Ty};

    #[test]
    fn files_are_written_once_deterministically_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let cols = [req("seq", Ty::Int64), req("known", Ty::TsMs), req("machine", Ty::Utf8), req("op", Ty::Utf8),
                    req("row_key", Ty::Utf8), opt("tags", Ty::ListUtf8), req("json", Ty::Utf8)];
        let long = "x".repeat(300);
        let rows = vec![vec![Cell::I(7), Cell::I(1_000), Cell::S("m".into()), Cell::S("insert".into()), Cell::S(long.clone()),
                             Cell::L(vec!["a".into(), "b".into()]), Cell::S(r#"{"id":"e1"}"#.into())]];
        let b = batch(&cols, &rows).unwrap();
        let (p, q) = (dir.path().join("a/m-7.parquet"), dir.path().join("b/m-7.parquet"));
        assert!(write_once(&p, &b).unwrap());
        assert!(!write_once(&p, &b).unwrap(), "never rewritten");
        assert!(write_once(&q, &b).unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), std::fs::read(&q).unwrap(), "same rows, same bytes");
        let rs = read_journal_rows(&p, "entries").unwrap();
        assert_eq!((rs[0].seq, rs[0].known, rs[0].row_key.as_str(), rs[0].json["id"].as_str()), (7, 1_000, long.as_str(), Some("e1")));
        let meta = ParquetRecordBatchReaderBuilder::try_new(File::open(&p).unwrap()).unwrap().metadata().clone();
        let stats = meta.row_group(0).column(4).statistics().unwrap();
        assert_eq!(stats.max_bytes_opt().unwrap().len(), 300, "statistics are not truncated");
        assert!(std::fs::read_dir(p.parent().unwrap()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with('.')));
    }
}
