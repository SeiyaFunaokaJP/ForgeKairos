//! The [`CanonicalPathBuf`] primitive: a symlink-free canonical path that, on
//! Linux, is backed by a **live `O_PATH` handle pinning the exact inode** it
//! names — not merely a path string.

use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};

/// A canonical, symlink-free filesystem path.
///
/// On **Linux** this is not merely a path string: it also owns a live `O_PATH`
/// file descriptor pinning the exact inode the path named at construction time.
/// The descriptor — not the text — is the security-relevant identity, so
/// enforcement can prove the object living at the path *now* is still the one
/// that was captured, closing the classic time-of-check-to-time-of-use hole
/// where a verified path is swapped for a symlink before it is actually used.
/// On **Windows** it is a plain path string, but constrained by construction to
/// one of the three shapes a sandboxed WSL command can name: a Windows drive
/// path (`C:\...`, on NTFS), a Linux-absolute path (`/...`, inside the WSL
/// distro), or a folder in a distribution's own filesystem named through its
/// share (`\\wsl.localhost\<distro>\...`, ADR-F49 第 2b 段 — the only Windows
/// spelling an ext4 worktree has, normalized to that one form so two spellings
/// of it compare equal). Other UNC paths and relative paths are rejected — a
/// sandboxed project is always local (a WSL *remote* project is unsandboxed
/// here), so a grant is either the project's own NTFS files or a WSL-native
/// location.
/// On other platforms it is a plain canonical path (see the constructors for the
/// per-platform rationale).
///
/// Invariant: [`path`](Self::path) is an **absolute**, symlink-free realpath; on
/// Linux this is *proven* at construction (`resolve` reads it back from
/// `/proc/self/fd`, `from_canonical` re-verifies a claimed value against the
/// reopened fd). Both constructors reject a non-absolute input on the platforms
/// that resolve paths themselves (Linux/macOS): a relative path would otherwise
/// be resolved against Zed's *own* process working directory, silently pinning
/// an unpredictable location.
#[derive(Clone)]
pub(crate) struct CanonicalPathBuf {
    path: PathBuf,
    /// An `O_PATH` descriptor pinning the inode of the canonical target. Wrapped
    /// in an `Arc` only so the surrounding policy types can stay `Clone`;
    /// cloning shares the same underlying descriptor.
    #[cfg(target_os = "linux")]
    fd: std::sync::Arc<OwnedFd>,
}

