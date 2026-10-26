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
    io::{Read, Seek},
    path::{Path, PathBuf},
};

use tracing::warn;
use zip::ZipArchive;

use super::archive::ArchiveError;

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

fn parse_line(line: &[u8]) -> Option<Parsed<'_>> {
    let mut f = line.split(|b| *b == 4);
    let file = std::str::from_utf8(f.nth(5)?).ok()?;
    let _size = f.next()?;
    let libid = std::str::from_utf8(f.next()?).ok()?.trim().parse().ok()?;
    let deleted = match f.next()? {
        b"0" | b"" => false,
        b"1" => true,
        _ => return None,
    };
    let ext = std::str::from_utf8(f.next()?).ok()?.trim();
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

impl InpxIndex {
    fn add_inp(&mut self, data: &[u8], archive: u32) {
        let mut ext_ids: HashMap<String, u16> = HashMap::new();
        for (i, e) in self.exts.iter().enumerate() {
            ext_ids.insert(e.to_string(), i as u16);
        }
        for raw in data.split(|b| *b == b'\n') {
            let line = raw.strip_suffix(b"\r").unwrap_or(raw);
            if line.is_empty() {
                continue;
            }
            let Some(p) = parse_line(line) else {
                self.malformed += 1;
                continue;
            };
            if let Some(old) = self.books.get(&p.libid) {
                if !old.deleted && p.deleted {
                    continue;
                }
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
    let mut buf = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let base = name.rsplit('/').next().unwrap_or(&name);
        buf.clear();
        if base.eq_ignore_ascii_case("version.info") {
            entry.read_to_end(&mut buf)?;
            let v = String::from_utf8_lossy(&buf).trim().to_string();
            if !v.is_empty() {
                index.version = v;
            }
            continue;
        }
        let Some(stem) = base
            .strip_suffix(".inp")
            .or_else(|| base.strip_suffix(".INP"))
        else {
            continue;
        };
        entry.read_to_end(&mut buf)?;
        let archive = u32::try_from(index.archives.len()).expect("too many archives");
        index.archives.push(dir.join(format!("{stem}.zip")));
        index.add_inp(&buf, archive);
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

/// Finds an `.inpx` in `dir` (non-recursive), then one level deep.
/// Only directory entries are listed; archives are never opened.
pub fn discover(dir: &Path) -> Option<PathBuf> {
    let mut found = inpx_in(dir);
    if found.is_empty() {
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();
        for d in subdirs {
            found.extend(inpx_in(&d));
            if !found.is_empty() {
                break;
            }
        }
    }
    if found.len() > 1 {
        warn!("Several .inpx files found, using {}", found[0].display());
    }
    found.into_iter().next()
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
        let p = parse_line(l.as_bytes()).unwrap();
        assert_eq!(p.file, "110119");
        assert_eq!(p.libid, 1230745);
        assert!(!p.deleted);
        assert_eq!(p.ext, "fb2");
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_line(b"a\x04b\x04c").is_none());
        assert!(parse_line(line("1", "abc", "0", "fb2").as_bytes()).is_none());
        assert!(parse_line(line("1", "5", "0", "").as_bytes()).is_none());
        assert!(parse_line(line("1", "5", "x", "fb2").as_bytes()).is_none());
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
        assert!(discover(dir.path()).is_none());
        let sub = dir.path().join("fb2.X");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.inpx"), b"").unwrap();
        assert_eq!(discover(dir.path()), Some(sub.join("a.inpx")));
        std::fs::write(dir.path().join("top.inpx"), b"").unwrap();
        assert_eq!(discover(dir.path()), Some(dir.path().join("top.inpx")));
    }
}
