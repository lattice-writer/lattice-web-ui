//! The compiled PDF of a project, for a tab that may already hold it.
//!
//! Every time a project is entered (opening it, a click on one of its tabs, a
//! page load), Lattice reads `read_compiled_pdf` before its build and again
//! after it (lattice-writer/lattice#1267). Over the browser host each read is
//! the whole PDF, base64 inside one JSON WebSocket message, even when the
//! bytes are the same as last time. bridge.js asks here first: this finds the
//! file `read_compiled_pdf` would read, and answers with its SHA-256 and size
//! as an ETag, a 304 when the tab names that tag, or the bytes themselves over
//! plain HTTP otherwise.
//!
//! The file is found the way Lattice 0.19.0 finds it (`compiled_pdf_path` in
//! `lattice-core/src/latex.rs`): the manifest's default root document, made
//! safe with `project::safe_path`, with the extension `pdf`. Lattice also
//! keeps one thing in memory the gateway cannot see: after a build of a
//! `standalone` figure or a `subfiles` chapter opened in the editor, the PDF
//! shown is that document's until the next build of anything else
//! (`compile_root::ONE_OFF_ROOTS`). So when a project has such a document,
//! the answer is `Unknown` and the tab asks Lattice as before.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// Lattice's manifest, relative to the project root.
const MANIFEST: &str = ".research/project.json";
/// Each root document a build here wrote a PDF of (`shown_pdf::HASHES`).
const HASHES: &str = ".research/cache/build-pdf-hashes.json";
/// Entries looked at for a document that compiles alone; past this the
/// project is too big to be sure of quickly, and Lattice is asked instead.
const MAX_WALK: usize = 20_000;
/// A PDF larger than this is left to Lattice.
pub const MAX_PDF: u64 = 256 * 1024 * 1024;

