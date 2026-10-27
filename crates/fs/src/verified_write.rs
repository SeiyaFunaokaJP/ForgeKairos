//! ADR-F49: writing a file an agent asked for exactly where it was checked to
//! land, or not at all.
//!
//! An agent's sandboxed commands can create links inside the project at any
//! moment, so a path that was checked and then opened by name can lead
//! somewhere else by the time it is written (`sub` swapped for a link to
//! `.git`). Here every folder on the way is opened without following links,
//! the file is opened the same way, its real path is read back from the open
//! handle, and only then is anything written — through that same handle.
//!
//! The write also refuses by itself what no agent write may reach, whoever
//! asks for it (Git metadata, the account's protected files), and this module
//! holds the name rules the client's judgement of an agent's write uses, so
//! both sides fold names the same way.

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};
use unicode_normalization::UnicodeNormalization as _;

pub const GIT_DIRECTORY_NAME: &str = ".git";

// The two lists below together are a copy of `sandbox::UNIX_PROTECTED_HOME_RELATIVE`:
// a file Zed writes for an agent must not reach what a sandboxed command can't.
// They are copied so that `fs` doesn't depend on the sandbox, and a test in
// `acp_thread` keeps them the same.
/// Home-relative entries no agent write may reach on a Unix host: credentials,
/// configuration that makes programs run (Git's included), shell and login
/// startup files, directories already on `PATH`, and this fork's remote
/// install.
pub const UNIX_PROTECTED_HOME_RELATIVE: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".config/gh",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    // ADR-F49 decision 8 (2026-09-15): the rest of what a sandboxed command
    // may not even read. Rewriting one of these aims the user's own next
    // `npm install`, `cargo publish` or `gcloud` wherever the writer chose.
    ".npmrc",
    ".yarnrc.yml",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/gcloud",
    ".terraform.d",
    ".local/share/keyrings",
    ".password-store",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".gitconfig",
    ".config/git",
    ".claude.json",
    ".zed_server",
    branding::concat_const!(
        ".local/share/",
        branding::FORGEKAIROS.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/share/",
        branding::ZED_JP_ENHANCE.app_directory_name_lowercase
    ),
    ".cargo/bin",
    ".local/bin",
    "go/bin",
    ".deno/bin",
    ".bun/bin",
    ".bun/install/global",
    ".bunfig.toml",
    ".volta",
    ".nvm",
    ".bashrc",
    ".bash_aliases",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".zlogout",
    ".cshrc",
    ".tcshrc",
    ".login",
    ".config/fish",
    ".bash_logout",
    ".pam_environment",
];

/// The user's own configuration of the agents that run on the host.
pub const AGENT_CONFIGURATION_HOME_RELATIVE: &[&str] = &[".claude", ".gemini", ".codex"];

/// Why [`crate::Fs::write_verified`] wrote nothing, or
/// [`crate::Fs::verify_read`] refused a path.
#[derive(Debug, thiserror::Error)]
pub enum VerifiedWriteError {
    #[error("`{}` is not a plain path inside the folder", .0.display())]
    NotBeneath(PathBuf),
    #[error("`{}` is Git metadata, which agents may not change", .0.display())]
    GitMetadata(PathBuf),
    #[error(
        "`{}` holds credentials or configuration that makes programs run, which agents may \
         not change",
        .0.display()
    )]
    Protected(PathBuf),
    #[error("`{}` is a symbolic link, junction or other redirection", .0.display())]
    Redirected(PathBuf),
    #[error("`{}` is not a regular file", .0.display())]
    NotAFile(PathBuf),
    #[error("`{}` is not a folder", .0.display())]
    NotAFolder(PathBuf),
    #[error(
        "`{}` has other hard links, so writing it would change other files too",
        .0.display()
    )]
    HardLinked(PathBuf),
    #[error("`{}` turned out to be `{}`", .expected.display(), .actual.display())]
    Elsewhere { expected: PathBuf, actual: PathBuf },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// How names are compared when judging where an agent's write lands. Every
