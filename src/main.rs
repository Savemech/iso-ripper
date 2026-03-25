use anyhow::Context;
use clap::{Parser, Subcommand};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

mod extract;
mod index;
mod iso;
mod probe;
mod progress;
mod udf;

#[derive(Parser)]
#[command(name = "iso-ripper", version, about = "High-performance parallel ISO image processor")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Quick-scan ISOs to detect format (ISO9660, UDF, hybrid, unknown)
    Probe {
        /// Directory containing ISO files
        iso_dir: PathBuf,
    },
    /// Build text index of every file inside each ISO
    Index {
        /// Directory containing ISO files
        iso_dir: PathBuf,
        /// Output directory for index files
        output_dir: PathBuf,
    },
    /// Extract files matching an extension from each ISO
    Extract {
        /// Directory containing ISO files
        iso_dir: PathBuf,
        /// Output directory for extracted files
        output_dir: PathBuf,
        /// File extension to match (without dot)
        #[arg(long, default_value = "mp3")]
        ext: String,
    },
}

fn collect_iso_files(dir: &PathBuf) -> anyhow::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.is_file() {
                let ext = path.extension()?.to_str()?.to_lowercase();
                if ext == "iso" {
                    return Some(path);
                }
            }
            None
        })
        .collect();
    files.sort();
    Ok(files)
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    for unit in UNITS {
        if size < 1024.0 {
            return format!("{:.1} {}", size, unit);
        }
        size /= 1024.0;
    }
    format!("{:.1} PB", size)
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Probe { iso_dir } => cmd_probe(&iso_dir),
        Command::Index {
            iso_dir,
            output_dir,
        } => cmd_index(&iso_dir, &output_dir),
        Command::Extract {
            iso_dir,
            output_dir,
            ext,
        } => cmd_extract(&iso_dir, &output_dir, &ext),
    }
}

fn cmd_probe(iso_dir: &PathBuf) -> ExitCode {
    let files = match collect_iso_files(iso_dir) {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => {
            eprintln!("No .iso files found in {}", iso_dir.display());
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("Error: {:#}", e);
            return ExitCode::from(1);
        }
    };

    eprintln!("Probing {} ISOs...", files.len());

    let results: Vec<_> = files
        .par_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            match probe::probe_iso(path) {
                Ok(r) => (name, Ok(r)),
                Err(e) => (name, Err(e)),
            }
        })
        .collect();

    // Print table
    println!(
        "{:<60} {:>10} {:<30} {}",
        "FILENAME", "SIZE", "FORMAT", "NOTES"
    );
    println!("{}", "-".repeat(110));

    let mut counts: HashMap<String, u32> = HashMap::new();
    for (name, result) in &results {
        match result {
            Ok(r) => {
                let label = r.format_label();
                *counts.entry(label.clone()).or_default() += 1;
                let notes = if r.has_udf && !r.has_iso9660 {
                    "*** UDF-only, will be skipped ***"
                } else if !r.has_iso9660 {
                    "*** Unknown format ***"
                } else {
                    ""
                };
                println!(
                    "{:<60} {:>10} {:<30} {}",
                    name,
                    human_size(r.file_size),
                    label,
                    notes
                );
            }
            Err(e) => {
                println!("{:<60} {:>10} {:<30} {:#}", name, "?", "ERROR", e);
            }
        }
    }

    // Summary
    let total_size: u64 = results
        .iter()
        .filter_map(|(_, r)| r.as_ref().ok())
        .map(|r| r.file_size)
        .sum();

    println!("\n--- Summary ---");
    let mut sorted_counts: Vec<_> = counts.into_iter().collect();
    sorted_counts.sort_by(|a, b| b.1.cmp(&a.1));
    for (format, count) in &sorted_counts {
        println!("  {}: {}", format, count);
    }
    println!(
        "  Total: {} ISOs, {}",
        results.len(),
        human_size(total_size)
    );

    ExitCode::from(0)
}

fn cmd_index(iso_dir: &PathBuf, output_dir: &PathBuf) -> ExitCode {
    let files = match collect_iso_files(iso_dir) {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => {
            eprintln!("No .iso files found in {}", iso_dir.display());
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("Error: {:#}", e);
            return ExitCode::from(1);
        }
    };

    if let Err(e) = std::fs::create_dir_all(output_dir) {
        eprintln!("Error creating output dir: {:#}", e);
        return ExitCode::from(1);
    }

    eprintln!("Indexing {} ISOs -> {}", files.len(), output_dir.display());
    let progress = Arc::new(progress::Progress::new(files.len() as u64));

    let _results: Vec<_> = files
        .par_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                index::index_one_iso(path, output_dir)
            }));
            match result {
                Ok(Ok(file_count)) => {
                    progress.report_ok(&name, &format!("({} files)", file_count));
                }
                Ok(Err(e)) => {
                    progress.report_err(&name, &e);
                }
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<String>()
                        .map(|s| s.as_str())
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic");
                    let e = anyhow::anyhow!("PANIC: {}", msg);
                    progress.report_err(&name, &e);
                }
            }
        })
        .collect();

    let (ok, failed) = progress.summary();
    eprintln!("\nDone: {} succeeded, {} failed", ok, failed);
    if ok == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

fn cmd_extract(iso_dir: &PathBuf, output_dir: &PathBuf, ext: &str) -> ExitCode {
    let files = match collect_iso_files(iso_dir) {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => {
            eprintln!("No .iso files found in {}", iso_dir.display());
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("Error: {:#}", e);
            return ExitCode::from(1);
        }
    };

    if let Err(e) = std::fs::create_dir_all(output_dir) {
        eprintln!("Error creating output dir: {:#}", e);
        return ExitCode::from(1);
    }

    eprintln!(
        "Extracting .{} from {} ISOs -> {}",
        ext,
        files.len(),
        output_dir.display()
    );
    let progress = Arc::new(progress::Progress::new(files.len() as u64));
    let ext = ext.to_string();

    let _results: Vec<_> = files
        .par_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                extract::extract_one_iso(path, output_dir, &ext)
            }));
            match result {
                Ok(Ok(report)) => {
                    progress.report_ok(
                        &name,
                        &format!(
                            "({} files, {})",
                            report.files_extracted,
                            human_size(report.bytes_written)
                        ),
                    );
                }
                Ok(Err(e)) => {
                    progress.report_err(&name, &e);
                }
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<String>()
                        .map(|s| s.as_str())
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic");
                    let e = anyhow::anyhow!("PANIC: {}", msg);
                    progress.report_err(&name, &e);
                }
            }
        })
        .collect();

    let (ok, failed) = progress.summary();
    eprintln!("\nDone: {} succeeded, {} failed", ok, failed);
    if ok == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}
