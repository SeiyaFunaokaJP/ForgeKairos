use std::{
    fs::File,
    io::{self, Read as _},
    path::Path,
};

/// Reads up to `limit` bytes of a file to index it.
///
/// A full update reads every C and C++ file of the worktree once, and on Linux those reads
/// would stay in the page cache until memory runs short. On WSL that is memory the VM keeps
/// from Windows. So a file whose first page was not cached before the read is dropped from the
/// cache again; one that was (an open file, or one a build just read) is left as it was.
pub fn read_file_for_index(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let was_cached = holds_start_of(&file);
    // `Take` hides the file's length from `read_to_end`, which would then grow the buffer by
    // doubling from a few bytes.
    let expected_length = file
        .metadata()
        .map_or(0, |metadata| metadata.len().min(limit));
    let mut contents = Vec::with_capacity(usize::try_from(expected_length).unwrap_or(0));
    (&file).take(limit).read_to_end(&mut contents)?;
    if !was_cached {
        drop_cached_pages(&file);
    }
    Ok(contents)
}

/// Whether the page cache holds the first page of `file`. When that cannot be told, the file
/// counts as cached so that nothing someone else may be using is dropped.
#[cfg(target_os = "linux")]
pub(crate) fn holds_start_of(file: &File) -> bool {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `sysconf` has no preconditions.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let Ok(page_size) = usize::try_from(page_size) else {
        return true;
    };
    // SAFETY: a new read-only mapping of one page of an open file. Nothing reads through it:
    // it is only handed to `mincore` and unmapped below.
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page_size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if address == libc::MAP_FAILED {
        log::debug!(
            "symbol_index: mmap to ask mincore: {}",
            io::Error::last_os_error()
        );
        return true;
    }
    let mut residency: libc::c_uchar = 0;
    // SAFETY: `address` maps exactly one page, so `mincore` writes one byte.
    let queried = unsafe { libc::mincore(address, page_size, &mut residency) } == 0;
    // SAFETY: unmaps the mapping made above, which nothing else refers to.
    if unsafe { libc::munmap(address, page_size) } != 0 {
        log::debug!(
            "symbol_index: munmap after mincore: {}",
            io::Error::last_os_error()
        );
    }
    !queried || residency & 1 != 0
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn holds_start_of(_file: &File) -> bool {
    true
}

#[cfg(target_os = "linux")]
pub(crate) fn drop_cached_pages(file: &File) {
    use std::os::fd::AsRawFd as _;

    // SAFETY: advice about an open descriptor; it does not change the file.
    let error = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if error != 0 {
        log::debug!(
            "symbol_index: posix_fadvise(DONTNEED): {}",
            io::Error::from_raw_os_error(error)
        );
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn drop_cached_pages(_file: &File) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A file whose contents are only on disk, or `None` where cached pages cannot be dropped
    /// (tmpfs keeps them).
    fn uncached_file(path: &Path, contents: &str) -> Option<File> {
        use std::io::Write as _;
        let mut writer = File::create(path).expect("create");
        writer.write_all(contents.as_bytes()).expect("write");
        // Dirty pages are only written back, not dropped, by `POSIX_FADV_DONTNEED`.
        writer.sync_all().expect("sync");
        // A write-only descriptor cannot be mapped to ask `mincore`.
        let file = File::open(path).expect("open");
        drop_cached_pages(&file);
        if holds_start_of(&file) {
            eprintln!("skipped: the file system of {path:?} keeps its pages cached");
            return None;
        }
        Some(file)
    }

    #[test]
    fn a_file_read_from_disk_is_not_left_in_the_page_cache() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("cold.c");
        let Some(file) = uncached_file(&path, &"int cold(void) { return 0; }\n".repeat(4096))
        else {
            return;
        };

        let contents = read_file_for_index(&path, u64::MAX).expect("read");

        assert_eq!(contents.len(), 29 * 4096);
        assert!(!holds_start_of(&file));
    }

    #[test]
    fn a_file_already_in_the_page_cache_stays_there() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("warm.c");
        let Some(file) = uncached_file(&path, &"int warm(void) { return 0; }\n".repeat(4096))
        else {
            return;
        };
        std::fs::read(&path).expect("warm the cache");
        assert!(holds_start_of(&file));

        read_file_for_index(&path, 16).expect("read");

        assert!(holds_start_of(&file));
    }

    #[test]
    fn the_limit_caps_what_is_read() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("large.c");
        std::fs::write(&path, vec![b'x'; 10_000]).expect("write");

        assert_eq!(read_file_for_index(&path, 100).expect("read").len(), 100);
    }
}
