//! INPX catalog support.
//!
//! An `.inpx` is a zip with `collection.info`, `version.info` and many `*.inp`
//! files. `X.inp` describes the books stored in the sibling archive `X.zip`.
//! Each line holds fields separated by `\x04`:
//! AUTHOR, GENRE, TITLE, SERIES, SERNO, FILE, SIZE, LIBID, DEL, EXT, DATE, ...
//! The zip entry name is `{FILE}.{EXT}`; the lookup key is LIBID.
//!
//! Duplicate LIBIDs: a non-deleted (DEL=0) row always wins over a deleted one;
//! among rows of equal deletion state the last one parsed wins.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek},
    path::{Path, PathBuf},
};

use tracing::warn;
use zip::ZipArchive;

use super::archive::ArchiveError;

/// Longest accepted `.inp` line; longer lines are skipped as malformed.
const MAX_LINE: usize = 64 * 1024;
/// Largest accepted `version.info` / `structure.info`.
const MAX_INFO: u64 = 64 * 1024;
/// Cap on the number of indexed books.
const MAX_BOOKS: usize = 10_000_000;

/// Field positions inside an `.inp` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    file: usize,
    libid: usize,
    del: usize,
    ext: usize,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            file: 5,
            libid: 7,
            del: 8,
            ext: 9,
        }
    }
}

impl Layout {
    /// Parses `structure.info` (`AUTHOR;GENRE;...;FILE;SIZE;LIBID;DEL;EXT;...`).
    fn from_structure(text: &str) -> Layout {
        let names: Vec<String> = text
            .trim()
            .split(';')
            .map(|n| n.trim().to_ascii_uppercase())
            .collect();
        let pos = |n: &str| names.iter().position(|x| x == n);
        match (pos("FILE"), pos("LIBID"), pos("DEL"), pos("EXT")) {
            (Some(file), Some(libid), Some(del), Some(ext)) => {
                let l = Layout {
                    file,
                    libid,
                    del,
                    ext,
                };
                if l != Layout::default() {
                    warn!("structure.info differs from the default field order: {l:?}");
                }
                l
            }
            _ => {
                warn!("structure.info lacks FILE/LIBID/DEL/EXT; using the default field order");
                Layout::default()
            }
        }
    }
}

struct Book {
    /// Entry file stem; empty when it equals the LIBID (the common case).
    file: Box<str>,
    archive: u32,
    ext: u16,
    deleted: bool,
}

#[derive(Default)]
pub struct InpxIndex {
    archives: Vec<PathBuf>,
    /// Interned extensions, original case as in the catalog.
    exts: Vec<Box<str>>,
    books: HashMap<u32, Book>,
    pub version: String,
    pub malformed: usize,
}

/// Result of an INPX lookup.
pub struct Hit {
    pub archive: PathBuf,
    /// Catalog FILE field (entry stem).
    pub file: String,
    pub entry: String,
    /// Lowercase extension.
    pub ext: String,
}

impl InpxIndex {
    pub fn len(&self) -> usize {
        self.books.len()
    }

    pub fn is_empty(&self) -> bool {
        self.books.is_empty()
    }

    pub fn archives(&self) -> usize {
        self.archives.len()
    }

    pub fn lookup(&self, id: u64) -> Option<Hit> {
        let id32 = u32::try_from(id).ok()?;
        let b = self.books.get(&id32)?;
        let ext = &self.exts[b.ext as usize];
        let entry = if b.file.is_empty() {
            format!("{id}.{ext}")
        } else {
            format!("{}.{ext}", b.file)
        };
        Some(Hit {
            archive: self.archives[b.archive as usize].clone(),
            file: if b.file.is_empty() {
                id.to_string()
            } else {
                b.file.to_string()
            },
            entry,
            ext: ext.to_lowercase(),
        })
    }
}

struct Parsed<'a> {
    file: &'a str,
    libid: u32,
    deleted: bool,
    ext: &'a str,
}

fn parse_line<'a>(line: &'a [u8], l: &Layout) -> Option<Parsed<'a>> {
    let max = l.file.max(l.libid).max(l.del).max(l.ext);
    let (mut file, mut libid, mut del, mut ext) = (None, None, None, None);
    for (i, f) in line.split(|b| *b == 4).enumerate() {
        if i == l.file {
            file = Some(f);
        }
        if i == l.libid {
            libid = Some(f);
        }
        if i == l.del {
            del = Some(f);
        }
        if i == l.ext {
            ext = Some(f);
        }
        if i >= max {
            break;
        }
    }
    let file = std::str::from_utf8(file?).ok()?;
    let libid = std::str::from_utf8(libid?).ok()?.trim().parse().ok()?;
    let deleted = match del? {
        b"0" | b"" => false,
        b"1" => true,
        _ => return None,
    };
    let ext = std::str::from_utf8(ext?).ok()?.trim();
    if file.is_empty() || ext.is_empty() {
        return None;
    }
    Some(Parsed {
        file,
        libid,
        deleted,
        ext,
    })
}

