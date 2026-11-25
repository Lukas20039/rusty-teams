//! Editing a library file in a local app: where the working copy lives and when a save is ready to upload.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::downloads::safe_file_name;

/// What tells two versions of the working copy apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub modified: SystemTime,
    pub size: u64,
}

pub fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: metadata.modified().ok()?,
        size: metadata.len(),
    })
}

/// A save is uploaded once the file differs from the uploaded version and looked the same at the
/// previous check, so a file still being written is not sent half done.
pub fn ready_to_upload(uploaded: Stamp, previous: Option<Stamp>, now: Stamp) -> bool {
    now != uploaded && previous == Some(now)
}

/// One folder per drive item keeps two files of the same name apart.
pub fn working_copy(root: &Path, item_id: &str, name: &str) -> PathBuf {
    root.join(safe_file_name(item_id))
        .join(safe_file_name(name))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use super::{Stamp, ready_to_upload, working_copy};

    fn at(seconds: u64, size: u64) -> Stamp {
        Stamp {
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
            size,
        }
    }

    #[test]
    fn a_save_uploads_once_it_has_settled() {
        let uploaded = at(1, 10);
        assert!(!ready_to_upload(uploaded, Some(uploaded), uploaded));
        assert!(!ready_to_upload(uploaded, Some(uploaded), at(2, 12)));
        assert!(ready_to_upload(uploaded, Some(at(2, 12)), at(2, 12)));
        assert!(!ready_to_upload(uploaded, Some(at(2, 12)), at(3, 14)));
        assert!(!ready_to_upload(uploaded, None, at(2, 12)));
    }

    #[test]
    fn working_copies_sit_in_a_folder_per_item() {
        let path = working_copy(Path::new("/edit"), "01ABC", "Plan.docx");
        assert_eq!(path, Path::new("/edit/01ABC/Plan.docx"));
    }
}