/// comparison drops characters that render as nothing, folds compatibility
/// forms (NFKC, so the Kelvin sign is a `K`) and ignores case, since a
/// case-insensitive filesystem may match any of those spellings; comparing
/// that way only ever refuses more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameRules {
    /// The files are on Windows or on a Windows drive seen from WSL, where
    /// `name:stream` is a stream of `name` and trailing dots and spaces are
    /// dropped. Elsewhere those spell other names.
    pub windows: bool,
}

impl NameRules {
    /// The rules for files at `path`: Windows ones for a Windows path, and
    /// for a Windows drive mounted in WSL (`/mnt/<letter>/…`).
    pub fn for_path(path: &Path) -> Self {
        let mut components = path.components();
        let windows = match (components.next(), components.next(), components.next()) {
            (Some(Component::Prefix(_)), _, _) => true,
            (
                Some(Component::RootDir),
                Some(Component::Normal(mount)),
                Some(Component::Normal(drive)),
            ) => {
                mount == "mnt"
                    && drive.to_str().is_some_and(|drive| {
                        drive.len() == 1 && drive.chars().all(|letter| letter.is_ascii_alphabetic())
                    })
            }
            _ => false,
        };
        Self { windows }
    }

    pub fn comparable_name(self, name: &str) -> String {
        let visible = name
            .chars()
            .filter(|character| !is_default_ignorable(*character))
            .nfkc()
            .collect::<String>();
        let name = if self.windows {
            let stem = visible
                .split_once(':')
                .map_or(visible.as_str(), |(stem, _)| stem);
            stem.trim_end_matches(['.', ' '])
        } else {
            visible.as_str()
        };
        name.to_uppercase().to_lowercase()
    }

    fn component_key(self, component: Component<'_>) -> String {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                    format!("{}:", char::from(drive).to_ascii_uppercase())
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
                    r"\\{}\{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                )
                .to_uppercase(),
                Prefix::Verbatim(name) | Prefix::DeviceNS(name) => {
                    name.to_string_lossy().to_uppercase()
                }
            },
            Component::RootDir => "/".to_string(),
            Component::CurDir => ".".to_string(),
            Component::ParentDir => "..".to_string(),
            Component::Normal(name) => self.comparable_name(&name.to_string_lossy()),
        }
    }

    pub fn normal_component_keys(self, path: &Path) -> impl Iterator<Item = String> + '_ {
        path.components()
            .filter_map(move |component| match component {
                Component::Normal(name) => Some(self.comparable_name(&name.to_string_lossy())),
                _ => None,
            })
    }

    /// `path` without the leading `ancestor`, or `None` when `path` is not
    /// inside `ancestor`.
    pub fn relative_to(self, path: &Path, ancestor: &Path) -> Option<PathBuf> {
        let ancestor = ancestor
            .components()
            .map(|component| self.component_key(component))
            .collect::<Vec<_>>();
        if ancestor.is_empty() {
            return None;
        }
        let mut components = path.components();
        for ancestor_component in &ancestor {
            if self.component_key(components.next()?) != *ancestor_component {
                return None;
            }
        }
        Some(components.collect())
    }

    /// Whether `path` is `ancestor` or inside it.
    pub fn path_is_within(self, path: &Path, ancestor: &Path) -> bool {
        self.relative_to(path, ancestor).is_some()
    }

    pub fn has_component_named(self, path: &Path, name: &str) -> bool {
        let name = self.comparable_name(name);
        self.normal_component_keys(path)
            .any(|component| component == name)
    }

    /// Whether some directory on the way to `path` holds one of `entries` that
    /// `path` is, or is inside of — as it would if that directory were the
    /// home.
    pub fn is_under_home_entry_anywhere(self, path: &Path, entries: &[&str]) -> bool {
        let names = self.normal_component_keys(path).collect::<Vec<_>>();
        entries.iter().any(|entry| {
            let entry_names = self
                .normal_component_keys(Path::new(entry))
                .collect::<Vec<_>>();
            !entry_names.is_empty()
                && names
                    .windows(entry_names.len())
                    .any(|window| window == entry_names.as_slice())
        })
    }
}

