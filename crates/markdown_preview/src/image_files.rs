//! Putting a picture into the `images` folder next to a Markdown document, for
//! the image dialog and for pasting or dropping one into the document.
//!
//! The document may be on this machine or on an SSH or WSL host. Either way
//! the picture goes through the document's worktree, which is what can write
//! on the host that holds it; a file picked or dropped from this machine is
//! read here and sent over as bytes, since the host cannot see this machine's
//! paths.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use fs::{CreateOptions, Fs, RemoveOptions};
use futures::channel::oneshot;
use gpui::{App, AsyncApp, Entity, Task, WeakEntity};
use project::{Project, ProjectEntryId, ProjectPath, Worktree, WorktreeId};
use util::ResultExt as _;
use util::paths::PathStyle;
use util::rel_path::RelPath;

use crate::image_dialog::{encode_destination, escape_attribute};

/// The largest picture that is stored. A screenshot or a photo is well below
/// it; anything larger is more likely a mistaken pick, and on a remote host it
/// would travel over the connection in one message.
pub(crate) const MAXIMUM_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

const IMAGES_FOLDER: &str = "images";

/// How many names are tried before giving up, which is far more than a folder
/// of pictures holds under one name.
const MAXIMUM_NAME_ATTEMPTS: usize = 10_000;

pub(crate) enum ImageData {
    /// A file on this machine (the OS file picker, a file dropped from the OS).
    LocalFile(PathBuf),
    /// Bytes with no file behind them (a pasted screenshot). `extension` without the dot, e.g. "png".
    Bytes { bytes: Vec<u8>, extension: String },
    /// A file of one of the project's worktrees. On a remote host it is
    /// copied there, never passing through this machine.
    ProjectFile(ProjectPath),
}

/// Stores the image in the `images` folder next to `document` (creating the folder), under
/// `preferred_name` (or the local file's name) with `-1`, `-2`, … appended on a collision.
/// Works for local and remote (SSH / WSL) worktrees: remote ones get the bytes through the
/// worktree (`Worktree::create_entry(path, false, Some(bytes))` or `copy_external_entries`).
/// Never overwrites an existing entry. Returns the project path it was stored at.
///
/// A name counts as taken when anything is there, a link that leads nowhere
/// included, since writing to it would write wherever it points. Images larger
/// than [`MAXIMUM_IMAGE_BYTES`] are refused. If the task is dropped before it
/// finishes, whatever it already stored is removed again; if storing on this
/// machine fails half-way, so is the partly written file.
///
/// A document on this machine opened on its own is a worktree by itself, with
/// no entry for the folder beside it. The picture is then written there with
/// the file system, and the returned path, in the document's worktree, is
/// relative to the document's folder rather than naming an entry; the other
/// functions here read it that way.
pub(crate) fn store_image_next_to(
    project: &Entity<Project>,
    document: &ProjectPath,
    image: ImageData,
    preferred_name: Option<String>,
    cx: &mut App,
) -> Task<Result<ProjectPath>> {
    let destination = match Destination::for_document(project, document, cx) {
        Ok(destination) => destination,
        Err(error) => return Task::ready(Err(error)),
    };
    let fs = project.read(cx).fs().clone();
    let project = project.downgrade();
    let (sender, receiver) = oneshot::channel();
    cx.spawn(async move |cx| {
        let stored = store(
            &destination,
            &project,
            fs.as_ref(),
            image,
            preferred_name,
            cx,
        )
        .await;
        let (reply, stored) = match stored {
            Ok(stored) => (Ok(stored.project_path.clone()), Some(stored)),
            Err(error) => (Err(error), None),
        };
        // Whoever asked has gone, dialog closed or paste abandoned, so the
        // picture would sit in the folder with nothing linking to it.
        if sender.send(reply).is_err()
            && let Some(stored) = stored
        {
            remove_stored(&destination, fs.as_ref(), stored, cx)
                .await
                .log_err();
        }
    })
    .detach();
    cx.background_executor().spawn(async move {
        receiver
            .await
            .map_err(|_| anyhow!("画像の保存が中断されました。"))?
    })
}

