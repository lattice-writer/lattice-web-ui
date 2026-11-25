//! The host's folders and files, for a tab's Open panel.
//!
//! Lattice's "Open…" asks for a folder or a file on the host in a panel on the
//! host's display, which nobody can see. bridge.js shows its own panel in the
//! tab instead and browses the host through these: a folder's subfolders (and,
//! when Lattice wants a file, the files Lattice's filters allow), a new
//! folder, and a file uploaded from the device into the folder shown. Every
//! path stays inside the home folder the gateway runs in; hidden entries are
//! not listed.

use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The largest file a tab uploads.
pub const MAX_UPLOAD: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 2000;

/// `path`, made absolute and checked to be a folder inside `home`.
pub fn folder_inside(home: &Path, path: &Path) -> Result<PathBuf, String> {
    let home = home
        .canonicalize()
        .map_err(|e| format!("The home folder is missing: {e}"))?;
    let path = path
        .canonicalize()
        .map_err(|_| "That folder does not exist.".to_string())?;
    if !path.starts_with(&home) {
        return Err("Only folders inside the home folder can be opened here.".into());
    }
    if !path.is_dir() {
        return Err("That is not a folder.".into());
    }
    Ok(path)
}

/// Where the panel starts: `asked` (or the folder holding it) when it is
/// inside `home`, else `fallback` (the projects folder), else `home`.
pub fn start(home: &Path, asked: Option<&str>, fallback: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(asked) = asked.filter(|a| a.starts_with('/')) {
        let asked = Path::new(asked);
        for candidate in [Some(asked), asked.parent()].into_iter().flatten() {
            if let Ok(found) = folder_inside(home, candidate) {
                return Ok(found);
            }
        }
    }
    if let Some(found) = fallback.and_then(|f| folder_inside(home, f).ok()) {
        return Ok(found);
    }
    folder_inside(home, home)
}

/// A folder's subfolders and, when `files`, its files whose extension is one
/// of `extensions` (any, when empty), folders first, by name.
pub fn list(
    home: &Path,
    folder: &Path,
    files: bool,
    extensions: &[String],
) -> Result<Value, String> {
    let folder = folder_inside(home, folder)?;
    let home = home.canonicalize().map_err(|e| e.to_string())?;
    let wanted: Vec<String> = extensions
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e != "*")
        .collect();
    let mut entries: Vec<(bool, String, Value)> = Vec::new();
    let read = fs::read_dir(&folder).map_err(|e| format!("Could not read that folder: {e}"))?;
    for entry in read.filter_map(Result::ok).take(MAX_ENTRIES * 4) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // Follows symlinks, as a file panel does; a link out of home is still
        // refused when it is opened.
        let Ok(meta) = fs::metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            let project = fs::read_dir(entry.path())
                .map(|d| {
                    d.filter_map(Result::ok)
                        .take(500)
                        .any(|f| f.file_name().to_string_lossy().ends_with(".tex"))
                })
                .unwrap_or(false);
            entries.push((
                true,
                name.to_lowercase(),
                json!({ "name": name, "kind": "folder", "project": project }),
            ));
        } else if files && meta.is_file() {
            let ext = Path::new(&name)
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if wanted.is_empty() || wanted.contains(&ext) {
                entries.push((
                    false,
                    name.to_lowercase(),
                    json!({ "name": name, "kind": "file", "size": meta.len() }),
                ));
            }
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let truncated = entries.len() > MAX_ENTRIES;
    let parent = (folder != home)
        .then(|| folder.parent().map(|p| p.to_string_lossy().into_owned()))
        .flatten();
    Ok(json!({
        "home": home.to_string_lossy(),
        "path": folder.to_string_lossy(),
        "parent": parent,
        "entries": entries.into_iter().take(MAX_ENTRIES).map(|e| e.2).collect::<Vec<_>>(),
        "truncated": truncated,
    }))
}

/// A plain name for a new folder or an uploaded file: one path segment, not
/// hidden, no control characters.
pub fn plain_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    let mut parts = Path::new(name).components();
    let ok = matches!(parts.next(), Some(Component::Normal(_)))
        && parts.next().is_none()
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
        && name.len() <= 200;
    if ok {
        Ok(name.to_string())
    } else {
        Err("Use a plain name: no slashes, not starting with a dot.".into())
    }
}