/// Reads one line (without terminator) of at most `MAX_LINE` bytes.
/// Returns `Ok(None)` at EOF and `Ok(Some(false))` for an over-long line (skipped).
fn read_line(r: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<Option<bool>> {
    buf.clear();
    let n = r
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        return Ok(Some(true));
    }
    if buf.len() <= MAX_LINE {
        return Ok(Some(true)); // last line without newline
    }
    // Over-long: discard the rest of the line.
    loop {
        let (done, used) = {
            let chunk = r.fill_buf()?;
            if chunk.is_empty() {
                break;
            }
            match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => (true, i + 1),
                None => (false, chunk.len()),
            }
        };
        r.consume(used);
        if done {
            break;
        }
    }
    Ok(Some(false))
}

impl InpxIndex {
    fn add_inp(
        &mut self,
        mut data: impl BufRead,
        archive: u32,
        layout: &Layout,
    ) -> Result<(), ArchiveError> {
        let mut ext_ids: HashMap<String, u16> = HashMap::new();
        for (i, e) in self.exts.iter().enumerate() {
            ext_ids.insert(e.to_string(), i as u16);
        }
        let mut buf = Vec::new();
        while let Some(ok) = read_line(&mut data, &mut buf)? {
            if !ok {
                self.malformed += 1;
                continue;
            }
            let line = buf.strip_suffix(b"\r").unwrap_or(&buf);
            if line.is_empty() {
                continue;
            }
            let Some(p) = parse_line(line, layout) else {
                self.malformed += 1;
                continue;
            };
            match self.books.get(&p.libid) {
                Some(old) => {
                    if !old.deleted && p.deleted {
                        continue;
                    }
                }
                None if self.books.len() >= MAX_BOOKS => {
                    return Err(ArchiveError::Limit(format!(
                        "INPX has more than {MAX_BOOKS} books"
                    )));
                }
                None => {}
            }
            let ext = match ext_ids.get(p.ext) {
                Some(i) => *i,
                None => {
                    let Ok(i) = u16::try_from(self.exts.len()) else {
                        self.malformed += 1;
                        continue;
                    };
                    self.exts.push(p.ext.into());
                    ext_ids.insert(p.ext.to_string(), i);
                    i
                }
            };
            let file = if p.file == p.libid.to_string() {
                "".into()
            } else {
                p.file.into()
            };
            self.books.insert(
                p.libid,
                Book {
                    file,
                    archive,
                    ext,
                    deleted: p.deleted,
                },
            );
        }
        Ok(())
    }
}

/// Loads an index from an `.inpx` file. Archives are resolved relative to its directory.
pub fn load(inpx: &Path) -> Result<InpxIndex, ArchiveError> {
    let dir = inpx.parent().unwrap_or(Path::new("."));
    load_from(File::open(inpx)?, dir)
}

pub fn load_from<R: Read + Seek>(reader: R, dir: &Path) -> Result<InpxIndex, ArchiveError> {
    let mut zip = ZipArchive::new(reader)?;
    let mut index = InpxIndex {
        version: "unknown".into(),
        ..Default::default()
    };
    // First pass: small info files (order inside the zip is arbitrary).
    let mut layout = Layout::default();
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name()?.into_owned();
        let base = name.rsplit('/').next().unwrap_or(&name);
        let is_version = base.eq_ignore_ascii_case("version.info");
        let is_structure = base.eq_ignore_ascii_case("structure.info");
        if !is_version && !is_structure {
            continue;
        }
        let mut buf = Vec::new();
        entry.take(MAX_INFO + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_INFO {
            warn!("{base} is larger than {MAX_INFO} bytes; ignored");
            continue;
        }
        let text = String::from_utf8_lossy(&buf).trim().to_string();
        if is_version {
            if !text.is_empty() {
                index.version = text;
            }
        } else {
            layout = Layout::from_structure(&text);
        }
    }
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name()?.into_owned();
        let base = name.rsplit('/').next().unwrap_or(&name);
        let Some(stem) = base
            .strip_suffix(".inp")
            .or_else(|| base.strip_suffix(".INP"))
        else {
            continue;
        };
        let archive = u32::try_from(index.archives.len())
            .map_err(|_| ArchiveError::Limit("too many archives".into()))?;
        index.archives.push(dir.join(format!("{stem}.zip")));
        index.add_inp(BufReader::new(entry), archive, &layout)?;
    }
    if index.malformed > 0 {
        warn!("Skipped {} malformed INPX lines", index.malformed);
    }
    Ok(index)
}

fn inpx_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("inpx"))
        })
        .collect();
    v.sort();
    v
}

