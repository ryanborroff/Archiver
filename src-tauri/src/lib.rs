use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
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
        let relative_path = safe_relative_path(&file.relative_path)
            .map_err(|_| "An unsafe source path was found in the archive plan.".to_string())?;

        let source_path = root.join(&relative_path);

        let destination_path = destination_root
            .join(file.year.to_string())
            .join(&relative_path);

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

fn validate_source_path(
    source_root: &Path,
    relative_path: &Path,
) -> Result<PathBuf, TransferFailure> {
    let root_metadata =
        fs::symlink_metadata(source_root).map_err(|_| TransferFailure::SourceMissing)?;

    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(TransferFailure::SourceChanged);
    }

    let mut current = source_root.to_path_buf();
    let mut components = relative_path.components().peekable();

    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            return Err(TransferFailure::UnsafeRelativePath);
        };

        current.push(component);

        let metadata =
            fs::symlink_metadata(&current).map_err(|_| TransferFailure::SourceMissing)?;

        if metadata.file_type().is_symlink() {
            return Err(TransferFailure::SourceChanged);
        }

        if components.peek().is_some() {
            if !metadata.is_dir() {
                return Err(TransferFailure::SourceChanged);
            }
        } else if !metadata.is_file() {
            return Err(TransferFailure::SourceChanged);
        }
    }

    if current == source_root {
        return Err(TransferFailure::UnsafeRelativePath);
    }

    Ok(current)
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

fn prepare_destination_parent(
    destination_root: &Path,
    parent: &Path,
) -> Result<(), TransferFailure> {
    if !parent.starts_with(destination_root) {
        return Err(TransferFailure::DestinationParentBlocked);
    }

    let relative = parent
        .strip_prefix(destination_root)
        .map_err(|_| TransferFailure::DestinationParentBlocked)?;

    let mut current = destination_root.to_path_buf();

    // The selected destination itself must be a real directory, not a symlink.
    let root_metadata =
        fs::symlink_metadata(&current).map_err(|_| TransferFailure::DestinationParentBlocked)?;

    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(TransferFailure::DestinationParentBlocked);
    }

    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(TransferFailure::DestinationParentBlocked);
        };

        current.push(component);

        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(TransferFailure::DestinationParentBlocked);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(TransferFailure::DestinationParentBlocked),
                }

                // Re-read after creation. If another process won the race, it
                // still has to be a genuine directory rather than a symlink.
                let metadata = fs::symlink_metadata(&current)
                    .map_err(|_| TransferFailure::DestinationParentBlocked)?;

                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(TransferFailure::DestinationParentBlocked);
                }
            }
            Err(_) => return Err(TransferFailure::DestinationParentBlocked),
        }
    }

    Ok(())
}

fn finalise_without_overwrite(
    temporary_path: &Path,
    destination_path: &Path,
) -> Result<(), TransferFailure> {
    match fs::hard_link(temporary_path, destination_path) {
        Ok(()) => {
            // Both names now refer to the same verified file. Removing our
            // private temporary name leaves the destination intact.
            fs::remove_file(temporary_path).map_err(|_| TransferFailure::FinalizeFailed)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(TransferFailure::DestinationExists)
        }
        Err(_) => Err(TransferFailure::FinalizeFailed),
    }
}

#[cfg(target_os = "macos")]
fn copy_file_with_metadata(
    source_path: &Path,
    temporary_path: &Path,
) -> Result<(), TransferFailure> {
    let source = CString::new(source_path.as_os_str().as_bytes())
        .map_err(|_| TransferFailure::CopyFailed)?;
    let destination = CString::new(temporary_path.as_os_str().as_bytes())
        .map_err(|_| TransferFailure::CopyFailed)?;

    // COPYFILE_ALL preserves file data plus macOS metadata such as
    // timestamps, permissions, ACLs, extended attributes and resource forks.
    //
    // COPYFILE_EXCL ensures this operation can never replace an existing
    // Archiver temporary file.
    let flags = libc::COPYFILE_ACL
        | libc::COPYFILE_STAT
        | libc::COPYFILE_XATTR
        | libc::COPYFILE_DATA
        | libc::COPYFILE_EXCL;

    let result = unsafe {
        libc::copyfile(
            source.as_ptr(),
            destination.as_ptr(),
            std::ptr::null_mut(),
            flags,
        )
    };

    if result != 0 {
        return Err(TransferFailure::CopyFailed);
    }

    let temporary = fs::File::open(temporary_path).map_err(|_| TransferFailure::CopyFailed)?;

    temporary
        .sync_all()
        .map_err(|_| TransferFailure::CopyFailed)?;

    Ok(())
}

