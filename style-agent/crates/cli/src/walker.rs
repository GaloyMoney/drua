use std::fs;
use std::path::Path;

/// Supported file extensions and their language identifiers.
const SUPPORTED_EXTENSIONS: &[(&str, &str)] =
    &[(".rs", "rust"), (".bats", "bats"), (".bash", "bash")];

/// A source file discovered in a repository.
pub struct RepoFile {
    /// Path relative to repo root
    pub path: String,
    /// File contents
    pub content: String,
    /// Derived Rust module path (e.g. `src/foo/bar.rs` → `foo::bar`).
    /// Empty for non-Rust files.
    pub module_path: String,
    /// Language identifier derived from the file extension (`rust`, `bats`, `bash`).
    pub language: String,
}

/// Walk a repo directory, find all supported source files, and return their contents.
///
/// Skips: `target/`, `.git/`, hidden dirs/files, unsupported extensions, empty files.
/// If `subpaths` is provided, only walks those subdirectories.
/// If `exclude_dirs` is non-empty, skips directories whose name matches any entry.
pub fn walk_repo(
    repo_path: &Path,
    subpaths: Option<&[String]>,
    exclude_dirs: &[String],
) -> anyhow::Result<Vec<RepoFile>> {
    let mut files = Vec::new();

    match subpaths {
        Some(paths) => {
            for sub in paths {
                let dir = repo_path.join(sub);
                if dir.is_dir() {
                    walk_dir(repo_path, &dir, exclude_dirs, &mut files)?;
                }
            }
        }
        None => {
            walk_dir(repo_path, repo_path, exclude_dirs, &mut files)?;
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Return the language for a file name, if the extension is supported.
fn detect_language(file_name: &str) -> Option<&'static str> {
    SUPPORTED_EXTENSIONS
        .iter()
        .find(|(ext, _)| file_name.ends_with(ext))
        .map(|(_, lang)| *lang)
}

fn walk_dir(
    base: &Path,
    dir: &Path,
    exclude_dirs: &[String],
    files: &mut Vec<RepoFile>,
) -> anyhow::Result<()> {
    let entries = fs::read_dir(dir)?;

    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Skip hidden files/dirs
        if name.starts_with('.') {
            continue;
        }

        let path = entry.path();

        if file_type.is_dir() {
            // Skip target directory
            if name == "target" {
                continue;
            }
            // Skip user-configured exclude dirs
            if exclude_dirs.iter().any(|d| d == name.as_ref()) {
                continue;
            }
            walk_dir(base, &path, exclude_dirs, files)?;
        } else if file_type.is_file() {
            let language = match detect_language(&name) {
                Some(lang) => lang,
                None => continue,
            };

            let content = fs::read_to_string(&path)?;
            // Skip empty files
            if content.trim().is_empty() {
                continue;
            }

            let rel_path = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();

            let module_path = if language == "rust" {
                style_agent_core::chunker::module_path_from_file(&rel_path)
            } else {
                String::new()
            };

            files.push(RepoFile {
                path: rel_path,
                content,
                module_path,
                language: language.to_string(),
            });
        }
    }

    Ok(())
}
