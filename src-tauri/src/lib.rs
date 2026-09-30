use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
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

#[derive(Debug, PartialEq)]
enum TransferFailure {
    UnsafeRelativePath,
    SourceMissing,
    SourceChanged,
    DestinationExists,
    DestinationParentBlocked,
    TemporaryFileExists,
    CopyFailed,
    VerificationFailed,
    FinalizeFailed,
    SourceDeleteFailed,
}

fn safe_relative_path(value: &str) -> Result<PathBuf, TransferFailure> {
    let path = Path::new(value);

    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(TransferFailure::UnsafeRelativePath);
    }

    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(TransferFailure::UnsafeRelativePath),
        }
    }

    Ok(path.to_path_buf())
}

fn modified_ms(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
}

fn verify_source(
    source_path: &Path,
    expected_size: u64,
    expected_modified_ms: u64,
) -> Result<(), TransferFailure> {
    let metadata = fs::symlink_metadata(source_path).map_err(|_| TransferFailure::SourceMissing)?;

    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(TransferFailure::SourceChanged);
    }

    if metadata.len() != expected_size || modified_ms(&metadata) != Some(expected_modified_ms) {
        return Err(TransferFailure::SourceChanged);
    }

    Ok(())
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, TransferFailure> {
    let mut left_file = fs::File::open(left).map_err(|_| TransferFailure::VerificationFailed)?;
    let mut right_file = fs::File::open(right).map_err(|_| TransferFailure::VerificationFailed)?;

    let left_len = left_file
        .metadata()
        .map_err(|_| TransferFailure::VerificationFailed)?
        .len();
    let right_len = right_file
        .metadata()
        .map_err(|_| TransferFailure::VerificationFailed)?
        .len();

    if left_len != right_len {
        return Ok(false);
    }

    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];

    loop {
        let left_read = left_file
            .read(&mut left_buffer)
            .map_err(|_| TransferFailure::VerificationFailed)?;
        let right_read = right_file
            .read(&mut right_buffer)
            .map_err(|_| TransferFailure::VerificationFailed)?;

        if left_read != right_read {
            return Ok(false);
        }

        if left_read == 0 {
            return Ok(true);
        }

        if left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
    }
}

fn temporary_path_for(destination: &Path) -> Result<PathBuf, TransferFailure> {
    let file_name = destination
        .file_name()
        .ok_or(TransferFailure::UnsafeRelativePath)?
        .to_string_lossy();

    Ok(destination.with_file_name(format!(".{file_name}.archiver-part")))
}