fn execute_file_transfer(
    source_root: &Path,
    destination_root: &Path,
    file: &ArchiveFile,
) -> Result<(), TransferFailure> {
    let relative_path = safe_relative_path(&file.relative_path)?;
    let source_path = validate_source_path(source_root, &relative_path)?;

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

    prepare_destination_parent(destination_root, parent)?;

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

    let copy_result = copy_file_with_metadata(&source_path, &temporary_path);

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

    if let Err(error) = finalise_without_overwrite(&temporary_path, &destination_path) {
        // The temporary file belongs to this transfer attempt. If finalisation
        // fails, clean it up but never touch the destination.
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    // Only after the verified copy has its final name do we remove the source.
    if fs::remove_file(&source_path).is_err() {
        return Err(TransferFailure::SourceDeleteFailed);
    }

    Ok(())
}

static ARCHIVE_EXECUTION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum ExecutionStatus {
    Archived,
    SourceRetained,
    Failed,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionItem {
    relative_path: String,
    destination: String,
    status: ExecutionStatus,
    detail: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionResult {
    items: Vec<ExecutionItem>,
    archived_files: usize,
    archived_size: u64,
    source_retained_files: usize,
    failed_files: usize,
}

fn failure_message(error: &TransferFailure) -> &'static str {
    match error {
        TransferFailure::UnsafeRelativePath => "Unsafe relative path.",
        TransferFailure::SourceMissing => "Source file is missing.",
        TransferFailure::SourceChanged => "Source file changed since the scan.",
        TransferFailure::DestinationExists => "Destination already exists.",
        TransferFailure::DestinationParentBlocked => "Destination folder could not be prepared.",
        TransferFailure::TemporaryFileExists => "A temporary Archiver file already exists.",
        TransferFailure::CopyFailed => "Copy failed.",
        TransferFailure::VerificationFailed => "Copied file could not be verified.",
        TransferFailure::FinalizeFailed => "Copied file could not be finalised.",
        TransferFailure::SourceDeleteFailed => {
            "Copy was verified and archived, but the source could not be removed."
        }
    }
}

fn destination_for_file(
    destination_root: &Path,
    file: &ArchiveFile,
) -> Result<PathBuf, TransferFailure> {
    let relative_path = safe_relative_path(&file.relative_path)?;

    Ok(destination_root
        .join(file.year.to_string())
        .join(relative_path))
}

fn execute_archive_batch(
    source_root: &Path,
    destination_root: &Path,
    files: &[ArchiveFile],
) -> ExecutionResult {
    let mut items = Vec::with_capacity(files.len());
    let mut archived_files = 0usize;
    let mut archived_size = 0u64;
    let mut source_retained_files = 0usize;
    let mut failed_files = 0usize;

    for file in files {
        let destination = destination_for_file(destination_root, file)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();

        match execute_file_transfer(source_root, destination_root, file) {
            Ok(()) => {
                archived_files += 1;
                archived_size += file.size;

                items.push(ExecutionItem {
                    relative_path: file.relative_path.clone(),
                    destination,
                    status: ExecutionStatus::Archived,
                    detail: None,
                });
            }
            Err(TransferFailure::SourceDeleteFailed) => {
                source_retained_files += 1;

                items.push(ExecutionItem {
                    relative_path: file.relative_path.clone(),
                    destination,
                    status: ExecutionStatus::SourceRetained,
                    detail: Some(failure_message(&TransferFailure::SourceDeleteFailed).to_string()),
                });
            }
            Err(error) => {
                failed_files += 1;

                items.push(ExecutionItem {
                    relative_path: file.relative_path.clone(),
                    destination,
                    status: ExecutionStatus::Failed,
                    detail: Some(failure_message(&error).to_string()),
                });
            }
        }
    }

    ExecutionResult {
        items,
        archived_files,
        archived_size,
        source_retained_files,
        failed_files,
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum JournalState {
    InProgress,
    Completed,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveJournal {
    version: u32,
    operation_id: String,
    created_ms: u64,
    updated_ms: u64,
    state: JournalState,
    source_root: String,
    destination_root: String,
    planned_files: usize,
    planned_size: u64,
    active_file: Option<String>,
    result: ExecutionResult,
}

fn empty_execution_result() -> ExecutionResult {
    ExecutionResult {
        items: Vec::new(),
        archived_files: 0,
        archived_size: 0,
        source_retained_files: 0,
        failed_files: 0,
    }
}

fn current_time_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|_| "System clock is before the Unix epoch.".to_string())
}

fn journal_path(destination_root: &Path, operation_id: &str) -> PathBuf {
    destination_root
        .join(".archiver")
        .join("manifests")
        .join(format!("archive-{operation_id}.json"))
}

fn persist_journal(path: &Path, journal: &ArchiveJournal) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not determine journal folder.".to_string())?;

    let destination_root = path
        .ancestors()
        .nth(3)
        .ok_or_else(|| "Could not determine archive destination.".to_string())?;

    prepare_destination_parent(destination_root, parent)
        .map_err(|_| "Journal folder is unsafe or could not be prepared.".to_string())?;

    let file_name = path
        .file_name()
        .ok_or_else(|| "Could not determine journal filename.".to_string())?
        .to_string_lossy();

    let temp_path = path.with_file_name(format!(".{file_name}.archiver-part"));

    // We only ever replace our own journal temp file. An unexpected one is
    // treated as evidence of an interrupted or concurrent write.
    if temp_path.exists() {
        return Err("An unfinished journal update already exists.".to_string());
    }

    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|_| "Could not serialize archive journal.".to_string())?;

    let write_result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|_| "Could not create temporary journal.".to_string())?;

        file.write_all(&bytes)
            .map_err(|_| "Could not write archive journal.".to_string())?;

        file.flush()
            .map_err(|_| "Could not flush archive journal.".to_string())?;

        file.sync_all()
            .map_err(|_| "Could not sync archive journal.".to_string())?;

        Ok(())
    })();

    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }

    // On our macOS target, rename replaces the existing journal atomically.
    fs::rename(&temp_path, path).map_err(|_| "Could not finalise archive journal.".to_string())?;

    Ok(())
}

