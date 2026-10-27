use std::{
    fs::File,
    io::{self, Read, Seek},
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
    #[error("limit exceeded: {0}")]
    Limit(String),
    #[error("invalid extension {0:?}")]
    InvalidExt(String),
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
        // file_type() does not follow symlinks; symlinks are skipped.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            scan_dir(&path, out);
            continue;
        }
        if !file_type.is_file() {
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

/// A book spooled to a temp file.
pub struct Output {
    pub file: tokio::fs::File,
    pub len: u64,
    /// Lowercase extension of the stored original (`[a-z0-9]+`).
    pub ext: String,
    /// The file is a zip wrapping the original (fb2 only).
    pub zipped: bool,
}

struct Spooled {
    file: File,
    len: u64,
    ext: String,
    zipped: bool,
}

/// Book lookup backend: INPX catalog, or archive-name range scan as fallback.
pub enum Catalog {
    Inpx(InpxIndex),
    Ranges(ArchiveIndex),
}

fn valid_ext(ext: &str) -> bool {
    !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn accepts(format: Option<&str>, ext: &str) -> bool {
    format.is_none_or(|f| f == ext)
}

/// Entries whose basename stem equals `stem` and whose lowercase extension is accepted.
fn search_stem(
    archive: &ZipArchive<File>,
    stem: &str,
    format: Option<&str>,
) -> Vec<(usize, String)> {
    let mut out = vec![];
    for i in 0..archive.len() {
        let Some(Ok(name)) = archive.name_for_index(i) else {
            continue;
        };
        if name.ends_with('/') {
            continue;
        }
        let base = name.rsplit('/').next().unwrap_or(&name);
        let Some((s, ext)) = base.rsplit_once('.') else {
            continue;
        };
        if s == stem {
            let ext = ext.to_lowercase();
            if accepts(format, &ext) {
                out.push((i, ext));
            }
        }
    }
    out
}

fn locate_inpx(
    archive: &ZipArchive<File>,
    id: u64,
    hit: &Hit,
    format: Option<&str>,
) -> Option<(usize, String)> {
    if let Some(index) = archive.index_for_name(&hit.entry) {
        return Some((index, hit.ext.clone()));
    }
    // Entry name differs from the catalog (e.g. extension case): search by the
    // catalog FILE basename; `{LIBID}.*` only when FILE equals LIBID.
    let id_s = id.to_string();
    let stem = if hit.file == id_s {
        id_s.as_str()
    } else {
        hit.file.rsplit('/').next().unwrap_or(&hit.file)
    };
    let mut found = search_stem(archive, stem, format);
    match found.len() {
        0 => None,
        1 => found.pop(),
        n => {
            warn!(
                "Ambiguous fallback for book {id} in {}: {n} entries match {stem:?}",
                hit.archive.display()
            );
            None
        }
    }
}

/// Finds and spools the book within ONE opened archive (no TOCTOU).
fn extract(
    path: &Path,
    id: u64,
    hit: Option<&Hit>,
    format: Option<&str>,
    max: u64,
) -> Result<Option<Spooled>, ArchiveError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            warn!("Archive {} is missing (still downloading?)", path.display());
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    let mut archive = ZipArchive::new(file)?;
    let found = match hit {
        Some(h) => locate_inpx(&archive, id, h, format),
        None => search_stem(&archive, &id.to_string(), format)
            .into_iter()
            .next(),
    };
    let Some((index, ext)) = found else {
        if hit.is_some() {
            warn!("Book {id} not found in archive {}", path.display());
        }
        return Ok(None);
    };
    if !valid_ext(&ext) || !accepts(format, &ext) {
        return Err(ArchiveError::InvalidExt(ext));
    }
    let zipped = ext == "fb2";
    let mut tmp = tempfile::tempfile()?;
    if zipped {
        let entry = archive.by_index_raw(index)?;
        if entry.compressed_size() > max || entry.size() > max {
            return Err(ArchiveError::Limit(format!("entry of book {id} too large")));
        }
        let mut writer = ZipWriter::new(&mut tmp);
        writer.raw_copy_file_rename(entry, format!("{id}.{ext}"))?;
        writer.finish()?;
    } else {
        let entry = archive.by_index(index)?;
        if entry.size() > max {
            return Err(ArchiveError::Limit(format!("entry of book {id} too large")));
        }
        let n = io::copy(&mut entry.take(max + 1), &mut tmp)?;
        if n > max {
            return Err(ArchiveError::Limit(format!("entry of book {id} too large")));
        }
    }
    let len = tmp.stream_position()?;
    tmp.rewind()?;
    Ok(Some(Spooled {
        file: tmp,
        len,
        ext,
        zipped,
    }))
}

impl Catalog {
    pub fn is_empty(&self) -> bool {
        match self {
            Catalog::Inpx(i) => i.is_empty(),
            Catalog::Ranges(r) => r.is_empty(),
        }
    }

    /// Finds book `id` (optionally only with lowercase extension `format`) and
    /// spools it to a temp file (wrapped in a zip for fb2). `Ok(None)` means
    /// not found; I/O and zip errors are returned as errors.
    pub async fn fetch(
        &self,
        id: u64,
        format: Option<&'static str>,
        max: u64,
    ) -> Result<Option<Output>, ArchiveError> {
        let spooled = match self {
            Catalog::Ranges(index) => {
                let mut first_err = None;
                let mut result = None;
                for path in index.candidates(id) {
                    let res =
                        tokio::task::spawn_blocking(move || extract(&path, id, None, format, max))
                            .await?;
                    match res {
                        Ok(Some(s)) => {
                            result = Some(s);
                            break;
                        }
                        Ok(None) => {}
                        Err(err) => {
                            warn!("Failed to read archive: {err}");
                            first_err.get_or_insert(err);
                        }
                    }
                }
                match (result, first_err) {
                    (Some(s), _) => Some(s),
                    (None, Some(err)) => return Err(err),
                    (None, None) => None,
                }
            }
            Catalog::Inpx(index) => {
                let Some(hit) = index.lookup(id) else {
                    return Ok(None);
                };
                if !accepts(format, &hit.ext) {
                    return Ok(None);
                }
                tokio::task::spawn_blocking(move || {
                    let path = hit.archive.clone();
                    extract(&path, id, Some(&hit), format, max)
                })
                .await??
            }
        };
        Ok(spooled.map(|s| Output {
            file: tokio::fs::File::from_std(s.file),
            len: s.len,
            ext: s.ext,
            zipped: s.zipped,
        }))
    }
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

    const MAX: u64 = 1 << 20;

    #[tokio::test]
    async fn scan_and_lookup() {
        let dir = tempfile::tempdir().unwrap();
        make_zip(&dir.path().join("fb2-1-100.zip"), &[("sub/5.fb2", b"x")]);
        make_zip(&dir.path().join("f.usr-101-200.zip"), &[("150.PDF", b"p")]);
        make_zip(&dir.path().join("junk.zip"), &[("1.fb2", b"x")]);
        let index = ArchiveIndex::scan(dir.path());
        assert_eq!(index.len(), 2);
        assert_eq!(index.candidates(150).len(), 1);
        let cat = Catalog::Ranges(index);
        let f = cat.fetch(5, None, MAX).await.unwrap().unwrap();
        assert_eq!(f.ext, "fb2");
        let f = cat.fetch(150, None, MAX).await.unwrap().unwrap();
        assert_eq!(f.ext, "pdf");
        assert!(cat.fetch(6, None, MAX).await.unwrap().is_none());
        assert!(cat.fetch(500, None, MAX).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn overlapping_ranges_with_format_filter() {
        let dir = tempfile::tempdir().unwrap();
        make_zip(&dir.path().join("a-1-100.zip"), &[("5.fb2", b"fb")]);
        make_zip(&dir.path().join("b-1-100.zip"), &[("5.epub", b"ep")]);
        let cat = Catalog::Ranges(ArchiveIndex::scan(dir.path()));
        let f = cat.fetch(5, Some("epub"), MAX).await.unwrap().unwrap();
        assert_eq!(f.ext, "epub");
        let f = cat.fetch(5, Some("fb2"), MAX).await.unwrap().unwrap();
        assert_eq!(f.ext, "fb2");
        assert!(cat.fetch(5, Some("mobi"), MAX).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn entry_cap_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        make_zip(&dir.path().join("a-1-100.zip"), &[("5.epub", &[0u8; 1000])]);
        let cat = Catalog::Ranges(ArchiveIndex::scan(dir.path()));
        assert!(cat.fetch(5, None, 10).await.is_err());
        assert!(cat.fetch(5, None, 1000).await.unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        make_zip(&real.path().join("a-1-100.zip"), &[("5.fb2", b"x")]);
        std::os::unix::fs::symlink(real.path(), dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(
            real.path().join("a-1-100.zip"),
            dir.path().join("b-1-100.zip"),
        )
        .unwrap();
        assert_eq!(ArchiveIndex::scan(dir.path()).len(), 0);
    }
}
