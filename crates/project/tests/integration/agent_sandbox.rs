//! ADR-F49 第 2a 段: an external agent's profile in the sandbox and its
//! per-project configuration folder.

use std::path::{Path, PathBuf};

use collections::HashMap;
use project::agent_sandbox::{
    AgentSandboxCommand, AgentSandboxHosts, AgentSandboxState, SandboxedAgentProfile,
    accept_remote_config_dir, accept_remote_hosts, agent_config_dir, claude_acp_host_config_dir,
    is_project_key, project_key, project_key_roots, readable_paths_in_data_dir,
};

#[test]
fn the_project_key_ignores_order_and_repeats() {
    let first = PathBuf::from("/work/a");
    let second = PathBuf::from("/work/b");
    let key = project_key(&[first.clone(), second.clone()]);
    assert!(is_project_key(&key), "{key:?}");
    assert_eq!(key, project_key(&[second.clone(), first.clone(), second]));
    assert_ne!(key, project_key(&[first]));
}

/// ADR-F49 決定 15 (MEDIUM-7): one definition of the folders a project's key
/// is taken from, so that the sandboxed Claude Code CLI, an agent in this
/// machine's sandbox and a remote host all name the same project alike: the
/// visible worktrees, a single file's by the folder holding it.
#[gpui::test]
async fn the_key_is_taken_from_the_same_folders_everywhere(cx: &mut gpui::TestAppContext) {
    crate::init_test(cx);
    let fs = fs::FakeFs::new(cx.executor());
    fs.insert_tree(
        util::path!("/work"),
        serde_json::json!({
            "project": { "src": { "main.rs": "" } },
            "notes": { "todo.md": "" },
        }),
    )
    .await;
    let project = project::Project::test(
        fs,
        [
            Path::new(util::path!("/work/project")),
            Path::new(util::path!("/work/notes/todo.md")),
        ],
        cx,
    )
    .await;

    let roots = project.read_with(cx, |project, cx| {
        project_key_roots(project.worktree_store().read(cx), cx)
    });
    let mut roots = roots;
    roots.sort();
    assert_eq!(
        roots,
        [
            PathBuf::from(util::path!("/work/notes")),
            PathBuf::from(util::path!("/work/project")),
        ],
        "a single file names the project by its folder"
    );
    assert_eq!(
        project_key(&roots),
        project_key(&[
            PathBuf::from(util::path!("/work/project")),
            PathBuf::from(util::path!("/work/notes")),
        ]),
        "the key does not depend on the order they were opened in"
    );
}

/// ADR-F49 MEDIUM-5: the folder a host says it placed the agent in is taken
/// only where this client's own policy for the agent names it.
#[test]
fn a_remote_folder_is_taken_only_where_the_client_names_it() {
    let claude = SandboxedAgentProfile::Claude;
    let accepted = "/home/u/.zed-jp-enhance-claude/projects/0123456789abcdef/claude-acp";
    assert_eq!(
        accept_remote_config_dir(accepted, &claude).expect("the folder the profile names"),
        PathBuf::from(accepted)
    );

    for refused in [
        // Another agent's folder, and with it its sign-in.
        "/home/u/.zed-jp-enhance-claude/projects/0123456789abcdef/codex",
        // ADR-F53: the sandboxed Claude Code CLI session's folder, which this
        // route may not read a subscription's sign-in out of.
        "/home/u/.zed-jp-enhance-claude/projects/0123456789abcdef/config",
        // Not a project's directory.
        "/home/u/.zed-jp-enhance-claude/projects/not-a-key/claude-acp",
        // Not below the sandboxed agents' projects directory.
        "/home/u/.ssh/claude-acp",
        "/home/u/projects/0123456789abcdef/claude-acp",
        // Walks out of it.
        "/home/u/.zed-jp-enhance-claude/projects/0123456789abcdef/../../../claude-acp",
        // Not absolute, and nothing at all.
        ".zed-jp-enhance-claude/projects/0123456789abcdef/claude-acp",
        "",
    ] {
        assert!(
            accept_remote_config_dir(refused, &claude).is_err(),
            "{refused} should be refused"
        );
    }

    assert!(
        accept_remote_config_dir(
            "/home/u/.zed-jp-enhance-claude/projects/0123456789abcdef/agent-my-agent",
            &SandboxedAgentProfile::Other {
                id: "my-agent".to_string()
            },
        )
        .is_ok()
    );
}