impl CanonicalPathBuf {
    /// Resolve `path` to its canonical target, **following** symlinks to
    /// discover the real object it names.
    ///
    /// Following symlinks is intentional: the goal is to find the true target so
    /// it can be shown, persisted, and pinned. Protection against a *later* swap
    /// comes from persisting the resolved path and rebuilding with
    /// [`Self::from_canonical`], which verifies it.
    pub(crate) fn resolve(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        // A relative path would be resolved against Zed's own process working
        // directory by the `open`/`canonicalize` below, silently pinning an
        // unpredictable location. Every grant flows through this constructor, so
        // rejecting here fails closed for all of them at once.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        require_absolute(path)?;

        // Windows captures no canonical identity of its own here — the real
        // resolution happens WSL-side — so the checked, normalized grant path
        // *is* the value.
        #[cfg(target_os = "windows")]
        {
            return Ok(Self {
                path: require_windows_grant_shape(path)?,
            });
        }
        #[cfg(target_os = "macos")]
        {
            // `canonicalize_allowing_missing_leaf` resolves through the existing
            // parent so a not-yet-created leaf still yields the real path
            // Seatbelt will match against.
            Ok(Self {
                path: super::canonicalize_allowing_missing_leaf(path),
            })
        }
        #[cfg(target_os = "linux")]
        {
            // `O_PATH` opens a handle that refers to the inode without granting
            // read/write on its contents, which is exactly what a bind source
            // needs. Symlinks are followed (no `O_NOFOLLOW`) so the fd pins the
            // real target; its canonical path is then read back from
            // `/proc/self/fd`.
            let fd = open_o_path(path, FollowSymlinks::Yes)?;
            let path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
            Ok(Self {
                path,
                fd: std::sync::Arc::new(fd),
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Ok(Self {
                path: path.to_path_buf(),
            })
        }
    }

    /// Verify a *claimed* canonical `path`, proving the object now living there
    /// is the one that was approved.
    ///
    /// This is the load-bearing reconstruction for user-approved write grants,
    /// which must survive process restarts and therefore can't keep the
    /// approval-time fd alive.
    ///
    /// On **Linux** this opens an `O_PATH` fd with `O_NOFOLLOW`, rejects a
    /// symlink leaf (`S_IFLNK` check, [`io::ErrorKind::PermissionDenied`]), and
    /// requires the fd's real path to still equal `path` (also
    /// `PermissionDenied` otherwise) — so any component swapped for a symlink
    /// after approval fails closed. On **macOS/other** the claimed path is
    /// trusted verbatim: Seatbelt re-resolves the access path at syscall time
    /// (so a later swap is denied, not redirected), and WSL/other resolve
    /// elsewhere.
    pub(crate) fn from_canonical(path: PathBuf) -> io::Result<Self> {
        // Same absolute-path requirement as `resolve`: a persisted or
        // hand-authored relative grant must never be resolved against Zed's own
        // working directory (Linux) or emitted as a relative Seatbelt literal
        // (macOS).
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        require_absolute(&path)?;

        // As in `resolve`: on Windows the checked, normalized grant path is all
        // there is to capture.
        #[cfg(target_os = "windows")]
        {
            return Ok(Self {
                path: require_windows_grant_shape(&path)?,
            });
        }
        #[cfg(target_os = "macos")]
        {
            Ok(Self { path })
        }
        #[cfg(target_os = "linux")]
        {
            // `O_NOFOLLOW` makes a symlink *leaf* open the symlink itself
            // (harmless with `O_PATH`) rather than its target, so we can detect
            // and reject it below; intermediate components are still traversed
            // and caught by the canonical-path comparison.
            let fd = open_o_path(&path, FollowSymlinks::No)?;

            // Reject a symlink leaf outright: a grant must name a real directory,
            // and `readlink` of an `O_PATH|O_NOFOLLOW` fd on a symlink returns
            // the symlink's *own* path (equal to `path`), so the comparison
            // below wouldn't catch it.
            let stat = nix::sys::stat::fstat(fd.as_raw_fd()).map_err(io::Error::from)?;
            if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "sandbox write grant {} is a symlink, not a directory",
                        path.display()
                    ),
                ));
            }

            // Load-bearing: the pinned inode's real path must still be exactly
            // the approved canonical path. If any component became a symlink
            // after approval, the fd resolves elsewhere and this diverges.
            let current = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
            if current != path {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "sandbox write grant {} was redirected to {}",
                        path.display(),
                        current.display()
                    ),
                ));
            }

            Ok(Self {
                path,
                fd: std::sync::Arc::new(fd),
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Ok(Self { path })
        }
    }

    /// The canonical, symlink-free path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Consume this value, returning the canonical path.
    #[cfg_attr(target_os = "windows", expect(dead_code))]
    pub(crate) fn into_path(self) -> PathBuf {
        self.path
    }

    /// Linux: a borrowed handle to the pinned inode, for `fstat`-based identity
    /// checks.
    #[cfg(target_os = "linux")]
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Linux: an independent `O_PATH` descriptor to the same pinned inode,
    /// duplicated (with `O_CLOEXEC`) so a validation server can own and send it
    /// over `SCM_RIGHTS` without affecting this value's descriptor.
    #[cfg(target_os = "linux")]
    pub(crate) fn dup_fd(&self) -> io::Result<OwnedFd> {
        self.fd.as_fd().try_clone_to_owned()
    }
}