fn create_archive_journal(
    destination_root: &Path,
    source_root: &Path,
    files: &[ArchiveFile],
) -> Result<(PathBuf, ArchiveJournal), String> {
    let created_ms = current_time_ms()?;

    // Timestamp plus process ID keeps operation names readable while avoiding
    // collisions between separate Archiver processes started in the same ms.
    let operation_id = format!("{created_ms}-{}", std::process::id());

    let path = journal_path(destination_root, &operation_id);

    if path.exists() {
        return Err("An archive journal with this identifier already exists.".to_string());
    }

    let journal = ArchiveJournal {
        version: 3,
        operation_id,
        created_ms,
        updated_ms: created_ms,
        state: JournalState::InProgress,
        source_root: source_root.to_string_lossy().into_owned(),
        destination_root: destination_root.to_string_lossy().into_owned(),
        planned_files: files.len(),
        planned_size: files.iter().map(|file| file.size).sum(),
        active_file: None,
        result: empty_execution_result(),
    };

    persist_journal(&path, &journal)?;

    Ok((path, journal))
}

fn set_journal_active_file(
    path: &Path,
    journal: &mut ArchiveJournal,
    relative_path: &str,
) -> Result<(), String> {
    journal.active_file = Some(relative_path.to_string());
    journal.updated_ms = current_time_ms()?;

    persist_journal(path, journal)
}

fn append_journal_result(
    path: &Path,
    journal: &mut ArchiveJournal,
    item: ExecutionItem,
    file_size: u64,
) -> Result<(), String> {
    match item.status {
        ExecutionStatus::Archived => {
            journal.result.archived_files += 1;
            journal.result.archived_size += file_size;
        }
        ExecutionStatus::SourceRetained => {
            journal.result.source_retained_files += 1;
        }
        ExecutionStatus::Failed => {
            journal.result.failed_files += 1;
        }
    }

    journal.result.items.push(item);
    journal.active_file = None;
    journal.updated_ms = current_time_ms()?;

    persist_journal(path, journal)
}

fn complete_archive_journal(path: &Path, journal: &mut ArchiveJournal) -> Result<(), String> {
    if journal.active_file.is_some() {
        return Err("Cannot complete an archive journal while a file is still active.".to_string());
    }

    journal.state = JournalState::Completed;
    journal.updated_ms = current_time_ms()?;

    persist_journal(path, journal)
}