/// ADR-F49 決定 13 + MEDIUM-5: the hosts a remote host asks for are kept
/// within the client's policy for the agent.
#[test]
fn a_remote_agent_stays_within_the_clients_hosts() {
    let claude = SandboxedAgentProfile::Claude;
    assert_eq!(
        accept_remote_hosts(
            &claude,
            AgentSandboxHosts::Listed(vec![
                "api.anthropic.com".to_string(),
                "evil.example.com".to_string(),
            ]),
        ),
        AgentSandboxHosts::Listed(vec!["api.anthropic.com".to_string()]),
        "a host the profile does not name is dropped"
    );
    assert_eq!(
        accept_remote_hosts(&claude, AgentSandboxHosts::EditorAllowlist),
        AgentSandboxHosts::Listed(vec!["api.anthropic.com".to_string()]),
        "a vendor's agent does not get the whole editor allowlist for asking"
    );

    let custom = SandboxedAgentProfile::Other {
        id: "my-agent".to_string(),
    };
    assert_eq!(
        accept_remote_hosts(&custom, AgentSandboxHosts::Listed(vec!["x.example".into()])),
        AgentSandboxHosts::Listed(vec!["x.example".to_string()]),
        "an agent without a profile is capped by the editor allowlist anyway"
    );
    assert_eq!(
        accept_remote_hosts(&custom, AgentSandboxHosts::EditorAllowlist),
        AgentSandboxHosts::EditorAllowlist
    );
}

#[test]
fn only_registry_entries_get_a_vendor_profile() {
    assert_eq!(
        SandboxedAgentProfile::for_agent("claude-acp", true),
        SandboxedAgentProfile::Claude
    );
    assert_eq!(
        SandboxedAgentProfile::for_agent("codex-acp", true),
        SandboxedAgentProfile::Codex
    );
    assert_eq!(
        SandboxedAgentProfile::for_agent("gemini", true),
        SandboxedAgentProfile::Gemini
    );
    assert_eq!(
        SandboxedAgentProfile::for_agent("gemini", false),
        SandboxedAgentProfile::Other {
            id: "gemini".to_string()
        },
        "a command the user configured under a vendor's name could be anything"
    );
}

#[test]
fn each_agent_has_its_own_folder() {
    let names = [
        SandboxedAgentProfile::Claude,
        SandboxedAgentProfile::Codex,
        SandboxedAgentProfile::Gemini,
        SandboxedAgentProfile::Other {
            id: "my-agent".to_string(),
        },
    ]
    .map(|profile| profile.config_folder_name());
    // ADR-F53: Claude's is no longer the sandboxed CLI session's `config`, so
    // a subscription the CLI signed in with is not on this route.
    assert_eq!(names, ["claude-acp", "codex", "gemini", "agent-my-agent"]);

    let unsafe_name = SandboxedAgentProfile::Other {
        id: "../x/y".to_string(),
    }
    .config_folder_name();
    assert!(unsafe_name.starts_with("agent-.._x_y-"), "{unsafe_name}");
    assert!(!unsafe_name.contains('/'), "{unsafe_name}");
    assert_ne!(
        unsafe_name,
        SandboxedAgentProfile::Other {
            id: "../x:y".to_string()
        }
        .config_folder_name(),
        "ids that read alike keep apart"
    );
}

#[test]
fn the_folder_is_below_the_projects_directory() {
    let home = Path::new("/home/user");
    assert_eq!(
        agent_config_dir(home, "0123456789abcdef", &SandboxedAgentProfile::Codex),
        home.join(".zed-jp-enhance-claude")
            .join("projects")
            .join("0123456789abcdef")
            .join("codex")
    );
}

/// ADR-F53: the ACP Claude agent may only be used with an API key, so the
/// folder it keeps its state in is never one the official Claude Code CLI
/// signs in to: not the account's `~/.claude`, and not the sandboxed CLI
/// session's per-project `config` folder, which lives below the account's home
/// among the sandboxed agents' projects.
#[test]
fn the_acp_claude_never_shares_the_clis_folder() {
    let home = Path::new("/home/user");
    let key = "0123456789abcdef";
    let sandboxed = agent_config_dir(home, key, &SandboxedAgentProfile::Claude);
    let cli_session = home
        .join(".zed-jp-enhance-claude")
        .join("projects")
        .join(key)
        .join("config");
    assert_ne!(sandboxed, cli_session);
    assert_eq!(sandboxed.parent(), cli_session.parent());

    let host = claude_acp_host_config_dir();
    assert!(
        host.starts_with(paths::external_agents_dir()),
        "the host route's folder is one this fork manages: {host:?}"
    );
    assert_ne!(host, paths::home_dir().join(".claude"));
    assert!(
        !host.starts_with(paths::home_dir().join(".zed-jp-enhance-claude")),
        "nor the sandboxed agents' projects directory: {host:?}"
    );
}

