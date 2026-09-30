use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveFile {
    name: String,
    relative_path: String,
    size: u64,
    modified_ms: u64,
    year: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct YearSummary {
    year: i32,
    file_count: usize,
    total_size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanResult {
    files: Vec<ArchiveFile>,
    years: Vec<YearSummary>,
    total_files: usize,
    total_size: u64,
    skipped_files: usize,
}

fn year_from_system_time(time: SystemTime) -> Option<i32> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;

    // Convert Unix days to a Gregorian calendar year without adding a
    // date/time dependency.
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let month = mp + if mp < 10 { 3 } else { -9 };

    if month <= 2 {
        year += 1;
    }

    i32::try_from(year).ok()
}

fn scan_directory(
    root: &Path,
    directory: &Path,
    cutoff_year: i32,
    archive_destination: Option<&Path>,
    files: &mut Vec<ArchiveFile>,
    skipped_files: &mut usize,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => {
            *skipped_files += 1;
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Never follow symlinks.
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                *skipped_files += 1;
                continue;
            }
        };

        if metadata.file_type().is_symlink() {
            continue;
        }

        if metadata.is_dir() {
            if let Some(destination) = archive_destination {
                if path == destination || path.starts_with(destination) {
                    continue;
                }
            }

            scan_directory(
                root,
                &path,
                cutoff_year,
                archive_destination,
                files,
                skipped_files,
            );
            continue;
        }

        if !metadata.is_file() {
            continue;
        }

        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(_) => {
                *skipped_files += 1;
                continue;
            }
        };

        let Some(year) = year_from_system_time(modified) else {
            *skipped_files += 1;
            continue;
        };

        if year >= cutoff_year {
            continue;
        }

        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();

        let name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| relative_path.clone());

        let modified_ms = modified
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);

        files.push(ArchiveFile {
            name,
            relative_path,
            size: metadata.len(),
            modified_ms,
            year,
        });
    }
}

#[tauri::command]
fn scan_archive(
    source: String,
    archive_destination: Option<String>,
    cutoff_year: i32,
) -> Result<ScanResult, String> {
    if !(1970..=9999).contains(&cutoff_year) {
        return Err("Choose a valid cutoff year.".into());
    }

    let root = PathBuf::from(&source);

    if !root.is_dir() {
        return Err("The source folder is not available.".into());
    }

    let destination = archive_destination
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);

    let mut files = Vec::new();
    let mut skipped_files = 0;

    scan_directory(
        &root,
        &root,
        cutoff_year,
        destination.as_deref(),
        &mut files,
        &mut skipped_files,
    );

    files.sort_by(|a, b| {
        b.year
            .cmp(&a.year)
            .then_with(|| a.relative_path.cmp(&b.relative_path))
    });

    let mut years = Vec::<YearSummary>::new();

    for file in &files {
        if let Some(summary) = years.iter_mut().find(|summary| summary.year == file.year) {
            summary.file_count += 1;
            summary.total_size += file.size;
        } else {
            years.push(YearSummary {
                year: file.year,
                file_count: 1,
                total_size: file.size,
            });
        }
    }

    years.sort_by(|a, b| b.year.cmp(&a.year));

    let total_size = files.iter().map(|file| file.size).sum();

    Ok(ScanResult {
        total_files: files.len(),
        total_size,
        files,
        years,
        skipped_files,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![scan_archive])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
