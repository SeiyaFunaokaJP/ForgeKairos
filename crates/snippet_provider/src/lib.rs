mod extension_snippet;
pub mod format;
mod registry;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::Result;
use collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use format::VsSnippetsFile;
use fs::Fs;
use futures::stream::StreamExt;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Task, WeakEntity};
pub use registry::*;
use util::ResultExt;

pub fn init(cx: &mut App) {
    SnippetRegistry::init_global(cx);
    extension_snippet::init(cx);
}

/// Language name, or `None` if the snippet file is global.
type SnippetKind = Option<String>;
fn file_stem_to_key(stem: &str) -> SnippetKind {
    if stem == "snippets" {
        None
    } else {
        Some(stem.to_owned())
    }
}

pub fn file_to_snippets(
    file_contents: VsSnippetsFile,
    source: &Path,
) -> impl Iterator<Item = Result<Arc<Snippet>>> {
    file_contents
        .snippets
        .into_iter()
        .map(move |(name, snippet)| {
            let snippet_name = name.clone();
            let prefixes = snippet
                .prefix
                .map_or_else(move || vec![snippet_name], |prefixes| prefixes.into());
            let description = snippet
                .description
                .map(|description| description.to_string());
            let body = snippet.body.to_string();
            match snippet::Snippet::parse(&body) {
                Ok(_) => Ok(Arc::new(Snippet {
                    body,
                    prefix: prefixes,
                    description,
                    name,
                })),
                Err(e) => Err(anyhow::anyhow!(
                    "Invalid snippet '{name}' in {source:?}: {e:#}"
                )),
            }
        })
}

/// The snippet files that ship with the editor, keyed by the language name
/// their file would have under the user's `snippets/` directory.
const BUILT_IN_SNIPPET_FILES: &[(&str, &str)] = &[(
    "markdown",
    include_str!("../../../assets/snippets/markdown.json"),
)];

/// The built-in snippets, parsed once and shared by every provider.
fn built_in_snippets() -> &'static HashMap<SnippetKind, Vec<Arc<Snippet>>> {
    static BUILT_IN_SNIPPETS: OnceLock<HashMap<SnippetKind, Vec<Arc<Snippet>>>> = OnceLock::new();
    BUILT_IN_SNIPPETS.get_or_init(|| {
        let mut snippets_by_kind: HashMap<SnippetKind, Vec<Arc<Snippet>>> = HashMap::default();
        for (name, contents) in BUILT_IN_SNIPPET_FILES {
            let source = PathBuf::from(format!("assets/snippets/{name}.json"));
            let Some(file_contents) =
                serde_json_lenient::from_str::<VsSnippetsFile>(contents).log_err()
            else {
                continue;
            };
            snippets_by_kind
                .entry(file_stem_to_key(name))
                .or_default()
                .extend(file_to_snippets(file_contents, &source).filter_map(Result::log_err));
        }
        snippets_by_kind
    })
}

// Snippet with all of the metadata
#[derive(Debug)]
pub struct Snippet {
    pub prefix: Vec<String>,
    pub body: String,
    pub description: Option<String>,
    pub name: String,
}

async fn process_updates(
    this: WeakEntity<SnippetProvider>,
    entries: Vec<PathBuf>,
    mut cx: AsyncApp,
) -> Result<()> {
    let fs = this.read_with(&cx, |this, _| this.fs.clone())?;
    for entry_path in entries {
        if entry_path
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let entry_metadata = fs.metadata(&entry_path).await;
        // Entry could have been removed, in which case we should no longer show completions for it.
        let entry_exists = entry_metadata.is_ok();
        if entry_metadata.is_ok_and(|entry| entry.is_some_and(|e| e.is_dir)) {
            // Don't process dirs.
            continue;
        }
        let Some(stem) = entry_path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let key = file_stem_to_key(stem);

        let contents = if entry_exists {
            fs.load(&entry_path).await.ok()
        } else {
            None
        };

        this.update(&mut cx, move |this, _| {
            let snippets_of_kind = this.snippets.entry(key).or_default();
            if entry_exists {
                let Some(file_contents) = contents else {
                    return;
                };
                let Ok(as_json) = serde_json_lenient::from_str::<VsSnippetsFile>(&file_contents)
                else {
                    return;
                };
                let snippets = file_to_snippets(as_json, entry_path.as_path());
                *snippets_of_kind.entry(entry_path).or_default() =
                    snippets.filter_map(Result::log_err).collect();
            } else {
                snippets_of_kind.remove(&entry_path);
            }
        })?;
    }
    Ok(())
}