/// Whether [`open_o_path`] traverses a symlink *leaf* or pins the link itself.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FollowSymlinks {
    /// Pin the symlink's target (used when discovering the real object).
    Yes,
    /// Add `O_NOFOLLOW`, so a symlink leaf pins the link itself and can be
    /// detected and rejected.
    No,
}

/// Open an `O_PATH | O_CLOEXEC` handle pinning the inode `path` names, adding
/// `O_NOFOLLOW` when the leaf must not be traversed.
///
/// Built from a raw `libc::open` **on purpose**. Rust's
/// `OpenOptions::custom_flags` masks the caller's flags with `!O_ACCMODE`, and
/// musl defines `O_ACCMODE` as `0o10000003` — which *includes* `O_PATH`
/// (`0o10000000`). On the musl-static remote server binary the flag was
/// therefore silently dropped and every "pin" became an ordinary read-open:
/// it needed read permission (a `chmod 0300` directory could not be pinned at
/// all), it blocked forever on a FIFO with no writer, and with `O_NOFOLLOW` it
/// failed on a symlink leaf with `ELOOP` instead of pinning the link for the
/// explicit `S_IFLNK` rejection below. glibc's `O_ACCMODE` is `0o3`, so no gnu
/// build — and therefore no unit test — ever saw any of it. Never route this
/// open back through `OpenOptions`.
#[cfg(target_os = "linux")]
pub(crate) fn open_o_path(path: &Path, follow: FollowSymlinks) -> io::Result<OwnedFd> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} contains a NUL byte", path.display()),
        )
    })?;
    let mut flags = libc::O_PATH | libc::O_CLOEXEC;
    if follow == FollowSymlinks::No {
        flags |= libc::O_NOFOLLOW;
    }
    loop {
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
        // call, and `O_PATH` opens take no `mode` argument.
        let raw = unsafe { libc::open(c_path.as_ptr(), flags) };
        if raw >= 0 {
            // SAFETY: freshly opened by us and not stored anywhere else, so this
            // is its sole owner.
            return Ok(unsafe { OwnedFd::from_raw_fd(raw) });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Reject a non-absolute grant path. Gated to the platforms whose constructors
/// resolve paths against the process working directory; on WSL/other the path is
/// a namespace-specific form that `Path::is_absolute` would misjudge, and its
/// real resolution happens WSL-side (see `crate::windows_wsl`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn require_absolute(path: &Path) -> io::Result<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("sandbox grant path {} is not absolute", path.display()),
        ))
    }
}

/// Windows: enforce that a stored grant path is one of the shapes a sandboxed
/// WSL command can name, returning it in the one spelling this type stores and
/// compares:
///
/// * a Windows drive path (`C:\...` or `\\?\C:\...`, on NTFS), kept as written;
/// * a Linux-absolute path (`/...`, inside the distro the command runs in), kept
///   as written; or
/// * (ADR-F49 第 2b 段) a folder in a distribution's own filesystem named
///   through its share, `\\wsl.localhost\<distro>\<rest>` — the only Windows
///   spelling a worktree on ext4 has. A location there can be written several
///   ways (`\\wsl$\`, `/` separators, the `\\?\UNC\` prefix
///   `std::fs::canonicalize` produces), so it is normalized to the single form
///   `util::path_mapping::ComparablePath` compares by. That matters because the
///   plain `PartialEq` and `Path::starts_with` this type's callers use to dedupe
///   grants and to decide whether one lies inside another would otherwise read
///   two spellings of one folder as two unrelated places.
///
/// Everything else is rejected, so an invalid grant shape can't be represented
/// as a [`CanonicalPathBuf`]: other UNC paths (no WSL sandbox can reach a
/// network share), relative paths, a bare distribution share
/// (`\\wsl.localhost\<distro>`, which would grant that distro's whole root), and
/// a share path containing `..` (it names a location other than the one shown).
/// `Path::is_absolute` isn't used: it would reject a perfectly valid
/// Linux-absolute grant like `/home/me` on Windows.
///
/// The distribution name is part of that normalization: WSL matches it
/// case-insensitively, so two casings of one name are one place and are folded
/// into one spelling. The name the user wrote is kept verbatim beside the
/// canonical, as [`super::HostFilesystemLocation`]'s untrusted raw path, and
/// that is the one the approval UI and the sandbox tooltip show.
#[cfg(target_os = "windows")]
fn require_windows_grant_shape(path: &Path) -> io::Result<PathBuf> {
    let text = path.to_string_lossy();
    // Linux-absolute (WSL): exactly one leading '/'.
    if text.starts_with('/') && !text.starts_with("//") {
        return Ok(path.to_path_buf());
    }
    // Windows drive (NTFS): `X:...`, optionally behind the `\\?\` verbatim prefix.
    let drive = text.strip_prefix(r"\\?\").unwrap_or(&text).as_bytes();
    if drive.len() >= 2 && drive[0].is_ascii_alphabetic() && drive[1] == b':' {
        return Ok(path.to_path_buf());
    }
    if let Some(share) = super::parse_wsl_share_path(&text)
        && share.linux_path != "/"
    {
        return Ok(PathBuf::from(super::wsl_share_path_spelling(
            &share.distro,
            &share.linux_path,
        )));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "sandbox grant path {} is none of a Windows drive path (`C:\\...`), \
             a WSL absolute path (`/...`) and a folder in a WSL distribution \
             (`\\\\wsl.localhost\\<distro>\\...`)",
            path.display()
        ),
    ))
}

