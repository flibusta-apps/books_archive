use std::{
    fs::File,
    io::{self, Seek},
    path::{Path, PathBuf},
};

use super::inpx::{Hit, InpxIndex};
use thiserror::Error;
use tracing::warn;
use zip::{ZipArchive, ZipWriter};

#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error("task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRange {
    pub start: u64,
    pub end: u64,
    pub path: PathBuf,
}

/// Parses `...-{start}-{end}.zip` file names.
pub fn parse_range(file_name: &str) -> Option<(u64, u64)> {
    let stem = file_name
        .strip_suffix(".zip")
        .or_else(|| file_name.strip_suffix(".ZIP"))?;
    let mut parts = stem.rsplitn(3, '-');
    let end = parts.next()?.parse().ok()?;
    let start = parts.next()?.parse().ok()?;
    parts.next()?;
    if start > end {
        return None;
    }
    Some((start, end))
}

#[derive(Debug, Default)]
pub struct ArchiveIndex {
    ranges: Vec<ArchiveRange>,
}

impl ArchiveIndex {
    pub fn scan(dir: &Path) -> ArchiveIndex {
        let mut ranges = vec![];
        scan_dir(dir, &mut ranges);
        ranges.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.path.cmp(&b.path)));
        ArchiveIndex { ranges }
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn candidates(&self, id: u64) -> Vec<PathBuf> {
        self.ranges
            .iter()
            .filter(|r| r.start <= id && id <= r.end)
            .map(|r| r.path.clone())
            .collect()
    }
}

fn scan_dir(dir: &Path, out: &mut Vec<ArchiveRange>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            warn!("Cannot read dir {}: {err}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, out);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.to_lowercase().ends_with(".zip") {
            continue;
        }
        match parse_range(name) {
            Some((start, end)) => out.push(ArchiveRange { start, end, path }),
            None => warn!("Ignoring archive without id range: {}", path.display()),
        }
    }
}

/// A book found in an archive.
pub struct Found {
    /// Lowercase extension of the stored original.
    pub ext: String,
    /// Source archive and entry index.
    archive: PathBuf,
    index: usize,
}

pub struct Output {
    pub file: tokio::fs::File,
    pub len: u64,
}

fn find_in_archive(path: &Path, id: u64) -> Result<Option<Found>, ArchiveError> {
    let archive = ZipArchive::new(File::open(path)?)?;
    let id_s = id.to_string();
    for i in 0..archive.len() {
        let Some(Ok(name)) = archive.name_for_index(i) else {
            continue;
        };
        if name.ends_with('/') {
            continue;
        }
        let base = name.rsplit('/').next().unwrap_or(&name);
        let Some((stem, ext)) = base.rsplit_once('.') else {
            continue;
        };
        if stem == id_s {
            return Ok(Some(Found {
                ext: ext.to_lowercase(),
                archive: path.to_path_buf(),
                index: i,
            }));
        }
    }
    Ok(None)
}

/// Book lookup backend: INPX catalog, or archive-name range scan as fallback.
pub enum Catalog {
    Inpx(InpxIndex),
    Ranges(ArchiveIndex),
}

fn find_by_entry(path: &Path, id: u64, hit: Hit) -> Result<Option<Found>, ArchiveError> {
    let archive = ZipArchive::new(File::open(path)?)?;
    if let Some(index) = archive.index_for_name(&hit.entry) {
        return Ok(Some(Found {
            ext: hit.ext,
            archive: path.to_path_buf(),
            index,
        }));
    }
    // Entry name differs from the catalog: fall back to basename-stem search.
    drop(archive);
    find_in_archive(path, id)
}