/// The link text from `document` to `image` for Markdown (`![](…)`) or an HTML attribute,
/// with the same encoding the dialog uses for new paths.
///
/// For an attribute (`in_markdown` false) the text is ready to go between the
/// double quotes of `src="…"` or `href="…"` as it is: a `"` is percent-encoded
/// and a `&` written as `&amp;`. For Markdown it goes between the parentheses
/// of `![…](…)` as it is. `None` when the two are in different worktrees,
/// since there is then no path between them that the worktrees can tell.
pub(crate) fn link_destination(
    document: &ProjectPath,
    image: &ProjectPath,
    in_markdown: bool,
    _cx: &App,
) -> Option<String> {
    let relative = relative_link(document, image)?;
    let encoded = encode_destination(&relative, in_markdown);
    Some(if in_markdown {
        encoded
    } else {
        // `%22` rather than `&quot;`, which a reader of the attribute would
        // turn back into a quote in the middle of the path.
        escape_attribute(&encoded.replace('"', "%22"))
    })
}

/// The `/`-separated path from `document`'s folder to `image`, before any
/// encoding.
pub(crate) fn relative_link(document: &ProjectPath, image: &ProjectPath) -> Option<String> {
    if document.worktree_id != image.worktree_id || image.path.is_empty() {
        return None;
    }
    if document.path.is_empty() {
        // The worktree of a document opened on its own holds nothing else, so
        // a path in it is one stored beside the document.
        return Some(image.path.as_unix_str().to_string());
    }
    let folder = document_folder(&document.path)?;
    let from = folder.components().collect::<Vec<_>>();
    let to = image.path.components().collect::<Vec<_>>();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts = vec![".."; from.len() - common];
    parts.extend(to.get(common..)?.iter().copied());
    Some(parts.join("/"))
}

/// The folder `document` is in within its worktree. `None` for a file opened
/// on its own, which is a worktree by itself with nothing beside it.
fn document_folder(document: &RelPath) -> Option<&RelPath> {
    if document.is_empty() {
        return None;
    }
    document.parent()
}

/// Takes away a picture [`store_image_next_to`] stored, when nothing is to
/// link to it after all. Only a file is removed, never a folder.
pub(crate) fn remove_stored_image(
    project: &Entity<Project>,
    document: &ProjectPath,
    image: &ProjectPath,
    cx: &mut App,
) -> Task<Result<()>> {
    let destination = match Destination::for_document(project, document, cx) {
        Ok(destination) => destination,
        Err(error) => return Task::ready(Err(error)),
    };
    let entry_id = project
        .read(cx)
        .entry_for_path(image, cx)
        .filter(|entry| entry.is_file())
        .map(|entry| entry.id);
    let fs = project.read(cx).fs().clone();
    let stored = StoredImage {
        project_path: image.clone(),
        entry_id,
    };
    cx.spawn(async move |cx| remove_stored(&destination, fs.as_ref(), stored, cx).await)
}

/// Why a picture cannot be stored next to `document`, if it cannot.
pub(crate) fn storing_refusal(
    project: &Entity<Project>,
    document: &ProjectPath,
    cx: &App,
) -> Option<String> {
    Destination::for_document(project, document, cx)
        .err()
        .map(|error| error.to_string())
}

/// The `images` folder a picture goes into, with what writing there needs.
struct Destination {
    worktree: WeakEntity<Worktree>,
    worktree_id: WorktreeId,
    folder: Arc<RelPath>,
    /// The worktree's root on the machine that holds it, spelled that
    /// machine's way; for a document opened on its own, the document's folder.
    root: String,
    path_style: PathStyle,
    is_local: bool,
    /// Whether the document is a file on this machine opened on its own, whose
    /// worktree has no entries beside it to create the picture through.
    beside_lone_file: bool,
}

impl Destination {
    fn for_document(project: &Entity<Project>, document: &ProjectPath, cx: &App) -> Result<Self> {
        let project = project.read(cx);
        let worktree = project
            .worktree_for_id(document.worktree_id, cx)
            .context("Markdown のあるワークツリーが見つかりません。")?;
        let is_local = worktree.read(cx).is_local();
        if !is_local && project.remote_client().is_none() {
            return Err(anyhow!(
                "共同編集で参加しているプロジェクトには画像を保存できません。"
            ));
        }
        let images = RelPath::from_unix_str(IMAGES_FOLDER)?;
        let path_style = project.path_style(cx);
        let root = worktree.read(cx).abs_path().to_string_lossy().into_owned();
        let (folder, root, beside_lone_file) = match document_folder(&document.path) {
            Some(folder) => (folder.join(images).into_arc(), root, false),
            None => {
                let lone_file = is_local
                    && worktree
                        .read(cx)
                        .root_entry()
                        .is_some_and(|entry| entry.is_file());
                let document_directory = lone_file
                    .then(|| Path::new(&root).parent())
                    .flatten()
                    .context(
                        "単独で開いた Markdown の隣には画像を保存できません。Markdown のあるフォルダーを開いてください。",
                    )?;
                (
                    images.into_arc(),
                    document_directory.to_string_lossy().into_owned(),
                    true,
                )
            }
        };
        Ok(Self {
            worktree_id: document.worktree_id,
            folder,
            root,
            path_style,
            is_local,
            beside_lone_file,
            worktree: worktree.downgrade(),
        })
    }

