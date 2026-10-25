//! iCloud Drive, with Optimize Mac Storage on, keeps some files only in the cloud: their names, sizes, times and
//! extended attributes are on the Mac, their data isn't (SF_DATALESS), and reading it downloads it. sift-it never
//! downloads one: a file only in the cloud is found by its name, or by what was read of it before macOS removed its
//! download, and it's read once it's downloaded again.

use std::path::Path;

/// Whether a file is only in the cloud: reading its data would download it.
pub fn cloud_only(metadata: &std::fs::Metadata) -> bool {
    use std::os::darwin::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    metadata.st_flags() & SF_DATALESS != 0
}

/// Whether the file at `path` is only in the cloud (false when it can't be looked at).
pub fn cloud_only_at(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| cloud_only(&metadata))
}

unsafe extern "C" {
    fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
}

/// Keeps this process, and the processes it starts (textutil), from downloading a file that's only in the cloud:
/// reading or mapping one fails with EDEADLK instead, in every framework. Its name, size, times and extended
/// attributes can still be read.
pub fn never_download() {
    // <sys/resource.h>
    const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: i32 = 3;
    const IOPOL_SCOPE_PROCESS: i32 = 0;
    const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: i32 = 1;
    let set = unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_PROCESS,
            IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
        )
    };
    if set != 0 {
        eprintln!("sift-it: can't keep files only in iCloud from downloading: {}", std::io::Error::last_os_error());
    }
}