fn execute_file_transfer(
    source_root: &Path,
    destination_root: &Path,
    file: &ArchiveFile,
) -> Result<(), TransferFailure> {
    let relative_path = safe_relative_path(&file.relative_path)?;
    let source_path = source_root.join(&relative_path);

    if !source_path.starts_with(source_root) {
        return Err(TransferFailure::UnsafeRelativePath);
    }

    verify_source(&source_path, file.size, file.modified_ms)?;

    let destination_path = destination_root
        .join(file.year.to_string())
        .join(&relative_path);

    if destination_path.exists() {
        return Err(TransferFailure::DestinationExists);
    }

    let parent = destination_path
        .parent()
        .ok_or(TransferFailure::DestinationParentBlocked)?;

    fs::create_dir_all(parent).map_err(|_| TransferFailure::DestinationParentBlocked)?;

    // Re-check after directory creation. Another process could have created the
    // destination between the first check and this point.
    if destination_path.exists() {
        return Err(TransferFailure::DestinationExists);
    }

    let temporary_path = temporary_path_for(&destination_path)?;

    // Never remove an unknown temporary file. Its presence blocks the transfer.
    if temporary_path.exists() {
        return Err(TransferFailure::TemporaryFileExists);
    }

    let copy_result = (|| -> Result<(), TransferFailure> {
        let mut source = fs::File::open(&source_path).map_err(|_| TransferFailure::CopyFailed)?;

        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .map_err(|_| TransferFailure::CopyFailed)?;

        std::io::copy(&mut source, &mut temporary).map_err(|_| TransferFailure::CopyFailed)?;

        temporary.flush().map_err(|_| TransferFailure::CopyFailed)?;

        temporary
            .sync_all()
            .map_err(|_| TransferFailure::CopyFailed)?;

        Ok(())
    })();

    if let Err(error) = copy_result {
        // This temp path was created by this transfer attempt, so it is safe
        // for this attempt to clean it up.
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    let temporary_size = fs::metadata(&temporary_path)
        .map_err(|_| TransferFailure::VerificationFailed)?
        .len();

    if temporary_size != file.size || !files_equal(&source_path, &temporary_path)? {
        let _ = fs::remove_file(&temporary_path);
        return Err(TransferFailure::VerificationFailed);
    }

    // The source must still match the scan immediately before finalising.
    if let Err(error) = verify_source(&source_path, file.size, file.modified_ms) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    if destination_path.exists() {
        let _ = fs::remove_file(&temporary_path);
        return Err(TransferFailure::DestinationExists);
    }

    fs::rename(&temporary_path, &destination_path).map_err(|_| TransferFailure::FinalizeFailed)?;

    // Only after the verified copy has its final name do we remove the source.
    if fs::remove_file(&source_path).is_err() {
        return Err(TransferFailure::SourceDeleteFailed);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env, fs,
        time::{Duration, SystemTime},
    };

    struct TestFixture {
        root: PathBuf,
        source: PathBuf,
        destination: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();

            let root =
                env::temp_dir().join(format!("archiver-{name}-{}-{unique}", std::process::id()));
            let source = root.join("source");
            let destination = root.join("archive");

            fs::create_dir_all(&source).unwrap();
            fs::create_dir_all(&destination).unwrap();

            Self {
                root,
                source,
                destination,
            }
        }

        fn create_source_file(
            &self,
            relative_path: &str,
            contents: &[u8],
            year: i32,
        ) -> ArchiveFile {
            let path = self.source.join(relative_path);

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }

            fs::write(&path, contents).unwrap();

            let metadata = fs::metadata(&path).unwrap();

            ArchiveFile {
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                relative_path: relative_path.to_string(),
                size: metadata.len(),
                modified_ms: modified_ms(&metadata).unwrap(),
                year,
            }
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn safe_relative_paths_reject_escape_attempts() {
        assert_eq!(
            safe_relative_path("../escape.txt"),
            Err(TransferFailure::UnsafeRelativePath)
        );
        assert_eq!(
            safe_relative_path("/tmp/escape.txt"),
            Err(TransferFailure::UnsafeRelativePath)
        );
        assert!(safe_relative_path("Insurance/policy.pdf").is_ok());
    }

    #[test]
    fn transfer_copies_verifies_finalises_and_removes_source() {
        let fixture = TestFixture::new("successful-transfer");
        let contents = b"important financial document";
        let file = fixture.create_source_file("Insurance/policy.pdf", contents, 2020);

        execute_file_transfer(&fixture.source, &fixture.destination, &file).unwrap();

        let source_path = fixture.source.join("Insurance/policy.pdf");
        let destination_path = fixture
            .destination
            .join("2020")
            .join("Insurance/policy.pdf");

        assert!(!source_path.exists());
        assert_eq!(fs::read(destination_path).unwrap(), contents);
    }

    #[test]
    fn transfer_never_overwrites_existing_destination() {
        let fixture = TestFixture::new("destination-conflict");
        let file = fixture.create_source_file("accounts.txt", b"source", 2020);

        let destination_path = fixture.destination.join("2020/accounts.txt");
        fs::create_dir_all(destination_path.parent().unwrap()).unwrap();
        fs::write(&destination_path, b"existing").unwrap();

        let result = execute_file_transfer(&fixture.source, &fixture.destination, &file);

        assert_eq!(result, Err(TransferFailure::DestinationExists));
        assert_eq!(fs::read(&destination_path).unwrap(), b"existing");
        assert_eq!(
            fs::read(fixture.source.join("accounts.txt")).unwrap(),
            b"source"
        );
    }

    #[test]
    fn transfer_refuses_source_changed_since_scan() {
        let fixture = TestFixture::new("changed-source");
        let file = fixture.create_source_file("budget.txt", b"original", 2020);

        // Change the size so the test does not depend on filesystem timestamp
        // resolution.
        fs::write(fixture.source.join("budget.txt"), b"changed contents").unwrap();

        let result = execute_file_transfer(&fixture.source, &fixture.destination, &file);

        assert_eq!(result, Err(TransferFailure::SourceChanged));
        assert!(fixture.source.join("budget.txt").exists());
        assert!(!fixture.destination.join("2020/budget.txt").exists());
    }

    #[test]
    fn transfer_does_not_touch_preexisting_temp_file() {
        let fixture = TestFixture::new("existing-temp");
        let file = fixture.create_source_file("invoice.txt", b"source", 2020);

        let destination_path = fixture.destination.join("2020/invoice.txt");
        fs::create_dir_all(destination_path.parent().unwrap()).unwrap();

        let temporary_path = temporary_path_for(&destination_path).unwrap();
        fs::write(&temporary_path, b"belongs to something else").unwrap();

        let result = execute_file_transfer(&fixture.source, &fixture.destination, &file);

        assert_eq!(result, Err(TransferFailure::TemporaryFileExists));
        assert_eq!(
            fs::read(&temporary_path).unwrap(),
            b"belongs to something else"
        );
        assert!(fixture.source.join("invoice.txt").exists());
        assert!(!destination_path.exists());
    }

    #[test]
    fn files_equal_detects_different_contents_of_same_size() {
        let fixture = TestFixture::new("verification");
        let left = fixture.root.join("left");
        let right = fixture.root.join("right");

        fs::write(&left, b"abcdef").unwrap();
        fs::write(&right, b"abcdeg").unwrap();

        assert!(!files_equal(&left, &right).unwrap());
    }

    #[test]
    fn missing_source_is_never_created_at_destination() {
        let fixture = TestFixture::new("missing-source");

        let file = ArchiveFile {
            name: "gone.txt".into(),
            relative_path: "gone.txt".into(),
            size: 12,
            modified_ms: 0,
            year: 2020,
        };

        let result = execute_file_transfer(&fixture.source, &fixture.destination, &file);

        assert_eq!(result, Err(TransferFailure::SourceMissing));
        assert!(!fixture.destination.join("2020/gone.txt").exists());
    }

    #[test]
    fn year_conversion_handles_known_dates() {
        let jan_2020 = UNIX_EPOCH + Duration::from_secs(1_577_836_800);
        let dec_2021 = UNIX_EPOCH + Duration::from_secs(1_640_908_799);

        assert_eq!(year_from_system_time(jan_2020), Some(2020));
        assert_eq!(year_from_system_time(dec_2021), Some(2021));
    }
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