    fn absolute(&self, path: &RelPath) -> Result<String> {
        self.path_style
            .join(&self.root, path.display(self.path_style).as_ref())
            .with_context(|| format!("パスを組み立てられません: {}", path.as_unix_str()))
    }
}

/// What was stored, with what taking it away again needs.
struct StoredImage {
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
}

async fn store(
    destination: &Destination,
    project: &WeakEntity<Project>,
    fs: &dyn Fs,
    image: ImageData,
    preferred_name: Option<String>,
    cx: &mut AsyncApp,
) -> Result<StoredImage> {
    let file_name = stored_file_name(&image, preferred_name)?;
    let bytes = match image {
        ImageData::LocalFile(path) => read_local_image(fs, &path).await?,
        ImageData::Bytes { bytes, .. } => {
            check_size(bytes.len() as u64)?;
            bytes
        }
        ImageData::ProjectFile(source) if !destination.is_local => {
            return copy_on_host(destination, project, &source, &file_name, cx).await;
        }
        ImageData::ProjectFile(source) => {
            let path = project
                .read_with(cx, |project, cx| project.absolute_path(&source, cx))?
                .with_context(|| {
                    format!(
                        "画像ファイルが見つかりません: {}",
                        source.path.display(destination.path_style)
                    )
                })?;
            read_local_image(fs, &path).await?
        }
    };

    if !destination.is_local {
        let path = find_remote_name(destination, project, &file_name, cx).await?;
        let worktree = destination
            .worktree
            .upgrade()
            .context("Markdown のあるワークツリーが閉じられました。")?;
        // Nothing is taken away when this fails: the name was only seen to be
        // free a moment ago, and what is at it now may be something else's.
        let created = worktree
            .update(cx, |worktree, cx| {
                worktree.create_entry(path.clone(), false, Some(bytes), cx)
            })
            .await
            .with_context(|| {
                format!(
                    "画像を保存できませんでした: {}",
                    path.display(destination.path_style)
                )
            })?;
        return Ok(StoredImage {
            project_path: ProjectPath {
                worktree_id: destination.worktree_id,
                path,
            },
            entry_id: created.into_included().map(|entry| entry.id),
        });
    }

    let path = reserve_local_name(destination, fs, &file_name).await?;
    let reserved = StoredImage {
        project_path: ProjectPath {
            worktree_id: destination.worktree_id,
            path: path.clone(),
        },
        entry_id: None,
    };
    // The name was claimed with a new empty file just now, so what is at it,
    // written in part or not at all, is this picture's and can go again.
    let written = write_local(destination, fs, &path, bytes, cx).await;
    match written {
        Ok(entry_id) => Ok(StoredImage {
            entry_id,
            ..reserved
        }),
        Err(error) => {
            remove_stored(destination, fs, reserved, cx).await.log_err();
            Err(error.context(format!(
                "画像を保存できませんでした: {}",
                path.display(destination.path_style)
            )))
        }
    }
}