fn read_archive_journal(path: &Path) -> Result<ArchiveJournal, String> {
    let bytes = fs::read(path).map_err(|_| "Could not read archive journal.".to_string())?;

    serde_json::from_slice(&bytes).map_err(|_| "Archive journal is not valid JSON.".to_string())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveExecutionResponse {
    result: ExecutionResult,
    journal_path: String,
}

fn validate_execution_roots(
    source: &Path,
    destination: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    // Check the paths exactly as supplied before canonicalising them.
    // Canonicalisation follows symlinks, so checking only afterwards would
    // make a selected symlink indistinguishable from its target.
    let supplied_source_metadata = fs::symlink_metadata(source)
        .map_err(|_| "The source folder is no longer available.".to_string())?;

    let supplied_destination_metadata = fs::symlink_metadata(destination)
        .map_err(|_| "The archive destination is no longer available.".to_string())?;

    if supplied_source_metadata.file_type().is_symlink() || !supplied_source_metadata.is_dir() {
        return Err("The source folder is not a regular directory.".to_string());
    }

    if supplied_destination_metadata.file_type().is_symlink()
        || !supplied_destination_metadata.is_dir()
    {
        return Err("The archive destination is not a regular directory.".to_string());
    }

    let source_root = fs::canonicalize(source)
        .map_err(|_| "The source folder is no longer available.".to_string())?;

    let destination_root = fs::canonicalize(destination)
        .map_err(|_| "The archive destination is no longer available.".to_string())?;

    let source_metadata = fs::symlink_metadata(&source_root)
        .map_err(|_| "The source folder is no longer available.".to_string())?;

    let destination_metadata = fs::symlink_metadata(&destination_root)
        .map_err(|_| "The archive destination is no longer available.".to_string())?;

    if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
        return Err("The source folder is not a regular directory.".to_string());
    }

    if destination_metadata.file_type().is_symlink() || !destination_metadata.is_dir() {
        return Err("The archive destination is not a regular directory.".to_string());
    }

    if source_root == destination_root {
        return Err("The archive destination cannot be the source folder.".to_string());
    }

    if source_root.starts_with(&destination_root) {
        return Err("The source folder cannot be inside the archive destination.".to_string());
    }

    Ok((source_root, destination_root))
}

fn preflight_archive_execution(
    source_root: &Path,
    destination_root: &Path,
    files: &[ArchiveFile],
) -> Result<(), String> {
    if files.is_empty() {
        return Err("There are no files ready to archive.".to_string());
    }

    for file in files {
        let relative_path = safe_relative_path(&file.relative_path)
            .map_err(|_| "The archive plan contains an unsafe file path.".to_string())?;

        let source_path = validate_source_path(source_root, &relative_path)
            .map_err(|error| failure_message(&error).to_string())?;

        verify_source(&source_path, file.size, file.modified_ms)
            .map_err(|error| failure_message(&error).to_string())?;

        let destination_path = destination_root
            .join(file.year.to_string())
            .join(&relative_path);

        // symlink_metadata catches dangling symlinks as conflicts too.
        if fs::symlink_metadata(&destination_path).is_ok() {
            return Err(format!(
                "Archive destination already exists for {}.",
                file.relative_path
            ));
        }

        let parent = destination_path
            .parent()
            .ok_or_else(|| "The archive destination path is invalid.".to_string())?;

        // Validate existing destination components without creating anything.
        if !parent.starts_with(destination_root) {
            return Err("The archive destination path is unsafe.".to_string());
        }

        let relative_parent = parent
            .strip_prefix(destination_root)
            .map_err(|_| "The archive destination path is unsafe.".to_string())?;

        let mut current = destination_root.to_path_buf();

        for component in relative_parent.components() {
            let Component::Normal(component) = component else {
                return Err("The archive destination path is unsafe.".to_string());
            };

            current.push(component);

            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(format!(
                            "Archive destination is blocked for {}.",
                            file.relative_path
                        ));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Missing directories are fine. They will be created only
                    // after the entire batch passes preflight.
                    break;
                }
                Err(_) => {
                    return Err(format!(
                        "Archive destination could not be checked for {}.",
                        file.relative_path
                    ));
                }
            }
        }
    }

    Ok(())
}