/// Unicode's default-ignorable code points (`Default_Ignorable_Code_Point`),
/// which render as nothing, so a name containing them looks like one without.
fn is_default_ignorable(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// What no verified write may reach, whoever asks for it: Git metadata inside
/// `root`, and the protected entries of the account's `home`. The client
/// judges agent writes in full first; this holds even for a caller that
/// skipped that (a client with a bug, or one talking to a remote server
/// directly).
pub fn refusal_for(root: &Path, relative_path: &Path, home: &Path) -> Option<VerifiedWriteError> {
    let target = root.join(relative_path);
    let rules = NameRules::for_path(&target);
    if rules.has_component_named(relative_path, GIT_DIRECTORY_NAME) {
        return Some(VerifiedWriteError::GitMetadata(target));
    }
    UNIX_PROTECTED_HOME_RELATIVE
        .iter()
        .chain(AGENT_CONFIGURATION_HOME_RELATIVE)
        .any(|entry| rules.path_is_within(&target, &home.join(entry)))
        .then(|| VerifiedWriteError::Protected(target))
}

/// Whether two paths read back from open handles name the same file on a
/// filesystem that ignores case.
pub fn same_path_ignoring_case(actual: &Path, expected: &Path) -> bool {
    actual.as_os_str().to_string_lossy().to_uppercase()
        == expected.as_os_str().to_string_lossy().to_uppercase()
}

/// The names `relative_path` walks through, all of which must be plain names.
pub(crate) fn plain_names(relative_path: &Path) -> Result<Vec<&OsStr>, VerifiedWriteError> {
    relative_path
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(VerifiedWriteError::NotBeneath(relative_path.to_path_buf())),
        })
        .collect()
}

/// See [`crate::Fs::verify_read`].
///
/// The refusals of [`refusal_for`] are not applied here: they are about what an
/// agent may *change*, and a sandboxed agent reads Git's own files (`.git/HEAD`
/// and the like) inside its sandbox as well. What this refuses is only a path
/// that leads somewhere else than where it is spelled.
pub(crate) fn verify_read_blocking(
    root: &Path,
    relative_path: &Path,
) -> Result<(), VerifiedWriteError> {
    plain_names(relative_path)?;
    platform::open_beneath(root, relative_path)
}

/// See [`crate::Fs::write_verified`].
pub(crate) fn write_verified_blocking(
    root: &Path,
    relative_path: &Path,
    content: &[u8],
) -> Result<(), VerifiedWriteError> {
    plain_names(relative_path)?;
    if let Some(refusal) = refusal_for(root, relative_path, paths::home_dir()) {
        return Err(refusal);
    }
    platform::write_beneath(root, relative_path, content)
}