/// Copies a file of the remote project next to the document on the host
/// itself. The host's image store cannot read every format a Markdown file
/// links (SVG, AVIF), and the copy never needs the bytes here anyway.
async fn copy_on_host(
    destination: &Destination,
    project: &WeakEntity<Project>,
    source: &ProjectPath,
    file_name: &str,
    cx: &mut AsyncApp,
) -> Result<StoredImage> {
    let (entry_id, size) = project
        .read_with(cx, |project, cx| {
            project
                .entry_for_path(source, cx)
                .filter(|entry| entry.is_file())
                .map(|entry| (entry.id, entry.size))
        })?
        .with_context(|| {
            format!(
                "画像ファイルが見つかりません: {}",
                source.path.display(destination.path_style)
            )
        })?;
    check_size(size)?;
    let path = find_remote_name(destination, project, file_name, cx).await?;
    let worktree = destination
        .worktree
        .upgrade()
        .context("Markdown のあるワークツリーが閉じられました。")?;
    // Unlike writing bytes, copying on the host does not create the folder.
    let has_folder = worktree.read_with(cx, |worktree, _| {
        worktree.entry_for_path(&destination.folder).is_some()
    });
    if !has_folder {
        worktree
            .update(cx, |worktree, cx| {
                worktree.create_entry(destination.folder.clone(), true, None, cx)
            })
            .await
            .with_context(|| {
                format!(
                    "images フォルダーを作れません: {}",
                    destination.folder.display(destination.path_style)
                )
            })?;
    }
    let project_path = ProjectPath {
        worktree_id: destination.worktree_id,
        path,
    };
    // The host refuses to copy over anything already at the name, so unlike
    // `create_entry` this cannot overwrite what turned up there meanwhile.
    let copied = project
        .update(cx, |project, cx| {
            project.copy_entry(entry_id, project_path.clone(), cx)
        })?
        .await
        .with_context(|| {
            format!(
                "接続先で画像をコピーできませんでした: {}",
                project_path.path.display(destination.path_style)
            )
        })?;
    Ok(StoredImage {
        project_path,
        entry_id: copied.map(|entry| entry.id),
    })
}

/// Writes the picture over the empty file that claimed its name, through the
/// worktree so that it shows up there at once, or straight to the disk beside
/// a document opened on its own.
async fn write_local(
    destination: &Destination,
    fs: &dyn Fs,
    path: &Arc<RelPath>,
    bytes: Vec<u8>,
    cx: &mut AsyncApp,
) -> Result<Option<ProjectEntryId>> {
    if destination.beside_lone_file {
        fs.write(Path::new(&destination.absolute(path)?), &bytes)
            .await?;
        return Ok(None);
    }
    let worktree = destination
        .worktree
        .upgrade()
        .context("Markdown のあるワークツリーが閉じられました。")?;
    let created = worktree
        .update(cx, |worktree, cx| {
            worktree.create_entry(path.clone(), false, Some(bytes), cx)
        })
        .await?;
    Ok(created.into_included().map(|entry| entry.id))
}