/// Make `name` in `parent`, returning its path.
pub fn make_folder(home: &Path, parent: &Path, name: &str) -> Result<PathBuf, String> {
    let parent = folder_inside(home, parent)?;
    let path = parent.join(plain_name(name)?);
    fs::create_dir(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
            "A file or folder with that name is already there.".to_string()
        }
        _ => format!("Could not make the folder: {e}"),
    })?;
    Ok(path)
}

/// Where an upload named `name` goes in `folder`: that name, or "name (2).ext"
/// and so on when it is taken.
pub fn upload_path(home: &Path, folder: &Path, name: &str) -> Result<PathBuf, String> {
    let folder = folder_inside(home, folder)?;
    let name = plain_name(name)?;
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name.as_str(), ""),
    };
    for n in 1..1000 {
        let candidate = if n == 1 {
            folder.join(&name)
        } else {
            folder.join(format!("{stem} ({n}){ext}"))
        };
        if fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err("Too many files with that name are already there.".into())
}

/// `%XX`-decoded text, for names sent in headers.
pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lw-files-{}", crate::store::random_hex(6)));
        fs::create_dir_all(dir.join("projects/paper")).unwrap();
        fs::write(dir.join("projects/paper/main.tex"), "x").unwrap();
        fs::write(dir.join("projects/refs.BIB"), "x").unwrap();
        fs::write(dir.join("projects/notes.txt"), "x").unwrap();
        fs::create_dir_all(dir.join("projects/.hidden")).unwrap();
        dir
    }

    #[test]
    fn lists_folders_then_wanted_files_inside_home_only() {
        let home = home();
        let listing = list(&home, &home.join("projects"), true, &["bib".into()]).unwrap();
        let names: Vec<_> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                format!(
                    "{}:{}",
                    e["kind"].as_str().unwrap(),
                    e["name"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(names, ["folder:paper", "file:refs.BIB"]);
        assert_eq!(listing["entries"][0]["project"], true);
        assert_eq!(
            listing["parent"],
            home.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        let folders = list(&home, &home.join("projects"), false, &[]).unwrap();
        assert_eq!(folders["entries"].as_array().unwrap().len(), 1);
        // Home has no parent here, and nothing outside it opens.
        assert!(list(&home, &home, false, &[]).unwrap()["parent"].is_null());
        assert!(list(&home, &home.join(".."), false, &[]).is_err());
        assert!(list(&home, Path::new("/etc"), false, &[]).is_err());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn starts_where_asked_else_the_projects_folder() {
        let home = home();
        let canon = |p: PathBuf| p.canonicalize().unwrap();
        let asked = home.join("projects/paper/main.tex");
        assert_eq!(
            start(&home, asked.to_str(), None).unwrap(),
            canon(home.join("projects/paper"))
        );
        assert_eq!(
            start(&home, Some("/etc"), Some(&home.join("projects"))).unwrap(),
            canon(home.join("projects"))
        );
        assert_eq!(start(&home, None, None).unwrap(), canon(home.clone()));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn new_folders_and_uploads_get_plain_free_names() {
        let home = home();
        let projects = home.join("projects");
        assert!(make_folder(&home, &projects, "../x").is_err());
        assert!(make_folder(&home, &projects, ".x").is_err());
        assert!(make_folder(&home, &projects, "paper").is_err());
        assert!(make_folder(&home, &projects, "thesis").unwrap().is_dir());
        let first = upload_path(&home, &projects, "refs.BIB").unwrap();
        assert_eq!(first.file_name().unwrap(), "refs (2).BIB");
        assert_eq!(
            upload_path(&home, &projects, "new.zip")
                .unwrap()
                .file_name()
                .unwrap(),
            "new.zip"
        );
        assert!(
            upload_path(&home, &std::env::temp_dir(), "x.zip").is_err(),
            "outside home"
        );
        assert_eq!(
            percent_decode("%E8%AE%BA%E6%96%87.pdf").as_deref(),
            Some("论文.pdf")
        );
        assert_eq!(percent_decode("bad%zz"), None);
        let _ = fs::remove_dir_all(home);
    }
}
