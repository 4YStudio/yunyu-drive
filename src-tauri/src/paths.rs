use std::path::{Path, PathBuf};

/// Returns `path` when free, otherwise `name (2).ext`, `name (3).ext`, …
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("untitled")
        .to_string();
    let ext = path.extension().and_then(|v| v.to_str()).map(|v| v.to_string());
    for index in 2..10_000 {
        let name = match &ext {
            Some(ext) => format!("{stem} ({index}).{ext}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

/// Makes one remote file name safe to use as a single local path component.
///
/// Server-provided names are untrusted: a drive (or a share link) can return
/// `../../.config/autostart/x.desktop` and have it written outside the download
/// folder. Separators and control characters are replaced, and names that carry
/// no real characters fall back to `untitled`.
pub fn safe_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' | '\0' => out.push('_'),
            c if (c as u32) < 0x20 => out.push('_'),
            c => out.push(c),
        }
    }
    let trimmed = out.trim();
    // `.` and `..` are directory references, not names.
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
        return "untitled".to_string();
    }
    trimmed.to_string()
}

/// Joins `relative` under `root`, refusing anything that escapes it.
pub fn safe_join(root: &Path, relative: &Path) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in relative.components() {
        use std::path::Component;
        match part {
            Component::Normal(name) => out.push(name),
            // `..` would climb out of the download folder; drop it instead of
            // trusting the remote listing.
            Component::ParentDir | Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// Builds names used for conflict handling on uploads.
pub fn renamed_for_conflict(name: &str, index: u32) -> String {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or(name)
        .to_string();
    match path.extension().and_then(|v| v.to_str()) {
        Some(ext) if !ext.is_empty() => format!("{stem} ({index}).{ext}"),
        _ => format!("{stem} ({index})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_cannot_escape_the_download_folder() {
        assert_eq!(safe_component("../../.config/autostart/x.desktop"), ".._.._.config_autostart_x.desktop");
        assert_eq!(safe_component("a/b"), "a_b");
        assert_eq!(safe_component("a\\b"), "a_b");
        assert_eq!(safe_component(".."), "untitled");
        assert_eq!(safe_component("   "), "untitled");
        assert_eq!(safe_component("正常名称.txt"), "正常名称.txt");
    }

    #[test]
    fn safe_join_drops_parent_components() {
        let root = Path::new("/home/u/Downloads/movie");
        let joined = safe_join(root, Path::new("../../etc/passwd"));
        assert_eq!(joined, PathBuf::from("/home/u/Downloads/movie/etc/passwd"));
        let plain = safe_join(root, Path::new("sub/dir"));
        assert_eq!(plain, PathBuf::from("/home/u/Downloads/movie/sub/dir"));
    }
}