fn check_size(length: u64) -> Result<()> {
    if length > MAXIMUM_IMAGE_BYTES {
        return Err(anyhow!(
            "画像が大きすぎます ({:.1} MiB)。保存できるのは {} MiB までです。",
            length as f64 / (1024.0 * 1024.0),
            MAXIMUM_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

async fn read_local_image(fs: &dyn Fs, path: &Path) -> Result<Vec<u8>> {
    let metadata = fs
        .metadata(path)
        .await?
        .with_context(|| format!("画像ファイルが見つかりません: {}", path.display()))?;
    if metadata.is_dir {
        return Err(anyhow!(
            "フォルダーは画像として保存できません: {}",
            path.display()
        ));
    }
    // Checked before reading, so a huge file is turned away without being
    // loaded into memory first.
    check_size(metadata.len)?;
    let bytes = fs
        .load_bytes(path)
        .await
        .with_context(|| format!("画像ファイルを読み込めません: {}", path.display()))?;
    check_size(bytes.len() as u64)?;
    Ok(bytes)
}

/// The name the picture is stored under before a collision is avoided: the
/// preferred name, or the local file's own, made safe to be one component of
/// a path on any host.
fn stored_file_name(image: &ImageData, preferred_name: Option<String>) -> Result<String> {
    let name = match (preferred_name, image) {
        (Some(name), _) => name,
        (None, ImageData::LocalFile(path)) => path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("ファイル名を読み取れません: {}", path.display()))?
            .to_string(),
        (None, ImageData::Bytes { .. }) => "image".to_string(),
        (None, ImageData::ProjectFile(source)) => source
            .path
            .file_name()
            .with_context(|| format!("ファイル名を読み取れません: {}", source.path.as_unix_str()))?
            .to_string(),
    };
    let mut name = sanitize_file_name(&name);
    if let ImageData::Bytes { extension, .. } = image {
        let extension = extension
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>();
        let has_extension = name
            .rsplit_once('.')
            .is_some_and(|(stem, extension)| !stem.is_empty() && !extension.is_empty());
        if !has_extension && !extension.is_empty() {
            name.push('.');
            name.push_str(&extension);
        }
    }
    Ok(name)
}

/// Replaces what a file name cannot hold on Windows, where the strictest
/// rules are, and what would make it more than one component of a path.
pub(crate) fn sanitize_file_name(name: &str) -> String {
    let replaced = name
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            character if character.is_control() => '-',
            character => character,
        })
        .collect::<String>();
    // Windows drops a trailing dot or space, which would store the file under
    // a name other than the one written into the link.
    let trimmed = replaced.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed.chars().all(|character| character == '.') {
        "image".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `file_name` itself, then with `-1`, `-2`, … before its extension.
fn candidate_names(file_name: &str) -> impl Iterator<Item = String> + '_ {
    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (file_name, String::new()),
    };
    (0..MAXIMUM_NAME_ATTEMPTS).map(move |attempt| {
        if attempt == 0 {
            file_name.to_string()
        } else {
            format!("{stem}-{attempt}{extension}")
        }
    })
}

fn no_free_name(file_name: &str) -> anyhow::Error {
    anyhow!("images フォルダーに使える名前が見つかりません: {file_name}")
}

/// Whether anything is at `path` on this machine, a link that leads nowhere
/// included.
async fn local_path_is_taken(fs: &dyn Fs, path: &Path) -> Result<bool> {
    Ok(fs.metadata(path).await?.is_some() || fs.read_link(path).await.is_ok())
}

/// Picks a free name in the `images` folder on this machine and claims it with
/// an empty file, which only succeeds when nothing is there, so that nothing
/// that turns up there in the meantime is overwritten.
async fn reserve_local_name(
    destination: &Destination,
    fs: &dyn Fs,
    file_name: &str,
) -> Result<Arc<RelPath>> {
    let folder = PathBuf::from(destination.absolute(&destination.folder)?);
    fs.create_dir(&folder)
        .await
        .with_context(|| format!("images フォルダーを作れません: {}", folder.display()))?;
    for candidate in candidate_names(file_name) {
        let path = destination
            .folder
            .join(RelPath::from_unix_str(&candidate)?)
            .into_arc();
        let absolute = folder.join(&candidate);
        if local_path_is_taken(fs, &absolute).await? {
            continue;
        }
        let options = CreateOptions {
            overwrite: false,
            ignore_if_exists: false,
        };
        match fs.create_file(&absolute, options).await {
            Ok(()) => return Ok(path),
            Err(_) if local_path_is_taken(fs, &absolute).await? => continue,
            Err(error) => {
                return Err(
                    error.context(format!("画像ファイルを作れません: {}", absolute.display()))
                );
            }
        }
    }
    Err(no_free_name(file_name))
}

/// Picks a name that nothing on the remote host occupies. The host is asked
/// about each name, since the worktree does not list what it ignores.
async fn find_remote_name(
    destination: &Destination,
    project: &WeakEntity<Project>,
    file_name: &str,
    cx: &mut AsyncApp,
) -> Result<Arc<RelPath>> {
    let client = project
        .read_with(cx, |project, cx| {
            project
                .remote_client()
                .map(|client| client.read(cx).proto_client())
        })?
        .context("リモートの接続が見つかりません。")?;
    let worktree = destination
        .worktree
        .upgrade()
        .context("Markdown のあるワークツリーが閉じられました。")?;
    for candidate in candidate_names(file_name) {
        let path = destination
            .folder
            .join(RelPath::from_unix_str(&candidate)?)
            .into_arc();
        if worktree.read_with(cx, |worktree, _| worktree.entry_for_path(&path).is_some()) {
            continue;
        }
        let response = client
            .request(proto::GetPathMetadata {
                project_id: proto::REMOTE_SERVER_PROJECT_ID,
                path: destination.absolute(&path)?,
            })
            .await
            .context("リモートのファイルを確認できませんでした。")?;
        if !response.exists {
            return Ok(path);
        }
    }
    Err(no_free_name(file_name))
}

async fn remove_stored(
    destination: &Destination,
    fs: &dyn Fs,
    stored: StoredImage,
    cx: &mut AsyncApp,
) -> Result<()> {
    if destination.is_local {
        let path = PathBuf::from(destination.absolute(&stored.project_path.path)?);
        return fs
            .remove_file(
                &path,
                RemoveOptions {
                    recursive: false,
                    ignore_if_not_exists: true,
                },
            )
            .await;
    }
    let worktree = destination
        .worktree
        .upgrade()
        .context("Markdown のあるワークツリーが閉じられました。")?;
    let entry_id = stored.entry_id.with_context(|| {
        format!(
            "リモートの画像を取り消せません: {}",
            stored.project_path.path.as_unix_str()
        )
    })?;
    let removal = worktree.update(cx, |worktree, cx| worktree.delete_entry(entry_id, cx));
    match removal {
        Some(removal) => removal.await,
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