/// Finds the single `.inpx` in `dir` (non-recursive), else one level deep.
/// Several candidates are an error: the caller must set `INPX_PATH`.
/// Only directory entries are listed; archives are never opened.
pub fn discover(dir: &Path) -> Result<Option<PathBuf>, String> {
    let mut found = inpx_in(dir);
    if found.is_empty() {
        let mut subdirs: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect(),
            Err(_) => return Ok(None),
        };
        subdirs.sort();
        for d in subdirs {
            found.extend(inpx_in(&d));
        }
    }
    if found.len() > 1 {
        let list: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
        return Err(format!(
            "several .inpx files found ({}); set INPX_PATH to choose one",
            list.join(", ")
        ));
    }
    Ok(found.into_iter().next())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::services::archive::tests::make_zip;

    pub fn line(file: &str, libid: &str, del: &str, ext: &str) -> String {
        format!(
            "Author,A,B:\x04sf:\x04Title\x04Series\x042\x04{file}\x04100\x04{libid}\x04{del}\x04{ext}\x042008-07-05\x04ru\x043\x04\x04"
        )
    }

    #[test]
    fn parses_line_with_trailing_sep() {
        let l = line("110119", "1230745", "0", "fb2");
        let p = parse_line(l.as_bytes(), &Layout::default()).unwrap();
        assert_eq!(p.file, "110119");
        assert_eq!(p.libid, 1230745);
        assert!(!p.deleted);
        assert_eq!(p.ext, "fb2");
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_line(b"a\x04b\x04c", &Layout::default()).is_none());
        assert!(parse_line(line("1", "abc", "0", "fb2").as_bytes(), &Layout::default()).is_none());
        assert!(parse_line(line("1", "5", "0", "").as_bytes(), &Layout::default()).is_none());
        assert!(parse_line(line("1", "5", "x", "fb2").as_bytes(), &Layout::default()).is_none());
    }

    #[test]
    fn multiple_inp_map_to_archives() {
        let a = format!(
            "{}\r\n{}\r\nbroken line\r\n\r\n",
            line("100", "100", "0", "fb2"),
            line("weird", "101", "0", "EPUB")
        );
        let b = format!(
            "{}\n{}\n{}",
            line("200", "200", "0", "fb2"),
            line("a", "300", "1", "fb2"),
            line("b", "300", "0", "mobi"),
        );
        let c = format!("{}\n", line("zzz", "300", "1", "fb2"));
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.inpx");
        make_zip(
            &p,
            &[
                ("version.info", b"20261001\r\n"),
                ("d.fb2-1-100.inp", a.as_bytes()),
                ("d.fb2-101-200.inp", b.as_bytes()),
                ("d.fb2-201-300.inp", c.as_bytes()),
                ("collection.info", b"x"),
            ],
        );
        let idx = load(&p).unwrap();
        assert_eq!(idx.version, "20261001");
        assert_eq!(idx.malformed, 1);
        assert_eq!(idx.len(), 4);
        assert_eq!(idx.archives(), 3);
        let h = idx.lookup(100).unwrap();
        assert_eq!(h.archive, dir.path().join("d.fb2-1-100.zip"));
        assert_eq!(h.entry, "100.fb2");
        let h = idx.lookup(101).unwrap();
        assert_eq!(h.entry, "weird.EPUB");
        assert_eq!(h.ext, "epub");
        let h = idx.lookup(200).unwrap();
        assert_eq!(h.archive, dir.path().join("d.fb2-101-200.zip"));
        // non-deleted wins over deleted, regardless of order
        let h = idx.lookup(300).unwrap();
        assert_eq!(h.entry, "b.mobi");
        assert!(idx.lookup(999).is_none());
        assert!(idx.lookup(u64::MAX).is_none());
    }

    #[test]
    fn discovers_inpx() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(discover(dir.path()), Ok(None));
        let sub = dir.path().join("fb2.X");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.inpx"), b"").unwrap();
        assert_eq!(discover(dir.path()), Ok(Some(sub.join("a.inpx"))));
        std::fs::write(dir.path().join("top.inpx"), b"").unwrap();
        assert_eq!(discover(dir.path()), Ok(Some(dir.path().join("top.inpx"))));
        std::fs::write(dir.path().join("top2.inpx"), b"").unwrap();
        assert!(discover(dir.path()).is_err());
    }

    #[test]
    fn honors_structure_info_and_line_limit() {
        let long = "x".repeat(MAX_LINE + 10);
        let a = format!("{long}\n9\x04ep\x04fileX\x040\x04pdf\n",);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.inpx");
        make_zip(
            &p,
            &[
                ("a.inp", a.as_bytes()),
                ("structure.info", b"LIBID;GENRE;FILE;DEL;EXT"),
            ],
        );
        let idx = load(&p).unwrap();
        assert_eq!(idx.malformed, 1);
        let h = idx.lookup(9).unwrap();
        assert_eq!(h.entry, "fileX.pdf");
    }
}