impl Catalog {
    /// Finds the book `id`; `accept` is called with its lowercase extension.
    /// With INPX the extension is known up front, so a rejected extension
    /// returns `None` without touching the disk.
    pub async fn find(
        &self,
        id: u64,
        accept: impl Fn(&str) -> bool,
    ) -> Result<Option<Found>, ArchiveError> {
        match self {
            Catalog::Ranges(index) => Ok(find(index, id).await?.filter(|f| accept(&f.ext))),
            Catalog::Inpx(index) => {
                let Some(hit) = index.lookup(id) else {
                    return Ok(None);
                };
                if !accept(&hit.ext) {
                    return Ok(None);
                }
                let path = hit.archive.clone();
                let p = path.clone();
                let res = tokio::task::spawn_blocking(move || {
                    if !p.is_file() {
                        warn!("Archive {} is missing (still downloading?)", p.display());
                        return Ok(None);
                    }
                    find_by_entry(&p, id, hit)
                })
                .await?;
                match res {
                    Ok(Some(f)) => Ok(Some(f)),
                    Ok(None) => {
                        warn!("Book {id} not found in archive {}", path.display());
                        Ok(None)
                    }
                    Err(err) => {
                        warn!("Cannot read archive {}: {err}", path.display());
                        Ok(None)
                    }
                }
            }
        }
    }
}

pub async fn find(index: &ArchiveIndex, id: u64) -> Result<Option<Found>, ArchiveError> {
    for path in index.candidates(id) {
        let res = tokio::task::spawn_blocking(move || find_in_archive(&path, id)).await?;
        match res {
            Ok(Some(f)) => return Ok(Some(f)),
            Ok(None) => {}
            Err(err) => warn!("Failed to read archive: {err}"),
        }
    }
    Ok(None)
}

fn spool(found: Found, id: u64, as_zip: bool) -> Result<(File, u64), ArchiveError> {
    let mut archive = ZipArchive::new(File::open(&found.archive)?)?;
    let mut tmp = tempfile::tempfile()?;
    if as_zip {
        let entry = archive.by_index_raw(found.index)?;
        let mut writer = ZipWriter::new(&mut tmp);
        writer.raw_copy_file_rename(entry, format!("{id}.{}", found.ext))?;
        writer.finish()?;
    } else {
        let mut entry = archive.by_index(found.index)?;
        io::copy(&mut entry, &mut tmp)?;
    }
    let len = tmp.stream_position()?;
    tmp.rewind()?;
    Ok((tmp, len))
}

/// Spools the book (raw, or wrapped in a zip with raw-copied entry) to a temp file.
pub async fn open(found: Found, id: u64, as_zip: bool) -> Result<Output, ArchiveError> {
    let (file, len) = tokio::task::spawn_blocking(move || spool(found, id, as_zip)).await??;
    Ok(Output {
        file: tokio::fs::File::from_std(file),
        len,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, CompressionMethod};

    pub fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            w.start_file(
                *name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("f.fb2-000024-030559.zip"), Some((24, 30559)));
        assert_eq!(parse_range("f.usr-000001-000010.zip"), Some((1, 10)));
        assert_eq!(parse_range("fb2-123456-234567.zip"), Some((123456, 234567)));
        assert_eq!(parse_range("fb2-10-5.zip"), None);
        assert_eq!(parse_range("books.zip"), None);
        assert_eq!(parse_range("a-1-2.tar"), None);
        assert_eq!(parse_range("1-2.zip"), None);
    }

    #[tokio::test]
    async fn scan_and_lookup() {
        let dir = tempfile::tempdir().unwrap();
        make_zip(&dir.path().join("fb2-1-100.zip"), &[("sub/5.fb2", b"x")]);
        make_zip(&dir.path().join("f.usr-101-200.zip"), &[("150.PDF", b"p")]);
        make_zip(&dir.path().join("junk.zip"), &[("1.fb2", b"x")]);
        let index = ArchiveIndex::scan(dir.path());
        assert_eq!(index.len(), 2);
        assert_eq!(index.candidates(150).len(), 1);
        let f = find(&index, 5).await.unwrap().unwrap();
        assert_eq!(f.ext, "fb2");
        let f = find(&index, 150).await.unwrap().unwrap();
        assert_eq!(f.ext, "pdf");
        assert!(find(&index, 6).await.unwrap().is_none());
        assert!(find(&index, 500).await.unwrap().is_none());
    }
}
