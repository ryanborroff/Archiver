use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize, Serialize)]
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedFile {
    source: String,
    destination: String,
    relative_path: String,
    year: i32,
    size: u64,
    status: String,
    detail: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchivePlan {
    files: Vec<PlannedFile>,
    ready_files: usize,
    ready_size: u64,
    conflicts: usize,
    changed_files: usize,
    missing_files: usize,
}

#[tauri::command]
fn plan_archive(
    source: String,
    archive_destination: String,
    files: Vec<ArchiveFile>,
) -> Result<ArchivePlan, String> {
    let root = fs::canonicalize(&source)
        .map_err(|_| "The source folder is no longer available.".to_string())?;

    let destination_root = PathBuf::from(&archive_destination);

    if archive_destination.trim().is_empty() {
        return Err("Choose an archive destination.".into());
    }

    // If the destination already exists, canonicalise it so path comparisons
    // are based on the real filesystem location.
    let canonical_destination = if destination_root.exists() {
        Some(
            fs::canonicalize(&destination_root)
                .map_err(|_| "The archive destination is not available.".to_string())?,
        )
    } else {
        None
    };

    if let Some(destination) = &canonical_destination {
        if destination == &root {
            return Err("The archive destination cannot be the source folder.".into());
        }

        if root.starts_with(destination) {
            return Err("The source folder cannot be inside the archive destination.".into());
        }
    }

    let mut planned = Vec::with_capacity(files.len());
    let mut ready_files = 0usize;
    let mut ready_size = 0u64;
    let mut conflicts = 0usize;
    let mut changed_files = 0usize;
    let mut missing_files = 0usize;

    for file in files {
        let source_path = root.join(&file.relative_path);

        // A relative path must never be allowed to escape the selected source.
        if !source_path.starts_with(&root) {
            return Err("An unsafe source path was found in the archive plan.".into());
        }

        let destination_path = destination_root
            .join(file.year.to_string())
            .join(&file.relative_path);

        let (status, detail) = match fs::symlink_metadata(&source_path) {
            Err(_) => {
                missing_files += 1;
                (
                    "missing".to_string(),
                    Some("Source file is no longer available.".to_string()),
                )
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                changed_files += 1;
                (
                    "changed".to_string(),
                    Some("Source is no longer the same regular file.".to_string()),
                )
            }
            Ok(metadata) => {
                let current_modified_ms = metadata
                    .modified()
                    .ok()
                    .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as u64);

                if metadata.len() != file.size || current_modified_ms != Some(file.modified_ms) {
                    changed_files += 1;
                    (
                        "changed".to_string(),
                        Some("File changed since the scan.".to_string()),
                    )
                } else if destination_path.exists() {
                    conflicts += 1;
                    (
                        "conflict".to_string(),
                        Some("A file or folder already exists at the destination.".to_string()),
                    )
                } else {
                    ready_files += 1;
                    ready_size += file.size;
                    ("ready".to_string(), None)
                }
            }
        };

        planned.push(PlannedFile {
            source: source_path.to_string_lossy().into_owned(),
            destination: destination_path.to_string_lossy().into_owned(),
            relative_path: file.relative_path,
            year: file.year,
            size: file.size,
            status,
            detail,
        });
    }

    Ok(ArchivePlan {
        files: planned,
        ready_files,
        ready_size,
        conflicts,
        changed_files,
        missing_files,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![scan_archive, plan_archive])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
