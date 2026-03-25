use crate::iso;
use crate::udf::UdfReader;
use anyhow::Context;
use cdfs::{DirectoryEntry, ISO9660};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

pub struct ExtractReport {
    pub files_extracted: u64,
    pub bytes_written: u64,
}

fn sanitize_path(internal_path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(internal_path.trim_start_matches('/'));
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return None;
        }
    }
    Some(path)
}

pub fn extract_one_iso(
    iso_path: &Path,
    output_dir: &Path,
    extension: &str,
) -> anyhow::Result<ExtractReport> {
    let file = File::open(iso_path)
        .with_context(|| format!("opening {}", iso_path.display()))?;

    match ISO9660::new(file) {
        Ok(iso) => extract_iso9660(iso_path, output_dir, extension, &iso),
        Err(_) => extract_udf(iso_path, output_dir, extension),
    }
}

fn extract_iso9660(
    iso_path: &Path,
    output_dir: &Path,
    extension: &str,
    iso: &ISO9660<File>,
) -> anyhow::Result<ExtractReport> {
    let stem = iso_path.file_stem().unwrap().to_string_lossy();
    let iso_output_dir = output_dir.join(stem.as_ref());
    let ext_lower = format!(".{}", extension.to_lowercase());

    let mut extracted = 0u64;
    let mut bytes_written = 0u64;
    let mut dir_created = false;

    iso::walk_iso(iso, &mut |path: &str, entry: &DirectoryEntry<File>| {
        let DirectoryEntry::File(iso_file) = entry else {
            return Ok(());
        };

        if !path.to_lowercase().ends_with(&ext_lower) {
            return Ok(());
        }

        let relative = match sanitize_path(path) {
            Some(p) => p,
            None => {
                eprintln!("  WARN: skipping unsafe path: {}", path);
                return Ok(());
            }
        };

        if !dir_created {
            fs::create_dir_all(&iso_output_dir)
                .with_context(|| format!("creating {}", iso_output_dir.display()))?;
            dir_created = true;
        }

        let out_path = iso_output_dir.join(&relative);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut reader = iso_file.read();
        let out_file = File::create(&out_path)
            .with_context(|| format!("creating {}", out_path.display()))?;
        let mut writer = BufWriter::with_capacity(64 * 1024, out_file);

        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
            bytes_written += n as u64;
        }
        writer.flush()?;
        extracted += 1;

        Ok(())
    })?;

    Ok(ExtractReport {
        files_extracted: extracted,
        bytes_written,
    })
}

fn extract_udf(
    iso_path: &Path,
    output_dir: &Path,
    extension: &str,
) -> anyhow::Result<ExtractReport> {
    let mut file = File::open(iso_path)
        .with_context(|| format!("opening {}", iso_path.display()))?;

    if !crate::udf::is_udf(&mut file) {
        anyhow::bail!("Not a valid ISO9660 or UDF image");
    }
    file.seek(SeekFrom::Start(0))?;

    let mut udf = UdfReader::new(file)
        .with_context(|| format!("parsing UDF {}", iso_path.display()))?;

    let (entries, _stats) = udf.walk()?;

    let stem = iso_path.file_stem().unwrap().to_string_lossy();
    let iso_output_dir = output_dir.join(stem.as_ref());
    let ext_lower = format!(".{}", extension.to_lowercase());

    let mut extracted = 0u64;
    let mut bytes_written = 0u64;
    let mut dir_created = false;

    // Filter matching entries, then extract each
    let matching: Vec<_> = entries
        .iter()
        .filter(|e| !e.is_dir && e.path.to_lowercase().ends_with(&ext_lower))
        .collect();

    for entry in matching {
        let relative = match sanitize_path(&entry.path) {
            Some(p) => p,
            None => {
                eprintln!("  WARN: skipping unsafe path: {}", entry.path);
                continue;
            }
        };

        if !dir_created {
            fs::create_dir_all(&iso_output_dir)
                .with_context(|| format!("creating {}", iso_output_dir.display()))?;
            dir_created = true;
        }

        let out_path = iso_output_dir.join(&relative);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let out_file = File::create(&out_path)
            .with_context(|| format!("creating {}", out_path.display()))?;
        let mut writer = BufWriter::with_capacity(64 * 1024, out_file);

        let written = udf.read_file_to(entry, &mut writer)?;
        writer.flush()?;
        bytes_written += written;
        extracted += 1;
    }

    Ok(ExtractReport {
        files_extracted: extracted,
        bytes_written,
    })
}