#[cfg(windows)]
mod platform {
    use super::{VerifiedWriteError, plain_names, same_path_ignoring_case};
    use std::ffi::{OsString, c_void};
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write as _};
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::path::{Path, PathBuf};
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileDispositionInfo, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        SetFileInformationByHandle,
    };

    // Open without `FILE_SHARE_DELETE`, a folder or file can be neither renamed
    // nor deleted, so nothing on the way can be swapped for a link between
    // being checked and being written through.
    const SHARE_ALL_BUT_DELETE: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0;

    pub(super) fn write_beneath(
        root: &Path,
        relative_path: &Path,
        content: &[u8],
    ) -> Result<(), VerifiedWriteError> {
        let names = plain_names(relative_path)?;
        // The root is the folder the user opened, so a link there is theirs.
        let root_folder = open_folder(root, true)?;
        let Some((file_name, folder_names)) = names.split_last() else {
            return write_root_file(root, content);
        };

        let mut expected = final_path(&root_folder)?;
        let mut open_folders = vec![root_folder];
        for name in folder_names {
            expected.push(name);
            let folder = match open_folder(&expected, false) {
                Ok(folder) => folder,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match std::fs::create_dir(&expected) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error.into()),
                    }
                    open_folder(&expected, false)?
                }
                Err(error) => return Err(error.into()),
            };
            let attributes = file_information(&folder)?.dwFileAttributes;
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                return Err(VerifiedWriteError::Redirected(expected));
            }
            if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
                return Err(VerifiedWriteError::NotAFolder(expected));
            }
            // A folder can go by a second name (an 8.3 alias such as `GIT~1`);
            // only the name it really has may be on the way.
            let actual = final_path(&folder)?;
            if !same_path_ignoring_case(&actual, &expected) {
                return Err(VerifiedWriteError::Elsewhere { expected, actual });
            }
            open_folders.push(folder);
        }

        expected.push(file_name);
        let (mut file, created) = open_file(&expected)?;
        if let Err(error) = check_file(&file, &expected) {
            if created && let Err(delete_error) = delete_through(&file) {
                log::error!(
                    "could not remove {} after refusing to write it: {delete_error}",
                    expected.display()
                );
            }
            return Err(error);
        }
        file.set_len(0)?;
        file.write_all(content)?;
        drop(open_folders);
        Ok(())
    }

    pub(super) fn open_beneath(
        root: &Path,
        relative_path: &Path,
    ) -> Result<(), VerifiedWriteError> {
        let names = plain_names(relative_path)?;
        // The root is the folder the user opened, so a link there is theirs.
        let root_folder = open_folder(root, true)?;
        let Some((file_name, folder_names)) = names.split_last() else {
            return check_root_file(root);
        };

        let mut expected = final_path(&root_folder)?;
        let mut open_folders = vec![root_folder];
        for name in folder_names {
            expected.push(name);
            let folder = open_folder(&expected, false)?;
            let attributes = file_information(&folder)?.dwFileAttributes;
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                return Err(VerifiedWriteError::Redirected(expected));
            }
            if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
                return Err(VerifiedWriteError::NotAFolder(expected));
            }
            let actual = final_path(&folder)?;
            if !same_path_ignoring_case(&actual, &expected) {
                return Err(VerifiedWriteError::Elsewhere { expected, actual });
            }
            open_folders.push(folder);
        }

        expected.push(file_name);
        let file = open_folder(&expected, false)?;
        let information = file_information(&file)?;
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(VerifiedWriteError::Redirected(expected));
        }
        if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            return Err(VerifiedWriteError::NotAFile(expected));
        }
        let actual = final_path(&file)?;
        if !same_path_ignoring_case(&actual, &expected) {
            return Err(VerifiedWriteError::Elsewhere { expected, actual });
        }
        drop(open_folders);
        Ok(())
    }

    /// A file the user opened on its own: a link to it is theirs to follow.
    fn check_root_file(root: &Path) -> Result<(), VerifiedWriteError> {
        let file = open_folder(root, true)?;
        if file_information(&file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            return Err(VerifiedWriteError::NotAFile(root.to_path_buf()));
        }
        Ok(())
    }

    /// Opens a folder, or — with `FILE_FLAG_BACKUP_SEMANTICS` — any other kind
    /// of entry, for reading its attributes and its real path only.
    fn open_folder(path: &Path, follow_link: bool) -> io::Result<File> {
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS.0;
        if !follow_link {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT.0;
        }
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode(SHARE_ALL_BUT_DELETE)
            .custom_flags(flags)
            .open(path)
    }

    /// Opens the file itself even when it is a link, creating it when it is
    /// missing; the flag says whether this call created it.
    fn open_file(path: &Path) -> Result<(File, bool), VerifiedWriteError> {
        let mut options = OpenOptions::new();
        options
            .write(true)
            .share_mode(SHARE_ALL_BUT_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
        match options
            .clone()
            .create_new(true)
            .access_mode(GENERIC_READ.0 | GENERIC_WRITE.0 | DELETE.0)
            .open(path)
        {
            Ok(file) => return Ok((file, true)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let file = options
            .access_mode(GENERIC_READ.0 | GENERIC_WRITE.0)
            .open(path)?;
        Ok((file, false))
    }

    fn check_file(file: &File, expected: &Path) -> Result<(), VerifiedWriteError> {
        let information = file_information(file)?;
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(VerifiedWriteError::Redirected(expected.to_path_buf()));
        }
        if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            return Err(VerifiedWriteError::NotAFile(expected.to_path_buf()));
        }
        // The link count is read before the write, so a hard link made in
        // between would still be written through. That gains nothing: the
        // sandbox keeps `.git` and the protected folders read-only, so a
        // sandboxed command can't link to what is in them.
        if information.nNumberOfLinks != 1 {
            return Err(VerifiedWriteError::HardLinked(expected.to_path_buf()));
        }
        let actual = final_path(file)?;
        if !same_path_ignoring_case(&actual, expected) {
            return Err(VerifiedWriteError::Elsewhere {
                expected: expected.to_path_buf(),
                actual,
            });
        }
        Ok(())
    }

    /// A file the user opened on its own: a link to it is theirs to follow.
    fn write_root_file(root: &Path, content: &[u8]) -> Result<(), VerifiedWriteError> {
        let mut file = OpenOptions::new()
            .write(true)
            .access_mode(GENERIC_READ.0 | GENERIC_WRITE.0)
            .share_mode(SHARE_ALL_BUT_DELETE)
            .open(root)?;
        let information = file_information(&file)?;
        if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            return Err(VerifiedWriteError::NotAFile(root.to_path_buf()));
        }
        if information.nNumberOfLinks != 1 {
            return Err(VerifiedWriteError::HardLinked(root.to_path_buf()));
        }
        file.set_len(0)?;
        file.write_all(content)?;
        Ok(())
    }

    fn handle(file: &File) -> HANDLE {
        HANDLE(file.as_raw_handle() as _)
    }

    fn file_information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(handle(file), &mut information) }
            .map_err(io::Error::from)?;
        Ok(information)
    }

    fn final_path(file: &File) -> io::Result<PathBuf> {
        let handle = handle(file);
        let required = unsafe { GetFinalPathNameByHandleW(handle, &mut [], FILE_NAME_NORMALIZED) };
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u16; required as usize + 1];
        let written =
            unsafe { GetFinalPathNameByHandleW(handle, &mut buffer, FILE_NAME_NORMALIZED) };
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        let written = buffer
            .get(..written as usize)
            .ok_or_else(|| io::Error::other("the path is longer than its buffer"))?;
        Ok(PathBuf::from(OsString::from_wide(written)))
    }

    fn delete_through(file: &File) -> io::Result<()> {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        unsafe {
            SetFileInformationByHandle(
                handle(file),
                FileDispositionInfo,
                (&raw const disposition).cast::<c_void>(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        }
        .map_err(io::Error::from)
    }
}

#[cfg(unix)]
mod platform {
    use super::{VerifiedWriteError, plain_names};
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::ffi::OsStr;
    use std::fs::OpenOptions;
    use std::io::{self, Write as _};
    use std::os::fd::OwnedFd;
    use std::path::{Path, PathBuf};

    pub(super) fn write_beneath(
        root: &Path,
        relative_path: &Path,
        content: &[u8],
    ) -> Result<(), VerifiedWriteError> {
        let names = plain_names(relative_path)?;
        // The root is the folder the user opened, so a link there is theirs.
        let root_folder =
            rustix::fs::open(root, folder_flags(), Mode::empty()).map_err(io::Error::from)?;
        let Some((file_name, folder_names)) = names.split_last() else {
            return write_root_file(root, content);
        };

        let mut expected = descriptor_path(&root_folder)?;
        let mut folder = root_folder;
        for name in folder_names {
            expected.push(name);
            folder = match open_folder_at(&folder, name) {
                Ok(next) => next,
                Err(Errno::NOENT) => {
                    match rustix::fs::mkdirat(&folder, *name, Mode::RWXU | Mode::RWXG | Mode::RWXO)
                    {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(errno) => return Err(io::Error::from(errno).into()),
                    }
                    open_folder_at(&folder, name)
                        .map_err(|errno| redirected_or(errno, &expected))?
                }
                Err(errno) => return Err(redirected_or(errno, &expected)),
            };
        }

        expected.push(file_name);
        let file_flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let new_file_mode = Mode::from_raw_mode(0o666);
        let (descriptor, created) = match rustix::fs::openat(
            &folder,
            *file_name,
            file_flags | OFlags::CREATE | OFlags::EXCL,
            new_file_mode,
        ) {
            Ok(descriptor) => (descriptor, true),
            Err(Errno::EXIST) => (
                rustix::fs::openat(&folder, *file_name, file_flags, Mode::empty())
                    .map_err(|errno| redirected_or(errno, &expected))?,
                false,
            ),
            Err(errno) => return Err(io::Error::from(errno).into()),
        };
        if let Err(error) = check_file(&descriptor, &expected) {
            if created
                && let Err(unlink_error) =
                    rustix::fs::unlinkat(&folder, *file_name, AtFlags::empty())
            {
                log::error!(
                    "could not remove {} after refusing to write it: {unlink_error}",
                    expected.display()
                );
            }
            return Err(error);
        }
        rustix::fs::ftruncate(&descriptor, 0).map_err(io::Error::from)?;
        std::fs::File::from(descriptor).write_all(content)?;
        Ok(())
    }

    pub(super) fn open_beneath(
        root: &Path,
        relative_path: &Path,
    ) -> Result<(), VerifiedWriteError> {
        let names = plain_names(relative_path)?;
        // The root is the folder the user opened, so a link there is theirs.
        let root_folder =
            rustix::fs::open(root, folder_flags(), Mode::empty()).map_err(io::Error::from)?;
        let Some((file_name, folder_names)) = names.split_last() else {
            return check_root_file(root);
        };

        let mut expected = descriptor_path(&root_folder)?;
        let mut folder = root_folder;
        for name in folder_names {
            expected.push(name);
            folder =
                open_folder_at(&folder, name).map_err(|errno| redirected_or(errno, &expected))?;
        }

        expected.push(file_name);
        // `O_NONBLOCK` so that a named pipe left in the folder opens instead of
        // waiting for a writer; the check below refuses it either way.
        let descriptor = rustix::fs::openat(
            &folder,
            *file_name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| redirected_or(errno, &expected))?;
        check_readable(&descriptor, &expected)
    }

    /// A file the user opened on its own: a link to it is theirs to follow.
    fn check_root_file(root: &Path) -> Result<(), VerifiedWriteError> {
        let descriptor = rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let stat = rustix::fs::fstat(&descriptor).map_err(io::Error::from)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(VerifiedWriteError::NotAFile(root.to_path_buf()));
        }
        Ok(())
    }

    /// The link count is deliberately not checked here, unlike in a verified
    /// write: a hard link inside the folder is a second name for a file the
    /// sandboxed agent can open itself, so refusing it would only turn away
    /// ordinary reads (a package manager links its store into the project),
    /// while a symbolic link into what the sandbox hides — which it cannot
    /// follow itself — is refused above.
    fn check_readable(descriptor: &OwnedFd, expected: &Path) -> Result<(), VerifiedWriteError> {
        let stat = rustix::fs::fstat(descriptor).map_err(io::Error::from)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(VerifiedWriteError::NotAFile(expected.to_path_buf()));
        }
        let actual = descriptor_path(descriptor)?;
        if !same_path(&actual, expected) {
            return Err(VerifiedWriteError::Elsewhere {
                expected: expected.to_path_buf(),
                actual,
            });
        }
        Ok(())
    }

    fn folder_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC
    }

    fn open_folder_at(folder: &OwnedFd, name: &OsStr) -> rustix::io::Result<OwnedFd> {
        rustix::fs::openat(
            folder,
            name,
            folder_flags() | OFlags::NOFOLLOW,
            Mode::empty(),
        )
    }

    fn redirected_or(errno: Errno, path: &Path) -> VerifiedWriteError {
        match errno {
            // `O_NOFOLLOW` refuses a link with `ELOOP`, and `O_DIRECTORY` a
            // non-folder (which a link is, not followed) with `ENOTDIR`.
            Errno::LOOP | Errno::NOTDIR => VerifiedWriteError::Redirected(path.to_path_buf()),
            errno => VerifiedWriteError::Io(errno.into()),
        }
    }

    fn check_file(descriptor: &OwnedFd, expected: &Path) -> Result<(), VerifiedWriteError> {
        let stat = rustix::fs::fstat(descriptor).map_err(io::Error::from)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(VerifiedWriteError::NotAFile(expected.to_path_buf()));
        }
        // The link count is read before the write, so a hard link made in
        // between would still be written through. That gains nothing: the
        // sandbox keeps `.git` and the protected folders read-only, so a
        // sandboxed command can't link to what is in them.
        if stat.st_nlink != 1 {
            return Err(VerifiedWriteError::HardLinked(expected.to_path_buf()));
        }
        let actual = descriptor_path(descriptor)?;
        if !same_path(&actual, expected) {
            return Err(VerifiedWriteError::Elsewhere {
                expected: expected.to_path_buf(),
                actual,
            });
        }
        Ok(())
    }

    /// A file the user opened on its own: a link to it is theirs to follow.
    fn write_root_file(root: &Path, content: &[u8]) -> Result<(), VerifiedWriteError> {
        let mut file = OpenOptions::new().write(true).open(root)?;
        let stat = rustix::fs::fstat(&file).map_err(io::Error::from)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(VerifiedWriteError::NotAFile(root.to_path_buf()));
        }
        if stat.st_nlink != 1 {
            return Err(VerifiedWriteError::HardLinked(root.to_path_buf()));
        }
        file.set_len(0)?;
        file.write_all(content)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn descriptor_path(descriptor: &OwnedFd) -> io::Result<PathBuf> {
        use std::os::fd::AsRawFd as _;
        std::fs::read_link(format!("/proc/self/fd/{}", descriptor.as_raw_fd()))
    }

    #[cfg(target_os = "macos")]
    fn descriptor_path(descriptor: &OwnedFd) -> io::Result<PathBuf> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::ffi::OsStrExt as _;
        let mut buffer = vec![0u8; libc::PATH_MAX as usize];
        let result =
            unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        let length = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        buffer.truncate(length);
        Ok(PathBuf::from(OsStr::from_bytes(&buffer)))
    }

    // Without a way to read a descriptor's path back, the write can't be
    // verified, so it doesn't happen.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn descriptor_path(_descriptor: &OwnedFd) -> io::Result<PathBuf> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reading a file's path from its descriptor is not supported on this system",
        ))
    }

    #[cfg(target_os = "macos")]
    fn same_path(actual: &Path, expected: &Path) -> bool {
        super::same_path_ignoring_case(actual, expected)
    }

    #[cfg(not(target_os = "macos"))]
    fn same_path(actual: &Path, expected: &Path) -> bool {
        actual == expected
    }
}
