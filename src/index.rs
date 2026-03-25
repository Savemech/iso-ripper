use crate::iso;
use crate::udf::UdfReader;
use anyhow::Context;
use cdfs::{DirectoryEntry, ExtraAttributes, ISO9660};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use time::format_description::well_known::Rfc3339;

fn format_time(t: time::OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_else(|_| "-".into())
}

pub fn index_one_iso(iso_path: &Path, output_dir: &Path) -> anyhow::Result<u64> {
    let file = File::open(iso_path)
        .with_context(|| format!("opening {}", iso_path.display()))?;

    // Try ISO9660 first, fall back to UDF
    match ISO9660::new(file) {
        Ok(iso) => index_iso9660(iso_path, output_dir, &iso),
        Err(_) => index_udf(iso_path, output_dir),
    }
}

fn index_iso9660(
    iso_path: &Path,
    output_dir: &Path,
    iso: &ISO9660<File>,
) -> anyhow::Result<u64> {
    let stem = iso_path.file_stem().unwrap().to_string_lossy();
    let out_path = output_dir.join(format!("{}.txt", stem));

    let has_joliet = iso.root_at(1).is_some();
    let has_rr = iso.is_rr();
    let vol_id = iso.volume_set_identifier().trim().to_string();
    let iso_size = iso_path.metadata()?.len();

    let mut lines: Vec<String> = Vec::new();

    let stats = iso::walk_iso(iso, &mut |path: &str, entry: &DirectoryEntry<File>| {
        let ts = format_time(entry.modify_time());
        match entry {
            DirectoryEntry::File(f) => {
                lines.push(format!("{}\t{}\t{}\tfile", path, f.size(), ts));
            }
            DirectoryEntry::Symlink(link) => {
                let target = link.target().map(|s| s.as_str()).unwrap_or("?");
                lines.push(format!("{}\t0\t{}\tsymlink->{}", path, ts, target));
            }
            _ => {}
        }
        Ok(())
    })?;

    write_index(
        &out_path,
        iso_path,
        &vol_id,
        &format!(
            "ISO9660{}{}",
            if has_joliet { "+Joliet" } else { "" },
            if has_rr { "+RockRidge" } else { "" }
        ),
        iso_size,
        &stats,
        &lines,
    )
}

fn index_udf(iso_path: &Path, output_dir: &Path) -> anyhow::Result<u64> {
    let mut file = File::open(iso_path)
        .with_context(|| format!("opening {}", iso_path.display()))?;

    if !crate::udf::is_udf(&mut file) {
        anyhow::bail!("Not a valid ISO9660 or UDF image");
    }
    file.seek(SeekFrom::Start(0))?;

    let mut udf = UdfReader::new(file)
        .with_context(|| format!("parsing UDF {}", iso_path.display()))?;

    let vol_id = udf.volume_id.clone();
    let iso_size = iso_path.metadata()?.len();

    let (entries, stats) = udf.walk()?;

    let lines: Vec<String> = entries
        .iter()
        .map(|e| {
            let ts = e
                .modified
                .map(|t| format_time(t))
                .unwrap_or_else(|| "-".into());
            format!("{}\t{}\t{}\tfile", e.path, e.size, ts)
        })
        .collect();

    let stem = iso_path.file_stem().unwrap().to_string_lossy();
    let out_path = output_dir.join(format!("{}.txt", stem));

    write_index(&out_path, iso_path, &vol_id, "UDF", iso_size, &stats, &lines)
}

fn write_index(
    out_path: &Path,
    iso_path: &Path,
    vol_id: &str,
    format_str: &str,
    iso_size: u64,
    stats: &iso::WalkStats,
    lines: &[String],
) -> anyhow::Result<u64> {
    let out_file = File::create(out_path)
        .with_context(|| format!("creating {}", out_path.display()))?;
    let mut w = BufWriter::with_capacity(128 * 1024, out_file);

    writeln!(w, "# iso-ripper index")?;
    writeln!(w, "# source: {}", iso_path.display())?;
    writeln!(w, "# volume: {}", vol_id)?;
    writeln!(w, "# format: {}", format_str)?;
    writeln!(w, "# iso_size: {}", iso_size)?;
    writeln!(w, "# files: {}", stats.files)?;
    writeln!(w, "# dirs: {}", stats.dirs)?;
    writeln!(w, "# total_content_size: {}", stats.total_size)?;
    writeln!(w, "#")?;
    writeln!(w, "# PATH\tSIZE\tMODIFIED\tTYPE")?;

    for line in lines {
        writeln!(w, "{}", line)?;
    }

    w.flush()?;
    Ok(stats.files)
}