/// Why there is no fingerprint to give.
#[derive(Debug, PartialEq, Eq)]
pub enum Miss {
    /// Not a folder inside the home folder: refused outright.
    Refused(&'static str),
    /// Lattice would answer with an error: no project, no root document, no PDF.
    NoPdf(&'static str),
    /// Lattice might read another file than this one, or this one is out of
    /// reach: the tab asks Lattice.
    Unknown(&'static str),
}

/// The bytes `read_compiled_pdf` reads and their tag.
#[derive(Debug)]
pub struct Compiled {
    pub bytes: Vec<u8>,
    pub tag: String,
}

/// The fields `read_manifest` requires, with the types Lattice reads them
/// as: a manifest Lattice cannot read gives no PDF there either.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct Manifest {
    schema_version: u32,
    project_id: String,
    name: String,
    root_documents: Vec<RootDocument>,
    primary_bibliography: String,
    trusted: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct RootDocument {
    path: String,
    name: String,
    is_default: bool,
    #[serde(default)]
    chosen: bool,
}

/// The strong ETag of `bytes`: their SHA-256 and length.
pub fn tag_of(bytes: &[u8]) -> String {
    format!("\"sha256-{:x}-{}\"", Sha256::digest(bytes), bytes.len())
}

/// Whether an `If-None-Match` value names `tag` (weak comparison, as for a GET).
pub fn names_tag(if_none_match: &str, tag: &str) -> bool {
    let opaque = |t: &str| t.trim().trim_start_matches("W/").to_string();
    let tag = opaque(tag);
    if_none_match.split(',').any(|t| opaque(t) == tag)
}

/// The project folder `asked` names, inside `home`.
fn project_root(home: &Path, asked: &str) -> Result<PathBuf, Miss> {
    let asked = Path::new(asked);
    if !asked.is_absolute() {
        return Err(Miss::Refused("The project is named by its full path."));
    }
    let home = home
        .canonicalize()
        .map_err(|_| Miss::Refused("The home folder is missing."))?;
    let root = asked
        .canonicalize()
        .map_err(|_| Miss::NoPdf("That project folder does not exist."))?;
    if !root.starts_with(&home) {
        return Err(Miss::Refused(
            "Only projects inside the home folder are read here.",
        ));
    }
    if !root.is_dir() {
        return Err(Miss::NoPdf("That is not a folder."));
    }
    Ok(root)
}

/// The manifest default (`default_root`): the one marked, else the first.
fn default_document(root: &Path) -> Result<String, Miss> {
    let raw = fs::read_to_string(root.join(MANIFEST))
        .map_err(|_| Miss::NoPdf("That folder is not a Lattice project."))?;
    let manifest: Manifest = serde_json::from_str(&raw)
        .map_err(|_| Miss::NoPdf("The project's manifest could not be read."))?;
    let documents = &manifest.root_documents;
    documents
        .iter()
        .find(|document| document.is_default)
        .or_else(|| documents.first())
        .map(|document| document.path.clone())
        .ok_or(Miss::NoPdf(
            "The project has no LaTeX document to build yet.",
        ))
}

/// `project::stays_inside`: non-empty, relative, and only plain names (and `.`).
fn stays_inside(relative: &Path) -> bool {
    !relative.as_os_str().is_empty()
        && !relative.is_absolute()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}

/// `project::safe_path` (an existing path, symbolic links refused on every
/// component) from the already canonical `root`.
fn safe_path(root: &Path, relative: &str) -> Result<PathBuf, Miss> {
    let relative = Path::new(relative);
    if !stays_inside(relative) {
        return Err(Miss::NoPdf("The root document is outside the project."));
    }
    let mut cursor = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            continue;
        };
        cursor.push(name);
        let is_parent = index + 1 < components.len();
        match fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Miss::NoPdf("The root document's path has a symbolic link."))
            }
            Ok(meta) if is_parent && !meta.is_dir() => {
                return Err(Miss::NoPdf(
                    "A part of the root document's path is not a folder.",
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound && !is_parent => {}
            Err(_) => return Err(Miss::NoPdf("The root document's folder is missing.")),
        }
    }
    Ok(cursor)
}

/// `project::compiles_alone`: the document class is `standalone` or
/// `subfiles`, by the text outside comments.
fn compiles_alone(content: &str) -> bool {
    let code: String = content
        .lines()
        .map(|line| line.split('%').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let Some(at) = code.find("\\documentclass") else {
        return false;
    };
    let rest = code[at + "\\documentclass".len()..].trim_start();
    let rest = match rest.strip_prefix('[') {
        Some(options) => match options.find(']') {
            Some(end) => options[end + 1..].trim_start(),
            None => return false,
        },
        None => rest,
    };
    let Some(name) = rest.strip_prefix('{').and_then(|name| name.split_once('}')) else {
        return false;
    };
    matches!(name.0.trim(), "standalone" | "subfiles")
}

/// The plain components of a project-relative path, for comparing two.
fn normal(relative: &Path) -> Vec<std::ffi::OsString> {
    relative
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect()
}

/// Whether a build may have left another document's PDF on screen: some
/// `.tex` other than `default` compiles alone, so opening it in the editor
/// and building makes it the one `read_compiled_pdf` reads until the next
/// build of anything else (`resolve_compile_root`, `choose_compile_root`).
/// Symbolic links are not followed: Lattice refuses a document behind one.
/// Git's own folder holds no document the editor opens.
///
/// Such a document deleted since its build is still the one Lattice reads,
/// its PDF left beside where it was. Each build that writes a PDF records
/// its document in `HASHES` (`shown_pdf::record_hash`), and only the next
/// build forgets deleted ones, so one recorded there whose `.tex` is gone
/// counts too.
fn may_show_another(root: &Path, default: &str) -> Result<bool, Miss> {
    let default = normal(Path::new(default));
    let recorded: BTreeMap<String, serde_json::Value> = fs::read(root.join(HASHES))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default();
    if recorded
        .keys()
        .any(|document| normal(Path::new(document)) != default && !root.join(document).is_file())
    {
        return Ok(true);
    }
    let mut folders = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(folder) = folders.pop() {
        let Ok(entries) = fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_WALK {
                return Err(Miss::Unknown("The project is too large to check quickly."));
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    folders.push(path);
                }
                continue;
            }
            let tex = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.to_ascii_lowercase().ends_with(".tex"));
            if !kind.is_file() || !tex {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            if normal(relative) == default {
                continue;
            }
            // Lattice reads it as text too; one it cannot read casts no vote.
            if fs::read_to_string(&path).is_ok_and(|content| compiles_alone(&content)) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Where `read_compiled_pdf` reads the PDF of the project at `asked`, when
/// the gateway can be sure, inside `home`.
pub fn compiled_pdf_path(home: &Path, asked: &str) -> Result<PathBuf, Miss> {
    let root = project_root(home, asked)?;
    let document = default_document(&root)?;
    if may_show_another(&root, &document)? {
        return Err(Miss::Unknown(
            "A standalone or subfiles document may be the one on screen.",
        ));
    }
    Ok(safe_path(&root, &document)?.with_extension("pdf"))
}

/// The compiled PDF of the project at `asked` and its tag: the bytes
/// `read_compiled_pdf` would send, read the way it reads them.
pub fn read(home: &Path, asked: &str) -> Result<Compiled, Miss> {
    let path = compiled_pdf_path(home, asked)?;
    // Lattice follows a link at the PDF itself; the gateway only inside home.
    let real = match path.canonicalize() {
        Ok(real) => real,
        Err(_) => return Err(Miss::NoPdf("The project has no compiled PDF yet.")),
    };
    let home = home
        .canonicalize()
        .map_err(|_| Miss::Refused("The home folder is missing."))?;
    if !real.starts_with(&home) {
        return Err(Miss::Unknown(
            "The compiled PDF is outside the home folder.",
        ));
    }
    let meta =
        fs::metadata(&real).map_err(|_| Miss::NoPdf("The compiled PDF could not be read."))?;
    if meta.len() > MAX_PDF {
        return Err(Miss::Unknown(
            "The compiled PDF is too large to serve here.",
        ));
    }
    let bytes = fs::read(&real).map_err(|_| Miss::NoPdf("The compiled PDF could not be read."))?;
    if !bytes.starts_with(b"%PDF-") {
        return Err(Miss::NoPdf("The compiled output is not a valid PDF."));
    }
    let tag = tag_of(&bytes);
    Ok(Compiled { bytes, tag })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST_JSON: &str = r#"{"schemaVersion":1,"projectId":"p","name":"Paper","rootDocuments":[{"path":"other.tex","name":"other","isDefault":false},{"path":"paper/main.tex","name":"main","isDefault":true}],"primaryBibliography":"refs.bib","trusted":true,"engine":"pdf"}"#;

    fn home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lw-pdf-{}", crate::store::random_hex(6)));
        fs::create_dir_all(dir.join("projects/thesis/.research")).unwrap();
        fs::create_dir_all(dir.join("projects/thesis/paper")).unwrap();
        fs::write(
            dir.join("projects/thesis/.research/project.json"),
            MANIFEST_JSON,
        )
        .unwrap();
        fs::write(
            dir.join("projects/thesis/paper/main.tex"),
            "\\documentclass{article}",
        )
        .unwrap();
        fs::write(dir.join("projects/thesis/paper/main.pdf"), "%PDF-1.5 first").unwrap();
        dir
    }

    fn project(home: &Path) -> String {
        home.join("projects/thesis").to_string_lossy().into_owned()
    }

    #[test]
    fn finds_the_default_root_documents_pdf_beside_it() {
        let home = home();
        let path = compiled_pdf_path(&home, &project(&home)).unwrap();
        let canon = home.canonicalize().unwrap();
        assert_eq!(path, canon.join("projects/thesis/paper/main.pdf"));
        let read = read(&home, &project(&home)).unwrap();
        assert_eq!(read.bytes, b"%PDF-1.5 first");
        assert_eq!(read.tag, tag_of(b"%PDF-1.5 first"));
        assert!(read.tag.ends_with("-14\""), "{}", read.tag);
        // Without a marked default, the first listed.
        let first = MANIFEST_JSON.replace("\"isDefault\":true", "\"isDefault\":false");
        fs::write(home.join("projects/thesis/.research/project.json"), first).unwrap();
        assert_eq!(
            compiled_pdf_path(&home, &project(&home)).unwrap(),
            canon.join("projects/thesis/other.pdf")
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn a_new_pdf_gets_a_new_tag() {
        let home = home();
        let first = read(&home, &project(&home)).unwrap().tag;
        fs::write(
            home.join("projects/thesis/paper/main.pdf"),
            "%PDF-1.5 again",
        )
        .unwrap();
        let second = read(&home, &project(&home)).unwrap().tag;
        assert_ne!(first, second, "same size, other bytes");
        assert!(names_tag(&format!("\"x\", W/{second}"), &second));
        assert!(!names_tag(&first, &second));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn follows_the_rules_of_lattices_read() {
        let home = home();
        let thesis = home.join("projects/thesis");
        let manifest = |path: &str| {
            let json = MANIFEST_JSON.replace("paper/main.tex", path);
            fs::write(thesis.join(".research/project.json"), json).unwrap();
        };
        // Not a PDF, as Lattice refuses it.
        fs::write(thesis.join("paper/main.pdf"), "<html>").unwrap();
        assert!(matches!(read(&home, &project(&home)), Err(Miss::NoPdf(_))));
        // No PDF yet.
        fs::remove_file(thesis.join("paper/main.pdf")).unwrap();
        assert!(matches!(read(&home, &project(&home)), Err(Miss::NoPdf(_))));
        // A root document out of the project, or behind a symbolic link.
        manifest("../escape.tex");
        assert!(matches!(
            compiled_pdf_path(&home, &project(&home)),
            Err(Miss::NoPdf(_))
        ));
        manifest("/etc/passwd.tex");
        assert!(matches!(
            compiled_pdf_path(&home, &project(&home)),
            Err(Miss::NoPdf(_))
        ));
        std::os::unix::fs::symlink(thesis.join("paper"), thesis.join("linked")).unwrap();
        manifest("linked/main.tex");
        assert!(matches!(
            compiled_pdf_path(&home, &project(&home)),
            Err(Miss::NoPdf(_))
        ));
        // The .tex may be gone while its PDF stays: Lattice still reads it.
        manifest("gone/../x.tex");
        assert!(compiled_pdf_path(&home, &project(&home)).is_err());
        manifest("./paper/old.tex");
        fs::write(thesis.join("paper/old.pdf"), "%PDF-1.4 old").unwrap();
        assert_eq!(read(&home, &project(&home)).unwrap().bytes, b"%PDF-1.4 old");
        // A manifest Lattice cannot read.
        fs::write(
            thesis.join(".research/project.json"),
            r#"{"rootDocuments":[]}"#,
        )
        .unwrap();
        assert!(matches!(
            compiled_pdf_path(&home, &project(&home)),
            Err(Miss::NoPdf(_))
        ));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn stays_inside_the_home_folder() {
        let home = home();
        let outside =
            std::env::temp_dir().join(format!("lw-pdf-out-{}", crate::store::random_hex(6)));
        fs::create_dir_all(outside.join(".research")).unwrap();
        fs::write(outside.join(".research/project.json"), MANIFEST_JSON).unwrap();
        fs::create_dir_all(outside.join("paper")).unwrap();
        fs::write(outside.join("paper/main.pdf"), "%PDF-1.5 secret").unwrap();
        assert!(matches!(
            read(&home, outside.to_str().unwrap()),
            Err(Miss::Refused(_))
        ));
        assert!(matches!(
            read(&home, "projects/thesis"),
            Err(Miss::Refused(_))
        ));
        let dotted = format!(
            "{}/projects/../../{}",
            home.display(),
            outside.file_name().unwrap().to_string_lossy()
        );
        assert!(matches!(read(&home, &dotted), Err(Miss::Refused(_))));
        // A link in home to a project outside it is outside it.
        std::os::unix::fs::symlink(&outside, home.join("projects/linked")).unwrap();
        let linked = home.join("projects/linked").to_string_lossy().into_owned();
        assert!(matches!(read(&home, &linked), Err(Miss::Refused(_))));
        // A PDF linked out of home is left to Lattice.
        let thesis = home.join("projects/thesis");
        fs::remove_file(thesis.join("paper/main.pdf")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("paper/main.pdf"),
            thesis.join("paper/main.pdf"),
        )
        .unwrap();
        assert!(matches!(
            read(&home, &project(&home)),
            Err(Miss::Unknown(_))
        ));
        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn a_document_that_compiles_alone_makes_the_answer_unknown() {
        let home = home();
        let thesis = home.join("projects/thesis");
        // Chapters and other documents are fine.
        fs::write(thesis.join("paper/intro.tex"), "\\section{Intro}").unwrap();
        fs::write(
            thesis.join("rebuttal.tex"),
            "\\documentclass[11pt]{article}",
        )
        .unwrap();
        fs::write(
            thesis.join("commented.tex"),
            "% \\documentclass{standalone}\n\\section{x}",
        )
        .unwrap();
        fs::create_dir_all(thesis.join(".git/objects")).unwrap();
        fs::write(
            thesis.join(".git/objects/fig.tex"),
            "\\documentclass{standalone}",
        )
        .unwrap();
        assert!(compiled_pdf_path(&home, &project(&home)).is_ok());
        // A standalone figure, or a subfiles chapter, anywhere else.
        fs::create_dir_all(thesis.join("figures/tikz")).unwrap();
        fs::write(
            thesis.join("figures/tikz/plot.TEX"),
            "\\documentclass[\n  border=2pt\n]{standalone}",
        )
        .unwrap();
        assert!(matches!(
            compiled_pdf_path(&home, &project(&home)),
            Err(Miss::Unknown(_))
        ));
        fs::remove_file(thesis.join("figures/tikz/plot.TEX")).unwrap();
        fs::write(
            thesis.join("paper/ch1.tex"),
            "\\documentclass[main.tex]{subfiles}",
        )
        .unwrap();
        assert!(matches!(
            compiled_pdf_path(&home, &project(&home)),
            Err(Miss::Unknown(_))
        ));
        // The default itself compiling alone is the default's own PDF.
        fs::remove_file(thesis.join("paper/ch1.tex")).unwrap();
        fs::write(thesis.join("paper/main.tex"), "\\documentclass{standalone}").unwrap();
        assert!(compiled_pdf_path(&home, &project(&home)).is_ok());
        // One built and deleted since: Lattice still reads its PDF until the next build.
        fs::create_dir_all(thesis.join(".research/cache")).unwrap();
        let hashes = thesis.join(".research/cache/build-pdf-hashes.json");
        fs::write(&hashes, r#"{"paper/main.tex":"aa","rebuttal.tex":"bb"}"#).unwrap();
        assert!(compiled_pdf_path(&home, &project(&home)).is_ok());
        fs::write(
            &hashes,
            r#"{"paper/main.tex":"aa","figures/gone.tex":"cc"}"#,
        )
        .unwrap();
        assert!(matches!(
            compiled_pdf_path(&home, &project(&home)),
            Err(Miss::Unknown(_))
        ));
        fs::remove_file(&hashes).unwrap();
        // One behind a symbolic link cannot be opened as a root.
        let figures =
            std::env::temp_dir().join(format!("lw-pdf-fig-{}", crate::store::random_hex(6)));
        fs::create_dir_all(&figures).unwrap();
        fs::write(figures.join("fig.tex"), "\\documentclass{standalone}").unwrap();
        std::os::unix::fs::symlink(&figures, thesis.join("shared")).unwrap();
        assert!(compiled_pdf_path(&home, &project(&home)).is_ok());
        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(figures);
    }

    #[test]
    fn compiles_alone_reads_the_class_as_lattice_does() {
        assert!(compiles_alone("\\documentclass{standalone}"));
        assert!(compiles_alone("%x\n\\documentclass [tikz] { subfiles }"));
        assert!(!compiles_alone("\\documentclass{article}"));
        assert!(!compiles_alone("\\documentclass[unclosed{standalone}"));
        assert!(!compiles_alone("% \\documentclass{standalone}"));
    }
}