async fn initial_scan(
    this: WeakEntity<SnippetProvider>,
    path: Arc<Path>,
    cx: AsyncApp,
) -> Result<()> {
    let fs = this.read_with(&cx, |this, _| this.fs.clone())?;
    let entries = fs.read_dir(&path).await;
    if let Ok(entries) = entries {
        let entries = entries
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        process_updates(this, entries, cx).await?;
    }
    Ok(())
}

pub struct SnippetProvider {
    fs: Arc<dyn Fs>,
    snippets: HashMap<SnippetKind, BTreeMap<PathBuf, Vec<Arc<Snippet>>>>,
    watch_tasks: Vec<Task<Result<()>>>,
}

// Watches global snippet directory, is created just once and reused across multiple projects
struct GlobalSnippetWatcher(Entity<SnippetProvider>);

impl GlobalSnippetWatcher {
    fn new(fs: Arc<dyn Fs>, cx: &mut App) -> Self {
        let global_snippets_dir = paths::snippets_dir();
        let provider = cx.new(|_cx| SnippetProvider {
            fs,
            snippets: Default::default(),
            watch_tasks: vec![],
        });
        provider.update(cx, |this, cx| this.watch_directory(global_snippets_dir, cx));
        Self(provider)
    }
}

impl gpui::Global for GlobalSnippetWatcher {}

impl SnippetProvider {
    pub fn new(fs: Arc<dyn Fs>, dirs_to_watch: BTreeSet<PathBuf>, cx: &mut App) -> Entity<Self> {
        cx.new(move |cx| {
            if !cx.has_global::<GlobalSnippetWatcher>() {
                let global_watcher = GlobalSnippetWatcher::new(fs.clone(), cx);
                cx.set_global(global_watcher);
            }
            let mut this = Self {
                fs,
                watch_tasks: Vec::new(),
                snippets: Default::default(),
            };

            for dir in dirs_to_watch {
                this.watch_directory(&dir, cx);
            }

            this
        })
    }

    /// Add directory to be watched for content changes
    fn watch_directory(&mut self, path: &Path, cx: &Context<Self>) {
        let path: Arc<Path> = Arc::from(path);

        self.watch_tasks.push(cx.spawn(async move |this, cx| {
            let fs = this.read_with(cx, |this, _| this.fs.clone())?;
            let watched_path = path.clone();
            let watcher = fs.watch(&watched_path, Duration::from_secs(1));
            initial_scan(this.clone(), path, cx.clone()).await?;

            let (mut entries, _) = watcher.await;
            while let Some(entries) = entries.next().await {
                process_updates(
                    this.clone(),
                    entries.into_iter().map(|event| event.path).collect(),
                    cx.clone(),
                )
                .await?;
            }
            Ok(())
        }));
    }

