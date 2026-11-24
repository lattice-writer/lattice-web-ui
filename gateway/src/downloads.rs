//! Files a browser tab saves, handed to the device it runs on.
//!
//! Lattice's "Save … as" asks for a place on the host's disk in a panel on
//! the host's display, which nobody can see. bridge.js asks the gateway for a
//! destination instead: a fresh folder here, which Lattice writes the file to.
//! Once Lattice's command returns, the tab fetches the file as a download.
//! A file goes ten minutes after it was first fetched, and every folder older
//! than a day goes when the next one is made (and when the gateway starts).

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A file is fetched again within this time (a browser may ask twice).
pub const KEEP_AFTER_FETCH: Duration = Duration::from_secs(600);
const KEEP_UNFETCHED: Duration = Duration::from_secs(24 * 3600);
const MAX_BYTES: u64 = 1024 * 1024 * 1024;

pub struct Downloads {
    dir: PathBuf,
}

pub struct Saved {
    pub name: String,
    pub bytes: Vec<u8>,
}

impl Downloads {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::remove_dir_all(&dir);
        Downloads { dir }
    }

    /// A new, empty folder and the path in it Lattice should write `name` to.
    pub fn destination(&self, name: &str) -> Result<(String, PathBuf), String> {
        self.sweep();
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(&self.dir)
            .map_err(|e| format!("Could not make the downloads folder: {e}"))?;
        let mut random = [0u8; 12];
        getrandom::getrandom(&mut random).map_err(|e| e.to_string())?;
        let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let folder = self.dir.join(&id);
        builder
            .recursive(false)
            .create(&folder)
            .map_err(|e| format!("Could not make a download folder: {e}"))?;
        Ok((id, folder.join(file_name(name))))
    }

    /// The file Lattice wrote for `id`, if it is there yet.
    pub fn take(&self, id: &str) -> Option<Saved> {
        if id.len() != 24 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let entry = fs::read_dir(self.dir.join(id))
            .ok()?
            .filter_map(Result::ok)
            .find(|e| e.file_type().is_ok_and(|t| t.is_file()))?;
        if entry.metadata().ok()?.len() > MAX_BYTES {
            return None;
        }
        let bytes = fs::read(entry.path()).ok()?;
        Some(Saved {
            name: entry.file_name().to_string_lossy().into_owned(),
            bytes,
        })
    }

    pub fn forget(&self, id: &str) {
        if id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            let _ = fs::remove_dir_all(self.dir.join(id));
        }
    }

    fn sweep(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > KEEP_UNFETCHED);
            if old {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// A plain file name: no folders, no leading dots, nothing a shell or a
/// Content-Disposition header would trip on.
fn file_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | '"'))
        .take(120)
        .collect();
    let clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() {
        "download".into()
    } else {
        clean
    }
}

pub fn content_type(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "csv" => "text/csv; charset=utf-8",
        "tex" | "bib" | "txt" | "md" | "typ" => "text/plain; charset=utf-8",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "html" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `attachment` with an ASCII name and the full UTF-8 one (RFC 6266).
pub fn disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gives_a_fresh_folder_and_hands_back_what_was_written_there() {
        let root = std::env::temp_dir().join(format!("lw-downloads-{}", std::process::id()));
        let downloads = Downloads::new(root.clone());
        let (id, path) = downloads.destination("../../etc/main.pdf").unwrap();
        assert_eq!(path.parent().unwrap(), root.join(&id));
        assert_eq!(path.file_name().unwrap(), "main.pdf");
        assert!(downloads.take(&id).is_none());
        fs::write(&path, b"%PDF-1.7").unwrap();
        let saved = downloads.take(&id).unwrap();
        assert_eq!(
            (saved.name.as_str(), saved.bytes.as_slice()),
            ("main.pdf", &b"%PDF-1.7"[..])
        );
        assert!(downloads.take("../x").is_none());
        downloads.forget(&id);
        assert!(!root.join(&id).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn names_and_headers() {
        assert_eq!(file_name(".hidden"), "hidden");
        assert_eq!(file_name(""), "download");
        assert_eq!(file_name("论文 v2.pdf"), "论文 v2.pdf");
        assert_eq!(content_type("a.PDF"), "application/pdf");
        assert_eq!(
            disposition("论文.pdf"),
            "attachment; filename=\"__.pdf\"; filename*=UTF-8''%E8%AE%BA%E6%96%87.pdf"
        );
    }
}
