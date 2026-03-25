use cdfs::{DirectoryEntry, ISO9660, ISO9660Reader, ISODirectory};

#[derive(Default)]
pub struct WalkStats {
    pub files: u64,
    pub dirs: u64,
    pub total_size: u64,
}

pub fn walk_iso<T, F>(iso: &ISO9660<T>, visitor: &mut F) -> anyhow::Result<WalkStats>
where
    T: ISO9660Reader,
    F: FnMut(&str, &DirectoryEntry<T>) -> anyhow::Result<()>,
{
    let mut stats = WalkStats::default();
    walk_dir(iso.root(), "", visitor, &mut stats)?;
    Ok(stats)
}

fn walk_dir<T, F>(
    dir: &ISODirectory<T>,
    parent_path: &str,
    visitor: &mut F,
    stats: &mut WalkStats,
) -> anyhow::Result<()>
where
    T: ISO9660Reader,
    F: FnMut(&str, &DirectoryEntry<T>) -> anyhow::Result<()>,
{
    for entry_result in dir.contents() {
        let entry = entry_result
            .map_err(|e| anyhow::anyhow!("reading directory '{}': {}", parent_path, e))?;

        let raw_name = entry.identifier().to_string();

        // Skip . and .. entries (encoded as \0 and \x01 in ISO9660)
        if raw_name == "\0" || raw_name == "\x01" || raw_name == "." || raw_name == ".." {
            continue;
        }

        // Strip ISO9660 version suffix (e.g., "FILE.TXT;1" -> "FILE.TXT")
        let name = raw_name.split(';').next().unwrap_or(&raw_name);
        // Also strip trailing dots left by some ISO creators
        let name = name.trim_end_matches('.');

        if name.is_empty() {
            continue;
        }

        let path = if parent_path.is_empty() {
            format!("/{}", name)
        } else {
            format!("{}/{}", parent_path, name)
        };

        match &entry {
            DirectoryEntry::Directory(sub_dir) => {
                stats.dirs += 1;
                walk_dir(sub_dir, &path, visitor, stats)?;
            }
            DirectoryEntry::File(f) => {
                stats.files += 1;
                stats.total_size += f.size() as u64;
                visitor(&path, &entry)?;
            }
            DirectoryEntry::Symlink(_) => {
                visitor(&path, &entry)?;
            }
        }
    }
    Ok(())
}