    fn lookup_snippets<'a, const LOOKUP_GLOBALS: bool>(
        &'a self,
        language: &'a SnippetKind,
        cx: &App,
    ) -> Vec<Arc<Snippet>> {
        let mut user_snippets: Vec<_> = self
            .snippets
            .get(language)
            .cloned()
            .unwrap_or_default()
            .into_values()
            .flat_map(|snippets| snippets.into_iter())
            .collect();
        if LOOKUP_GLOBALS {
            if let Some(global_watcher) = cx.try_global::<GlobalSnippetWatcher>() {
                user_snippets.extend(
                    global_watcher
                        .0
                        .read(cx)
                        .lookup_snippets::<false>(language, cx),
                );
            }

            let Some(registry) = SnippetRegistry::try_global(cx) else {
                return user_snippets;
            };

            let registry_snippets = registry.get_snippets(language);
            user_snippets.extend(registry_snippets);
        }

        user_snippets
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn add_snippet_for_test(
        &mut self,
        language: SnippetKind,
        path: PathBuf,
        snippet: Vec<Arc<Snippet>>,
    ) {
        self.snippets
            .entry(language)
            .or_default()
            .insert(path, snippet);
    }

    pub fn snippets_for(&self, language: SnippetKind, cx: &App) -> Vec<Arc<Snippet>> {
        let mut requested_snippets = self.lookup_snippets::<true>(&language, cx);

        if language.is_some() {
            // Look up global snippets as well.
            requested_snippets.extend(self.lookup_snippets::<true>(&None, cx));
        }

        // Built-in snippets come last and lose every prefix that a user or
        // extension snippet already claims, so redefining `mermaid-flowchart`
        // replaces the shipped one instead of showing two completions for it.
        let claimed_prefixes: HashSet<&str> = requested_snippets
            .iter()
            .flat_map(|snippet| snippet.prefix.iter().map(String::as_str))
            .collect();
        let built_in_snippets = built_in_snippets();
        let unclaimed_built_ins = built_in_snippets
            .get(&language)
            .into_iter()
            .chain(
                language
                    .is_some()
                    .then(|| built_in_snippets.get(&None))
                    .flatten(),
            )
            .flatten()
            .filter(|snippet| {
                !snippet
                    .prefix
                    .iter()
                    .any(|prefix| claimed_prefixes.contains(prefix.as_str()))
            })
            .cloned()
            .collect::<Vec<_>>();

        requested_snippets.extend(unclaimed_built_ins);
        requested_snippets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui;
    use gpui::TestAppContext;
    use indoc::indoc;

    #[gpui::test]
    fn test_lookup_snippets_dup_registry_snippets(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.background_executor.clone());
        cx.update(|cx| {
            SnippetRegistry::init_global(cx);
            SnippetRegistry::global(cx)
                .register_snippets(
                    "ruby".as_ref(),
                    indoc! {r#"
                    {
                      "Log to console": {
                        "prefix": "log",
                        "body": ["console.info(\"Hello, ${1:World}!\")", "$0"],
                        "description": "Logs to console"
                      }
                    }
            "#},
                )
                .unwrap();
            let provider = SnippetProvider::new(fs.clone(), Default::default(), cx);
            cx.update_entity(&provider, |provider, cx| {
                assert_eq!(1, provider.snippets_for(Some("ruby".to_owned()), cx).len());
            });
        });
    }

    #[gpui::test]
    fn test_built_in_snippets_are_offered(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.background_executor.clone());
        cx.update(|cx| {
            SnippetRegistry::init_global(cx);
            let provider = SnippetProvider::new(fs.clone(), Default::default(), cx);
            cx.update_entity(&provider, |provider, cx| {
                let snippets = provider.snippets_for(Some("markdown".to_owned()), cx);
                let prefixes = snippets
                    .iter()
                    .flat_map(|snippet| snippet.prefix.iter().map(String::as_str))
                    .collect::<Vec<_>>();
                for prefix in [
                    "mermaid-flowchart",
                    "mermaid-sequence",
                    "mermaid-class",
                    "mermaid-state",
                    "mermaid-er",
                    "mermaid-gantt",
                    "mermaid-pie",
                    "mermaid-gitgraph",
                    "mermaid-mindmap",
                    "mermaid-timeline",
                    "mermaid-journey",
                    "mermaid-quadrant",
                    "mermaid-xychart",
                ] {
                    assert!(
                        prefixes.contains(&prefix),
                        "built-in snippet {prefix} is missing from {prefixes:?}"
                    );
                }

                // Languages without a built-in file are unaffected.
                assert!(
                    provider
                        .snippets_for(Some("ruby".to_owned()), cx)
                        .is_empty()
                );
            });
        });
    }

    #[gpui::test]
    fn test_user_snippet_hides_built_in_with_same_prefix(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.background_executor.clone());
        cx.update(|cx| {
            SnippetRegistry::init_global(cx);
            let provider = SnippetProvider::new(fs.clone(), Default::default(), cx);
            cx.update_entity(&provider, |provider, cx| {
                provider.add_snippet_for_test(
                    Some("markdown".to_owned()),
                    PathBuf::from("markdown.json"),
                    vec![Arc::new(Snippet {
                        prefix: vec!["mermaid-flowchart".to_owned()],
                        body: "flowchart LR".to_owned(),
                        description: None,
                        name: "user flowchart".to_owned(),
                    })],
                );

                let snippets = provider.snippets_for(Some("markdown".to_owned()), cx);
                let flowchart_snippets = snippets
                    .iter()
                    .filter(|snippet| {
                        snippet
                            .prefix
                            .iter()
                            .any(|prefix| prefix == "mermaid-flowchart")
                    })
                    .collect::<Vec<_>>();
                assert_eq!(1, flowchart_snippets.len());
                assert_eq!("user flowchart", flowchart_snippets[0].name);

                // The other built-in snippets are still offered.
                assert!(
                    snippets
                        .iter()
                        .any(|snippet| snippet.name == "Mermaid シーケンス図")
                );
            });
        });
    }
}
