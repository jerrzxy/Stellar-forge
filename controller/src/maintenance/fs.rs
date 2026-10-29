//! Filesystem operations used by the local bucket pruner.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// An XDR file found beneath the configured bucket directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageFile {
    /// Canonical path to the regular file.
    pub path: PathBuf,
    /// File category. Bucket files are never inferred to be obsolete by age.
    pub kind: StorageFileKind,
    /// Current size in bytes.
    pub size_bytes: u64,
}

/// XDR file categories understood by the pruner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageFileKind {
    /// A ledger XDR file whose ledger sequence is encoded in its filename.
    LedgerXdr { ledger: u32 },
    /// A bucket XDR file; liveness must be established by Core inventory.
    Bucket,
}

/// A shared lock for a file being read by a cooperating consumer.
///
/// Consumers that query bucket files must hold this guard for the duration of
/// the read so the pruner's exclusive lock can detect active queries.
#[derive(Debug)]
pub struct FileReadGuard {
    _file: File,
}

impl FileReadGuard {
    /// Opens a file for reading and holds a shared lock until this guard drops.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        file.lock_shared()?;
        Ok(Self { _file: file })
    }
}

/// Recursively scans regular `.xdr` files without following symbolic links.
pub async fn scan_bucket_dir(root: impl AsRef<Path>) -> io::Result<Vec<StorageFile>> {
    let root = root.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || scan_bucket_dir_blocking(&root))
        .await
        .map_err(io::Error::other)?
}

/// Attempts to acquire an exclusive lock and remove a single regular file.
///
/// Returns `Ok(false)` when a cooperating reader currently holds a shared lock.
pub(crate) async fn remove_if_unlocked(path: impl AsRef<Path>) -> io::Result<bool> {
    let path = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new().read(true).open(&path)?;
        match file.try_lock() {
            Ok(()) => {
                fs::remove_file(path)?;
                drop(file);
                Ok(true)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    })
    .await
    .map_err(io::Error::other)?
}

fn scan_bucket_dir_blocking(root: &Path) -> io::Result<Vec<StorageFile>> {
    let root = fs::canonicalize(root)?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bucket path is not a directory: {}", root.display()),
        ));
    }

    let mut files = Vec::new();
    scan_directory(&root, &root, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn scan_directory(root: &Path, directory: &Path, files: &mut Vec<StorageFile>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            scan_directory(root, &entry.path(), files)?;
        } else if file_type.is_file() {
            let path = fs::canonicalize(entry.path())?;
            if !path.starts_with(root) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("scanned file escaped bucket directory: {}", path.display()),
                ));
            }

            let Some(kind) = classify_file(&path) else {
                continue;
            };
            let metadata = fs::metadata(&path)?;
            files.push(StorageFile {
                path,
                kind,
                size_bytes: metadata.len(),
            });
        }
    }
    Ok(())
}

fn classify_file(path: &Path) -> Option<StorageFileKind> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("xdr") {
        return None;
    }

    let name = path.file_stem()?.to_str()?;
    if let Some(sequence) = name.strip_prefix("ledger-") {
        if !sequence.is_empty() && sequence.bytes().all(|byte| byte.is_ascii_digit()) {
            return sequence
                .parse()
                .ok()
                .map(|ledger| StorageFileKind::LedgerXdr { ledger });
        }
    }

    Some(StorageFileKind::Bucket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use tempfile::tempdir;

    #[tokio::test]
    async fn scans_nested_xdr_files_and_ignores_non_xdr_files() {
        let temp = tempdir().unwrap();
        let buckets = temp.path().join("buckets");
        fs::create_dir_all(buckets.join("nested")).unwrap();
        fs::write(buckets.join("ledger-123.xdr"), b"ledger").unwrap();
        fs::write(buckets.join("nested").join("bucket-hash.xdr"), b"bucket").unwrap();
        fs::write(buckets.join("nested").join("notes.txt"), b"ignore").unwrap();

        let files = scan_bucket_dir(&buckets).await.unwrap();

        assert_eq!(files.len(), 2);
        assert!(files
            .iter()
            .any(|file| file.kind == (StorageFileKind::LedgerXdr { ledger: 123 })));
        assert!(files
            .iter()
            .any(|file| file.kind == StorageFileKind::Bucket));
    }

    #[tokio::test]
    async fn active_reader_lock_prevents_removal() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("bucket.xdr");
        fs::write(&path, b"bucket").unwrap();
        let _reader = FileReadGuard::open(&path).unwrap();

        assert!(!remove_if_unlocked(&path).await.unwrap());
        assert!(path.exists());
    }

    #[tokio::test]
    async fn removes_unlocked_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("bucket.xdr");
        fs::write(&path, b"bucket").unwrap();

        assert!(remove_if_unlocked(&path).await.unwrap());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn rejects_non_directory_bucket_path() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("not-a-directory");
        fs::write(&file, b"data").unwrap();

        let error = scan_bucket_dir(&file).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
}