fn execute_archive_operation(
    source: &Path,
    destination: &Path,
    files: &[ArchiveFile],
) -> Result<ArchiveExecutionResponse, String> {
    let (source_root, destination_root) = validate_execution_roots(source, destination)?;

    // Critical rule: nothing is moved until every supplied file passes.
    preflight_archive_execution(&source_root, &destination_root, files)?;

    let (journal_path, mut journal) =
        create_archive_journal(&destination_root, &source_root, files)?;

    for file in files {
        let destination_path = destination_for_file(&destination_root, file)
            .map_err(|error| failure_message(&error).to_string())?;

        let destination_string = destination_path.to_string_lossy().into_owned();

        // Persist which file is about to be transferred before touching it.
        // If the process stops during the transfer, recovery can inspect this
        // exact source/destination pair rather than guessing.
        set_journal_active_file(&journal_path, &mut journal, &file.relative_path)?;

        let item = match execute_file_transfer(&source_root, &destination_root, file) {
            Ok(()) => ExecutionItem {
                relative_path: file.relative_path.clone(),
                destination: destination_string,
                status: ExecutionStatus::Archived,
                detail: None,
            },
            Err(TransferFailure::SourceDeleteFailed) => ExecutionItem {
                relative_path: file.relative_path.clone(),
                destination: destination_string,
                status: ExecutionStatus::SourceRetained,
                detail: Some(failure_message(&TransferFailure::SourceDeleteFailed).to_string()),
            },
            Err(error) => {
                let detail = failure_message(&error).to_string();

                let item = ExecutionItem {
                    relative_path: file.relative_path.clone(),
                    destination: destination_string,
                    status: ExecutionStatus::Failed,
                    detail: Some(detail.clone()),
                };

                // Persist the failure before returning. The journal deliberately
                // remains InProgress so interrupted/partial work is visible.
                append_journal_result(&journal_path, &mut journal, item, file.size)?;

                return Err(format!(
                    "Archiving stopped at {}: {}",
                    file.relative_path, detail
                ));
            }
        };

        append_journal_result(&journal_path, &mut journal, item, file.size)?;
    }

    complete_archive_journal(&journal_path, &mut journal)?;

    Ok(ArchiveExecutionResponse {
        result: journal.result,
        journal_path: journal_path.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
fn execute_archive(
    source: String,
    archive_destination: String,
    files: Vec<ArchiveFile>,
) -> Result<ArchiveExecutionResponse, String> {
    if source.trim().is_empty() {
        return Err("Choose a source folder.".to_string());
    }

    if archive_destination.trim().is_empty() {
        return Err("Choose an archive destination.".to_string());
    }

    with_archive_execution_lock(|| {
        execute_archive_operation(Path::new(&source), Path::new(&archive_destination), &files)
    })?
}

fn with_archive_execution_lock<T>(operation: impl FnOnce() -> T) -> Result<T, String> {
    let lock = ARCHIVE_EXECUTION_LOCK.get_or_init(|| Mutex::new(()));

    let _guard = lock
        .try_lock()
        .map_err(|_| "Another archive operation is already running.".to_string())?;

    Ok(operation())
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

    #[cfg(target_os = "macos")]
    #[test]
    fn transfer_preserves_macos_file_metadata() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::process::Command;

        let fixture = TestFixture::new("metadata-fidelity");

        let file = fixture.create_source_file("fidelity.txt", b"fidelity data", 2020);
        let source_path = fixture.source.join("fidelity.txt");

        // Give the source a distinctive permission mode.
        fs::set_permissions(&source_path, fs::Permissions::from_mode(0o640)).unwrap();

        // Give it a known extended attribute.
        let status = Command::new("/usr/bin/xattr")
            .args(["-w", "com.archiver.test", "preserve-me"])
            .arg(&source_path)
            .status()
            .unwrap();

        assert!(status.success());

        // Capture the source metadata immediately before transfer.
        let source_metadata = fs::metadata(&source_path).unwrap();
        let source_mtime_sec = source_metadata.mtime();
        let source_mtime_nsec = source_metadata.mtime_nsec();
        let source_mode = source_metadata.permissions().mode() & 0o777;

        execute_file_transfer(&fixture.source, &fixture.destination, &file).unwrap();

        let destination_path = fixture.destination.join("2020/fidelity.txt");

        assert!(!source_path.exists());
        assert_eq!(fs::read(&destination_path).unwrap(), b"fidelity data");

        let destination_metadata = fs::metadata(&destination_path).unwrap();

        assert_eq!(destination_metadata.mtime(), source_mtime_sec);
        assert_eq!(destination_metadata.mtime_nsec(), source_mtime_nsec);
        assert_eq!(
            destination_metadata.permissions().mode() & 0o777,
            source_mode
        );

        let output = Command::new("/usr/bin/xattr")
            .args(["-p", "com.archiver.test"])
            .arg(&destination_path)
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim_end(),
            "preserve-me"
        );
    }

    #[test]
    fn batch_archives_multiple_files() {
        let fixture = TestFixture::new("batch-success");

        let first = fixture.create_source_file("Accounts/a.txt", b"alpha", 2020);
        let second = fixture.create_source_file("Insurance/b.txt", b"bravo", 2021);

        let files = vec![first, second];

        let result = execute_archive_batch(&fixture.source, &fixture.destination, &files);

        assert_eq!(result.archived_files, 2);
        assert_eq!(result.failed_files, 0);
        assert_eq!(result.source_retained_files, 0);

        assert!(!fixture.source.join("Accounts/a.txt").exists());
        assert!(!fixture.source.join("Insurance/b.txt").exists());

        assert_eq!(
            fs::read(fixture.destination.join("2020/Accounts/a.txt")).unwrap(),
            b"alpha"
        );
        assert_eq!(
            fs::read(fixture.destination.join("2021/Insurance/b.txt")).unwrap(),
            b"bravo"
        );
    }

    #[test]
    fn batch_failure_does_not_prevent_other_safe_files() {
        let fixture = TestFixture::new("batch-partial");

        let first = fixture.create_source_file("good.txt", b"good", 2020);
        let second = fixture.create_source_file("conflict.txt", b"source", 2020);

        let conflict_destination = fixture.destination.join("2020/conflict.txt");
        fs::create_dir_all(conflict_destination.parent().unwrap()).unwrap();
        fs::write(&conflict_destination, b"existing").unwrap();

        let files = vec![first, second];

        let result = execute_archive_batch(&fixture.source, &fixture.destination, &files);

        assert_eq!(result.archived_files, 1);
        assert_eq!(result.failed_files, 1);
        assert_eq!(result.source_retained_files, 0);

        assert!(!fixture.source.join("good.txt").exists());
        assert!(fixture.source.join("conflict.txt").exists());

        assert_eq!(
            fs::read(fixture.destination.join("2020/good.txt")).unwrap(),
            b"good"
        );
        assert_eq!(fs::read(conflict_destination).unwrap(), b"existing");
    }

    #[test]
    fn execution_lock_rejects_concurrent_operation() {
        let lock = ARCHIVE_EXECUTION_LOCK.get_or_init(|| Mutex::new(()));
        let guard = lock.lock().unwrap();

        let result = with_archive_execution_lock(|| 42);

        assert_eq!(
            result,
            Err("Another archive operation is already running.".to_string())
        );

        drop(guard);

        assert_eq!(with_archive_execution_lock(|| 42), Ok(42));
    }

    #[cfg(unix)]
    #[test]
    fn journal_rejects_symlinked_archiver_folder() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new("journal-archiver-symlink");
        let outside = fixture.root.join("outside");
        fs::create_dir_all(&outside).unwrap();

        symlink(&outside, fixture.destination.join(".archiver")).unwrap();

        let file = fixture.create_source_file("report.txt", b"report", 2020);

        let result = create_archive_journal(&fixture.destination, &fixture.source, &[file]);

        assert!(result.is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn journal_rejects_symlinked_manifests_folder() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new("journal-manifests-symlink");
        let outside = fixture.root.join("outside");
        fs::create_dir_all(&outside).unwrap();

        let archiver = fixture.destination.join(".archiver");
        fs::create_dir(&archiver).unwrap();
        symlink(&outside, archiver.join("manifests")).unwrap();

        let file = fixture.create_source_file("report.txt", b"report", 2020);

        let result = create_archive_journal(&fixture.destination, &fixture.source, &[file]);

        assert!(result.is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn journal_exists_before_any_transfer() {
        let fixture = TestFixture::new("journal-before-transfer");

        let file = fixture.create_source_file("Accounts/report.txt", b"report", 2020);

        let (path, journal) =
            create_archive_journal(&fixture.destination, &fixture.source, &[file]).unwrap();

        assert!(path.exists());
        assert_eq!(journal.version, 3);
        assert_eq!(journal.state, JournalState::InProgress);
        assert_eq!(journal.planned_files, 1);
        assert_eq!(journal.planned_size, 6);
        assert_eq!(journal.result.items.len(), 0);

        let stored = read_archive_journal(&path).unwrap();

        assert_eq!(stored.state, JournalState::InProgress);
        assert_eq!(stored.result.archived_files, 0);
        assert_eq!(stored.result.failed_files, 0);
    }

    #[test]
    fn journal_persists_each_completed_file() {
        let fixture = TestFixture::new("journal-progress");

        let file = fixture.create_source_file("Accounts/report.txt", b"report", 2020);

        let (path, mut journal) = create_archive_journal(
            &fixture.destination,
            &fixture.source,
            std::slice::from_ref(&file),
        )
        .unwrap();

        execute_file_transfer(&fixture.source, &fixture.destination, &file).unwrap();

        let destination = destination_for_file(&fixture.destination, &file)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        append_journal_result(
            &path,
            &mut journal,
            ExecutionItem {
                relative_path: file.relative_path.clone(),
                destination,
                status: ExecutionStatus::Archived,
                detail: None,
            },
            file.size,
        )
        .unwrap();

        let stored = read_archive_journal(&path).unwrap();

        assert_eq!(stored.state, JournalState::InProgress);
        assert_eq!(stored.result.archived_files, 1);
        assert_eq!(stored.result.archived_size, file.size);
        assert_eq!(stored.result.items.len(), 1);
        assert_eq!(stored.result.items[0].status, ExecutionStatus::Archived);
    }

    #[test]
    fn journal_is_only_completed_explicitly() {
        let fixture = TestFixture::new("journal-complete");

        let file = fixture.create_source_file("report.txt", b"report", 2020);

        let (path, mut journal) =
            create_archive_journal(&fixture.destination, &fixture.source, &[file]).unwrap();

        assert_eq!(
            read_archive_journal(&path).unwrap().state,
            JournalState::InProgress
        );

        complete_archive_journal(&path, &mut journal).unwrap();

        assert_eq!(
            read_archive_journal(&path).unwrap().state,
            JournalState::Completed
        );
    }

    #[test]
    fn unfinished_journal_is_detectable_after_interruption() {
        let fixture = TestFixture::new("journal-interrupted");

        let file = fixture.create_source_file("report.txt", b"report", 2020);

        let (path, _) =
            create_archive_journal(&fixture.destination, &fixture.source, &[file]).unwrap();

        // Simulate the app stopping without completing the journal.
        let recovered = read_archive_journal(&path).unwrap();

        assert_eq!(recovered.state, JournalState::InProgress);
        assert_eq!(recovered.planned_files, 1);
    }

    #[test]
    fn finalisation_never_overwrites_destination() {
        let fixture = TestFixture::new("atomic-no-overwrite");

        let temporary = fixture.destination.join(".report.txt.archiver-part");
        let destination = fixture.destination.join("report.txt");

        fs::write(&temporary, b"new archive data").unwrap();
        fs::write(&destination, b"existing data").unwrap();

        let result = finalise_without_overwrite(&temporary, &destination);

        assert_eq!(result, Err(TransferFailure::DestinationExists));
        assert_eq!(fs::read(&destination).unwrap(), b"existing data");
        assert_eq!(fs::read(&temporary).unwrap(), b"new archive data");
    }

    #[cfg(unix)]
    #[test]
    fn destination_parent_rejects_symlinked_component() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new("destination-symlink");
        let outside = fixture.root.join("outside");
        fs::create_dir_all(&outside).unwrap();

        let year = fixture.destination.join("2020");
        fs::create_dir_all(&year).unwrap();

        let linked = year.join("Accounts");
        symlink(&outside, &linked).unwrap();

        let result = prepare_destination_parent(&fixture.destination, &linked);

        assert_eq!(result, Err(TransferFailure::DestinationParentBlocked));
    }

    #[test]
    fn source_path_accepts_normal_nested_file() {
        let fixture = TestFixture::new("source-path-normal");
        fixture.create_source_file("Accounts/Tax/report.txt", b"report", 2020);

        let relative = Path::new("Accounts/Tax/report.txt");

        let validated = validate_source_path(&fixture.source, relative).unwrap();

        assert_eq!(validated, fixture.source.join("Accounts/Tax/report.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn source_path_rejects_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new("source-parent-symlink");

        let outside = fixture.root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("report.txt"), b"outside").unwrap();

        symlink(&outside, fixture.source.join("Accounts")).unwrap();

        let result = validate_source_path(&fixture.source, Path::new("Accounts/report.txt"));

        assert_eq!(result, Err(TransferFailure::SourceChanged));
    }

    #[cfg(unix)]
    #[test]
    fn transfer_refuses_symlinked_source_parent() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new("transfer-source-parent-symlink");

        let outside = fixture.root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("report.txt");
        fs::write(&outside_file, b"report").unwrap();

        let metadata = fs::metadata(&outside_file).unwrap();

        symlink(&outside, fixture.source.join("Accounts")).unwrap();

        let file = ArchiveFile {
            name: "report.txt".into(),
            relative_path: "Accounts/report.txt".into(),
            size: metadata.len(),
            modified_ms: modified_ms(&metadata).unwrap(),
            year: 2020,
        };

        let result = execute_file_transfer(&fixture.source, &fixture.destination, &file);

        assert_eq!(result, Err(TransferFailure::SourceChanged));
        assert_eq!(fs::read(&outside_file).unwrap(), b"report");
        assert!(!fixture
            .destination
            .join("2020/Accounts/report.txt")
            .exists());
    }

    #[test]
    fn destination_parent_creates_normal_directories() {
        let fixture = TestFixture::new("destination-parent");

        let parent = fixture
            .destination
            .join("2020")
            .join("Accounts")
            .join("Tax");

        prepare_destination_parent(&fixture.destination, &parent).unwrap();

        assert!(parent.is_dir());
        assert!(!fs::symlink_metadata(&parent)
            .unwrap()
            .file_type()
            .is_symlink());
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
    fn journal_records_active_file_before_transfer() {
        let fixture = TestFixture::new("journal-active");
        let file = fixture.create_source_file("Accounts/tax.pdf", b"tax", 2020);

        let (path, mut journal) =
            create_archive_journal(&fixture.destination, &fixture.source, &[file]).unwrap();

        set_journal_active_file(&path, &mut journal, "Accounts/tax.pdf").unwrap();

        let recovered = read_archive_journal(&path).unwrap();

        assert_eq!(recovered.active_file.as_deref(), Some("Accounts/tax.pdf"));
        assert_eq!(recovered.state, JournalState::InProgress);
        assert!(recovered.result.items.is_empty());
    }

    #[test]
    fn journal_result_clears_active_file() {
        let fixture = TestFixture::new("journal-active-clear");
        let file = fixture.create_source_file("report.txt", b"report", 2020);

        let (path, mut journal) =
            create_archive_journal(&fixture.destination, &fixture.source, &[file]).unwrap();

        set_journal_active_file(&path, &mut journal, "report.txt").unwrap();

        append_journal_result(
            &path,
            &mut journal,
            ExecutionItem {
                relative_path: "report.txt".to_string(),
                destination: fixture
                    .destination
                    .join("2020/report.txt")
                    .to_string_lossy()
                    .into_owned(),
                status: ExecutionStatus::Archived,
                detail: None,
            },
            6,
        )
        .unwrap();

        let recovered = read_archive_journal(&path).unwrap();

        assert_eq!(recovered.active_file, None);
        assert_eq!(recovered.result.archived_files, 1);
        assert_eq!(recovered.result.items.len(), 1);
    }

    #[test]
    fn interrupted_active_file_remains_recoverable() {
        let fixture = TestFixture::new("journal-interrupted-active");
        let file = fixture.create_source_file("unfinished.txt", b"unfinished", 2020);

        let (path, mut journal) =
            create_archive_journal(&fixture.destination, &fixture.source, &[file]).unwrap();

        set_journal_active_file(&path, &mut journal, "unfinished.txt").unwrap();

        // Simulate process termination before a result can be recorded.
        let recovered = read_archive_journal(&path).unwrap();

        assert_eq!(recovered.state, JournalState::InProgress);
        assert_eq!(recovered.active_file.as_deref(), Some("unfinished.txt"));
        assert_eq!(recovered.result.archived_files, 0);
    }

    #[test]
    fn execution_preflight_failure_archives_nothing() {
        let fixture = TestFixture::new("execution-preflight");

        let first = fixture.create_source_file("first.txt", b"first", 2020);
        let second = fixture.create_source_file("second.txt", b"second", 2020);

        fs::create_dir_all(fixture.destination.join("2020")).unwrap();
        fs::write(fixture.destination.join("2020/second.txt"), b"existing").unwrap();

        let result =
            execute_archive_operation(&fixture.source, &fixture.destination, &[first, second]);

        assert!(result.is_err());
        assert!(fixture.source.join("first.txt").exists());
        assert!(fixture.source.join("second.txt").exists());
        assert!(!fixture.destination.join("2020/first.txt").exists());
        assert_eq!(
            fs::read(fixture.destination.join("2020/second.txt")).unwrap(),
            b"existing"
        );
    }

    #[test]
    fn execution_archives_batch_and_completes_journal() {
        let fixture = TestFixture::new("execution-complete");

        let first = fixture.create_source_file("Accounts/first.txt", b"first", 2020);
        let second = fixture.create_source_file("second.txt", b"second", 2021);

        let response =
            execute_archive_operation(&fixture.source, &fixture.destination, &[first, second])
                .unwrap();

        assert_eq!(response.result.archived_files, 2);
        assert_eq!(response.result.failed_files, 0);
        assert_eq!(response.result.source_retained_files, 0);

        assert!(!fixture.source.join("Accounts/first.txt").exists());
        assert!(!fixture.source.join("second.txt").exists());

        assert_eq!(
            fs::read(fixture.destination.join("2020/Accounts/first.txt")).unwrap(),
            b"first"
        );

        assert_eq!(
            fs::read(fixture.destination.join("2021/second.txt")).unwrap(),
            b"second"
        );

        let journal = read_archive_journal(Path::new(&response.journal_path)).unwrap();

        assert_eq!(journal.state, JournalState::Completed);
        assert_eq!(journal.result.archived_files, 2);
        assert_eq!(journal.result.items.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn execution_roots_reject_symlinked_source_root() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new("symlink-source-root");
        let source_link = fixture.root.join("source-link");

        symlink(&fixture.source, &source_link).unwrap();

        let result = validate_execution_roots(&source_link, &fixture.destination);

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn execution_roots_reject_symlinked_destination_root() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new("symlink-destination-root");
        let destination_link = fixture.root.join("destination-link");

        symlink(&fixture.destination, &destination_link).unwrap();

        let result = validate_execution_roots(&fixture.source, &destination_link);

        assert!(result.is_err());
    }

    #[test]
    fn execution_rejects_empty_batch() {
        let fixture = TestFixture::new("execution-empty");

        let result = execute_archive_operation(&fixture.source, &fixture.destination, &[]);

        assert!(result.is_err());

        let manifests = fixture.destination.join(".archiver/manifests");
        assert!(!manifests.exists());
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
        .invoke_handler(tauri::generate_handler![
            scan_archive,
            plan_archive,
            execute_archive
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