fn environment_of(profile: SandboxedAgentProfile) -> HashMap<String, String> {
    profile.environment(Path::new("/c")).into_iter().collect()
}

#[test]
fn each_profile_keeps_the_agent_in_its_folder() {
    let home = Path::new("/c").join("home").to_string_lossy().into_owned();

    let claude = environment_of(SandboxedAgentProfile::Claude);
    assert_eq!(claude["CLAUDE_CONFIG_DIR"], "/c");
    assert_eq!(claude["HOME"], home);
    // ADR-F53: no sign-in of the agent's own is offered on this route, so it
    // is not pointed at the paste-a-code flow either.
    assert!(!claude.contains_key("NO_BROWSER"), "{claude:?}");

    let codex = environment_of(SandboxedAgentProfile::Codex);
    assert_eq!(
        codex["CODEX_HOME"],
        Path::new("/c")
            .join("home")
            .join(".codex")
            .to_string_lossy()
    );
    assert_eq!(codex["HOME"], home);
    assert_eq!(codex["NO_BROWSER"], "1");

    let gemini = environment_of(SandboxedAgentProfile::Gemini);
    assert_eq!(gemini["GEMINI_CLI_HOME"], home);
    assert_eq!(gemini["HOME"], home);
    assert_eq!(gemini["NO_BROWSER"], "1");

    let other = environment_of(SandboxedAgentProfile::Other {
        id: "x".to_string(),
    });
    assert_eq!(other.len(), 1);
    assert_eq!(other["HOME"], home);
}

#[test]
fn vendors_have_their_hosts_and_others_the_allowlist() {
    // ADR-F53: `platform.claude.com` is the OAuth token exchange, which only a
    // subscription sign-in needs, and this route is API-key only.
    assert_eq!(
        SandboxedAgentProfile::Claude.hosts(),
        Some(&["api.anthropic.com"][..])
    );
    assert_eq!(
        SandboxedAgentProfile::Codex.hosts(),
        Some(&["api.openai.com", "chatgpt.com", "auth.openai.com"][..])
    );
    assert!(
        SandboxedAgentProfile::Gemini
            .hosts()
            .is_some_and(|hosts| hosts.contains(&"cloudcode-pa.googleapis.com"))
    );
    assert_eq!(
        SandboxedAgentProfile::Other { id: "x".into() }.hosts(),
        None
    );
}

#[test]
fn only_paths_in_the_data_directory_are_shown_back() {
    let data = PathBuf::from("/data/zed");
    let install = data.join("external_agents/registry/npx/gemini");
    let node = data.join("node");
    assert_eq!(
        readable_paths_in_data_dir(
            &data,
            &[
                install.clone(),
                PathBuf::from("/usr/bin/node"),
                node.clone(),
                install.clone()
            ]
        ),
        [install, node]
    );
}

#[test]
fn the_profile_environment_wins_over_the_agents_own() {
    let home = tempfile::tempdir().expect("tempdir");
    let command = AgentSandboxCommand::new(
        SandboxedAgentProfile::Claude,
        HashMap::from_iter([
            ("CLAUDE_CONFIG_DIR".to_string(), "/elsewhere".to_string()),
            ("KEEP".to_string(), "1".to_string()),
        ]),
        Vec::new(),
    );
    let result = command.resolve_for_project(home.path(), &[PathBuf::from("/work/a")]);
    if cfg!(unix) {
        let resolved = result.expect("placed in the project");
        let config_dir = resolved.config_dir().expect("placed").to_path_buf();
        assert_eq!(
            resolved.env["CLAUDE_CONFIG_DIR"],
            config_dir.to_string_lossy()
        );
        assert_eq!(resolved.env["KEEP"], "1");
        assert!(matches!(
            resolved.state,
            AgentSandboxState::Resolved {
                hosts: AgentSandboxHosts::Listed(_),
                ..
            }
        ));
    } else {
        assert!(result.is_err(), "nothing is created on this OS");
    }
}