impl PartialEq for CanonicalPathBuf {
    /// Two values are equal when they refer to the **same filesystem object**:
    /// the inode behind the `O_PATH` fd on Linux, the canonical path on
    /// macOS/other — never merely equal path text where an fd is available. This
    /// is what lets policy bookkeeping dedupe "the same location named two
    /// different ways" and refuse to treat "two different objects that happen to
    /// share a path string" as one.
    fn eq(&self, other: &Self) -> bool {
        #[cfg(target_os = "linux")]
        {
            match (
                linux_fd_identity(self.fd.as_raw_fd()),
                linux_fd_identity(other.fd.as_raw_fd()),
            ) {
                (Some(a), Some(b)) => a == b,
                // An `fstat` on an `O_PATH` fd we own should never fail; if it
                // somehow does we can't prove identity, so report "not equal"
                // (the safe answer) and leave a trace.
                _ => {
                    log::error!(
                        "failed to fstat an O_PATH descriptor while comparing sandbox locations"
                    );
                    false
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Canonicalization is a bijection on real paths, so equal canonical
            // paths mean the same directory/file.
            self.path == other.path
        }
    }
}

impl Eq for CanonicalPathBuf {}

/// The `(device, inode)` pair behind an `O_PATH` descriptor, used to decide
/// whether two [`CanonicalPathBuf`]s (or their parents) refer to the same
/// filesystem object.
#[cfg(target_os = "linux")]
pub(crate) fn linux_fd_identity(fd: std::os::fd::RawFd) -> Option<(u64, u64)> {
    let stat = nix::sys::stat::fstat(fd).ok()?;
    Some((stat.st_dev as u64, stat.st_ino as u64))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::CanonicalPathBuf;
    use std::path::PathBuf;

    #[test]
    fn constructors_reject_relative_paths() {
        // A relative grant would otherwise be resolved against Zed's own working
        // directory; both constructors must fail closed before touching the fs.
        // (`CanonicalPathBuf` is deliberately not `Debug`, so match rather than
        // `expect_err`.)
        match CanonicalPathBuf::resolve("relative/dir") {
            Ok(_) => panic!("resolve must reject a relative path"),
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput),
        }
        match CanonicalPathBuf::from_canonical(PathBuf::from("relative/dir")) {
            Ok(_) => panic!("from_canonical must reject a relative path"),
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::{CanonicalPathBuf, FollowSymlinks, open_o_path};
    use std::os::fd::AsRawFd as _;

    /// `O_PATH` as the kernel reports it in `/proc/self/fdinfo/<fd>` (octal
    /// `flags:`), i.e. `0o10000000` == `0x200000`.
    const O_PATH_FLAG: u64 = 0o10000000;

    /// Read the open flags the *kernel* recorded for `fd`, which is the only
    /// way to catch a libc that quietly dropped one on the way in.
    fn kernel_open_flags(fd: std::os::fd::RawFd) -> u64 {
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
            .expect("reading /proc/self/fdinfo");
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .expect("fdinfo has a flags: line");
        u64::from_str_radix(flags.trim(), 8).expect("fdinfo flags are octal")
    }

    /// Regression (musl-only in practice, but asserted on every libc): musl's
    /// `O_ACCMODE` includes `O_PATH`, so building these handles through
    /// `OpenOptions::custom_flags` — which std masks with `!O_ACCMODE` — turned
    /// the inode pin into a real read-open on the shipped musl binary. Assert
    /// the flag reached the kernel rather than trusting the request.
    #[test]
    fn pinned_handles_really_carry_o_path() {
        let dir = tempfile::tempdir().expect("tempdir");

        let resolved = CanonicalPathBuf::resolve(dir.path()).expect("resolve");
        assert_ne!(
            kernel_open_flags(resolved.fd().as_raw_fd()) & O_PATH_FLAG,
            0,
            "resolve() must pin with O_PATH"
        );

        let reopened = CanonicalPathBuf::from_canonical(resolved.path().to_path_buf())
            .expect("from_canonical");
        let flags = kernel_open_flags(reopened.fd().as_raw_fd());
        assert_ne!(
            flags & O_PATH_FLAG,
            0,
            "from_canonical() must pin with O_PATH"
        );
        assert_ne!(
            flags & libc::O_NOFOLLOW as u64,
            0,
            "from_canonical() relies on O_NOFOLLOW to detect a symlink leaf"
        );
    }

    /// `O_PATH` needs no read permission, so a directory the owner cannot read
    /// still pins. Without it a `chmod 0300 .git` could not be pinned at all,
    /// and the caller that dropped the failure then handed out a recursive
    /// write grant over the whole worktree.
    #[test]
    fn an_unreadable_directory_still_pins() {
        use std::os::unix::fs::PermissionsExt as _;
        let parent = tempfile::tempdir().expect("tempdir");
        let dir = parent.path().join("locked");
        std::fs::create_dir(&dir).expect("create dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300)).expect("chmod 0300");
        let pinned = CanonicalPathBuf::resolve(&dir);
        // Restore the mode before asserting, so the temp dir can clean itself
        // up (its recursive delete has to read this directory) either way.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore mode");
        pinned.expect("an unreadable directory must still pin");
    }

    /// A FIFO with no writer blocks an ordinary `open` forever. `O_PATH` never
    /// waits for a partner — the helper enumerates a worktree root's children,
    /// so one FIFO used to hang the whole sandbox setup with no error and no
    /// exit.
    #[test]
    fn pinning_a_fifo_without_a_writer_returns_promptly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("pipe");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600))
            .expect("mkfifo");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let pinned = CanonicalPathBuf::resolve(&fifo).is_ok()
                && open_o_path(&fifo, FollowSymlinks::No).is_ok();
            // If this races a `recv_timeout` timeout, `rx` is already dropped and
            // the test body has already failed via its own `expect` below.
            tx.send(pinned)
                .expect("receiver dropped before the FIFO probe finished");
        });
        let pinned = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("pinning a FIFO must not block on a writer");
        assert!(pinned, "a FIFO must pin like any other inode");
    }

    /// The symlink-leaf rejection in `from_canonical` depends on `O_PATH`
    /// *and* `O_NOFOLLOW` both surviving: without `O_PATH`, `O_NOFOLLOW` alone
    /// makes the open fail with `ELOOP`, which reports the wrong error kind
    /// (and, for a non-symlink swap, the wrong outcome entirely).
    #[test]
    fn from_canonical_rejects_a_symlink_leaf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("real");
        std::fs::create_dir(&target).expect("create dir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        match CanonicalPathBuf::from_canonical(link) {
            Ok(_) => panic!("a symlink leaf must not be accepted as a grant"),
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod windows_tests {
    use super::CanonicalPathBuf;
    use std::path::{Path, PathBuf};

    #[test]
    fn constructors_enforce_windows_grant_shape() {
        // Accepted, and kept as written: Windows drive paths (NTFS) and WSL
        // absolute paths.
        for ok in [
            r"C:\Users\me",
            r"\\?\C:\Users\me",
            "/home/me/proj",
            "/mnt/c/Users/me",
        ] {
            let canonical = CanonicalPathBuf::from_canonical(PathBuf::from(ok))
                .unwrap_or_else(|_| panic!("{ok} should be accepted"));
            assert_eq!(canonical.path(), Path::new(ok));
        }
        // Rejected: non-WSL UNC and relative paths.
        for bad in [
            r"\\server\share\dir",
            r"relative\dir",
            "also/relative",
            r"\\?\UNC\server\share\dir",
        ] {
            match CanonicalPathBuf::from_canonical(PathBuf::from(bad)) {
                Ok(_) => panic!("{bad} should be rejected"),
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput),
            }
        }
    }

    /// ADR-F49 第 2b 段: a worktree on the distribution's own ext4 reaches the
    /// sandbox as a `\\wsl.localhost\<distro>\…` path, and every spelling of one
    /// location has to end up as the same stored path — the dedupe and
    /// containment checks over grants compare these as plain paths.
    #[test]
    fn a_wsl_share_grant_is_accepted_in_one_spelling() {
        let expected = Path::new(r"\\wsl.localhost\ubuntu\root\p");
        for spelling in [
            r"\\wsl.localhost\Ubuntu\root\p",
            r"\\wsl$\Ubuntu\root\p",
            r"\\WSL.LOCALHOST\Ubuntu\root\p",
            r"\\?\UNC\wsl.localhost\Ubuntu\root\p",
            r"\\?\UNC\wsl$\Ubuntu\root\p",
            r"//wsl.localhost/Ubuntu/root/p",
            "\\\\wsl.localhost\\Ubuntu\\root\\p\\",
            r"\\wsl.localhost\Ubuntu\root\.\p",
        ] {
            let canonical = CanonicalPathBuf::from_canonical(PathBuf::from(spelling))
                .unwrap_or_else(|_| panic!("{spelling} should be accepted"));
            assert_eq!(canonical.path(), expected, "from {spelling}");
        }
        // WSL matches the distribution name case-insensitively, so a differently
        // cased name is the same folder and has to normalize to the same stored
        // path — otherwise the same grant, named twice, is two entries.
        for spelling in [
            r"\\wsl.localhost\UBUNTU\root\p",
            r"\\wsl.localhost\ubuntu\root\p",
            r"\\wsl$\uBuNtU\root\p",
        ] {
            let canonical = CanonicalPathBuf::from_canonical(PathBuf::from(spelling))
                .expect("a differently-cased distro name is still a grant");
            assert_eq!(
                canonical.path(),
                Path::new(r"\\wsl.localhost\ubuntu\root\p"),
                "from {spelling}"
            );
        }
    }

    #[test]
    fn a_wsl_share_grant_without_a_folder_in_it_is_rejected() {
        for bad in [
            // A whole distribution, not a folder in one.
            r"\\wsl.localhost\Ubuntu",
            r"\\wsl.localhost\Ubuntu\",
            r"\\wsl$\Ubuntu",
            r"\\wsl.localhost",
            // No distribution name at all.
            r"\\wsl.localhost\\root",
            // `..` names something other than what was shown.
            r"\\wsl.localhost\Ubuntu\root\..\etc",
            r"\\wsl.localhost\Ubuntu\..",
        ] {
            match CanonicalPathBuf::from_canonical(PathBuf::from(bad)) {
                Ok(_) => panic!("{bad} should be rejected"),
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput),
            }
        }
    }
}