#[test]
fn a_project_without_folders_has_no_place_for_an_agent() {
    let home = tempfile::tempdir().expect("tempdir");
    let command =
        AgentSandboxCommand::new(SandboxedAgentProfile::Codex, HashMap::default(), Vec::new());
    assert!(command.resolve_for_project(home.path(), &[]).is_err());
}

#[cfg(unix)]
mod creation {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use project::agent_sandbox::{
        SandboxedAgentProfile, agent_config_dir, ensure_agent_config_dir,
    };

    const KEY: &str = "0123456789abcdef";

    #[test]
    fn creates_every_level_owner_only() {
        let home = tempfile::tempdir().expect("tempdir");
        let config_dir = ensure_agent_config_dir(home.path(), KEY, &SandboxedAgentProfile::Codex)
            .expect("created");
        assert_eq!(
            config_dir,
            agent_config_dir(home.path(), KEY, &SandboxedAgentProfile::Codex)
        );
        let mut level = config_dir.clone();
        while level != home.path() {
            let mode = std::fs::metadata(&level)
                .expect("exists")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{}", level.display());
            level.pop();
        }
        assert!(config_dir.join("home/.codex").is_dir());
        assert!(config_dir.join(".mcp.json").is_file());
        assert!(config_dir.join(".claude").is_dir());

        ensure_agent_config_dir(home.path(), KEY, &SandboxedAgentProfile::Codex)
            .expect("a second call finds it all in place");
    }

    #[test]
    fn refuses_a_link_on_the_way() {
        let home = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let projects = home.path().join(".zed-jp-enhance-claude/projects");
        std::fs::create_dir_all(&projects).expect("projects");
        symlink(elsewhere.path(), projects.join(KEY)).expect("link");

        let error = ensure_agent_config_dir(home.path(), KEY, &SandboxedAgentProfile::Gemini)
            .expect_err("a linked level is refused");
        assert!(
            format!("{error:#}").contains("リンクではなく実体のフォルダー"),
            "{error:#}"
        );
        assert!(
            std::fs::read_dir(elsewhere.path())
                .expect("readable")
                .next()
                .is_none(),
            "nothing is created through the link"
        );
    }

    #[test]
    fn refuses_the_folder_itself_as_a_link() {
        let home = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let project = home
            .path()
            .join(".zed-jp-enhance-claude/projects")
            .join(KEY);
        std::fs::create_dir_all(&project).expect("project");
        symlink(elsewhere.path(), project.join("config")).expect("link");

        assert!(ensure_agent_config_dir(home.path(), KEY, &SandboxedAgentProfile::Claude).is_err());
    }

    #[test]
    fn refuses_an_invalid_key() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(
            ensure_agent_config_dir(home.path(), "../escape", &SandboxedAgentProfile::Claude)
                .is_err()
        );
    }

    /// The folder holds the agent's credentials and the hooks it runs, so a
    /// level another account could write is refused — its group included.
    #[test]
    fn refuses_a_level_other_accounts_can_write() {
        let home = tempfile::tempdir().expect("tempdir");
        let projects = home.path().join(".zed-jp-enhance-claude/projects");
        std::fs::create_dir_all(&projects).expect("projects");
        std::fs::set_permissions(&projects, std::fs::Permissions::from_mode(0o770))
            .expect("permissions");

        let error = ensure_agent_config_dir(home.path(), KEY, &SandboxedAgentProfile::Claude)
            .expect_err("a group-writable level is refused");
        assert!(
            format!("{error:#}").contains("他のアカウントから書き換えられる"),
            "{error:#}"
        );

        std::fs::set_permissions(&projects, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        ensure_agent_config_dir(home.path(), KEY, &SandboxedAgentProfile::Claude)
            .expect("owner-only again");
    }

    /// The protected names are bound read-only in the sandbox, so what is
    /// already there under one of them has to be a real file, examined
    /// without following it.
    #[test]
    fn refuses_a_protected_name_that_is_not_a_regular_file() {
        let home = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let config = agent_config_dir(home.path(), KEY, &SandboxedAgentProfile::Claude);
        std::fs::create_dir_all(&config).expect("config");
        symlink(
            elsewhere.path().join("stolen.json"),
            config.join(".mcp.json"),
        )
        .expect("link");

        let error = ensure_agent_config_dir(home.path(), KEY, &SandboxedAgentProfile::Claude)
            .expect_err("a link is refused");
        assert!(
            format!("{error:#}").contains("通常のファイルではありません"),
            "{error:#}"
        );
        assert!(
            !elsewhere.path().join("stolen.json").exists(),
            "nothing is created through the link"
        );
    }
}
