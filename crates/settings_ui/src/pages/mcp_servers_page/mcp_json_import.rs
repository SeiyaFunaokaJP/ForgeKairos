use gpui::SharedString;
use serde_json::{Map, Value};

/// Keys other clients use for a remote server's endpoint, in lookup order
/// (`serverUrl` is Windsurf's, `httpUrl` Gemini CLI's).
const URL_KEYS: [&str; 3] = ["url", "serverUrl", "httpUrl"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ImportedServer {
    /// Empty when the snippet was a single server object with no name around it.
    pub(super) name: String,
    pub(super) definition: ImportedDefinition,
    /// Only Zed's own format can say `false`; it is kept rather than quietly
    /// turned into a server that starts.
    pub(super) enabled: bool,
    /// Keys of the server object that Zed has no equivalent for, in snippet order.
    pub(super) ignored_keys: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ImportedDefinition {
    Local {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        timeout: Option<u64>,
        remote: bool,
    },
    Remote {
        url: String,
        headers: Vec<(String, String)>,
        timeout: Option<u64>,
        oauth_client_id: Option<String>,
        oauth_client_secret: Option<String>,
    },
}

impl ImportedServer {
    pub(super) fn display_name(&self) -> &str {
        display_name(&self.name)
    }
}

fn display_name(name: &str) -> &str {
    if name.trim().is_empty() {
        i18n::t!("(unnamed)")
    } else {
        name
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dialect {
    /// Zed's own `context_servers`. Only here is `timeout` known to be in
    /// seconds (Gemini CLI, for one, counts milliseconds), and only here do
    /// `remote` and `oauth` mean what the form means by them.
    Zed,
    Other,
}

/// Reads the snippet formats MCP server READMEs publish: `mcpServers` (Claude
/// Desktop, Cursor, Windsurf, Cline), `servers` / `mcp.servers` (VS Code),
/// Zed's `context_servers`, a bare `{ "name": { … } }` map, or one server
/// object on its own.
pub(super) fn parse_mcp_json_snippet(text: &str) -> Result<Vec<ImportedServer>, SharedString> {
    let text = strip_code_fence(text.trim());
    if text.is_empty() {
        return Err(i18n::t!("Paste some JSON.").into());
    }
    let Value::Object(object) = parse_lenient(text)? else {
        return Err(i18n::t!("Paste a JSON object ({{ … }}).").into());
    };
    let (entries, dialect) = locate_servers(object)?;
    entries
        .into_iter()
        .map(|(name, value)| {
            let Value::Object(server) = value else {
                let name = display_name(&name);
                return Err(i18n::t!("\"{name}\": the value is not an object ({{ … }}).").into());
            };
            parse_server(name, server, dialect)
        })
        .collect()
}

fn not_found() -> SharedString {
    i18n::t!(
        "No MCP server settings were found. Paste JSON that contains `command` (local) or `url` (remote)."
    )
    .into()
}

/// README snippets are often copied together with the Markdown fence around them.
fn strip_code_fence(text: &str) -> &str {
    let Some(fenced) = text.strip_prefix("```") else {
        return text;
    };
    // The opening line may carry an info string such as `json`.
    let Some((_, body)) = fenced.split_once('\n') else {
        return text;
    };
    let body = body.trim_end();
    body.strip_suffix("```").unwrap_or(body).trim()
}

fn parse_lenient(text: &str) -> Result<Value, SharedString> {
    let error = match settings_json::parse_json_with_comments::<Value>(text) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    // A fragment cut out of a larger file (`"github": { … }`) has no braces of
    // its own.
    if text.starts_with('"')
        && let Ok(value) = settings_json::parse_json_with_comments::<Value>(&format!("{{{text}}}"))
    {
        return Ok(value);
    }
    Err(i18n::t!("Couldn't read this as JSON: {error}").into())
}

fn looks_like_server(object: &Map<String, Value>) -> bool {
    object.contains_key("command") || URL_KEYS.iter().any(|key| object.contains_key(*key))
}

fn locate_servers(
    object: Map<String, Value>,
) -> Result<(Vec<(String, Value)>, Dialect), SharedString> {
    let containers = [
        ("mcpServers", Dialect::Other),
        ("context_servers", Dialect::Zed),
        ("servers", Dialect::Other),
    ];
    for (key, dialect) in containers {
        // A server that happens to be named like a container is still a server.
        if let Some(container) = object.get(key)
            && !container.as_object().is_some_and(looks_like_server)
        {
            return server_entries(key, container, dialect);
        }
    }
    if let Some(container) = object.get("mcp").and_then(|mcp| mcp.get("servers")) {
        return server_entries("mcp.servers", container, Dialect::Other);
    }
    if looks_like_server(&object) {
        return Ok((vec![(String::new(), Value::Object(object))], Dialect::Other));
    }
    if !object
        .values()
        .any(|value| value.as_object().is_some_and(looks_like_server))
    {
        return Err(not_found());
    }
    Ok((object.into_iter().collect(), Dialect::Other))
}

fn server_entries(
    key: &str,
    container: &Value,
    dialect: Dialect,
) -> Result<(Vec<(String, Value)>, Dialect), SharedString> {
    let Value::Object(servers) = container else {
        return Err(i18n::t!("The value of `{key}` is not an object ({{ … }}).").into());
    };
    if servers.is_empty() {
        return Err(not_found());
    }
    Ok((
        servers
            .iter()
            .map(|(name, server)| (name.clone(), server.clone()))
            .collect(),
        dialect,
    ))
}

fn parse_server(
    name: String,
    mut server: Map<String, Value>,
    dialect: Dialect,
) -> Result<ImportedServer, SharedString> {
    let label = display_name(&name).to_string();
    let is_remote = match server.shift_remove("type") {
        None => {
            !server.contains_key("command") && URL_KEYS.iter().any(|key| server.contains_key(*key))
        }
        Some(Value::String(kind)) => match kind.as_str() {
            "stdio" => false,
            "http" | "sse" | "streamable-http" | "streamable_http" | "streamableHttp" => true,
            _ => {
                return Err(i18n::t!(
                    "\"{label}\": `type` \"{kind}\" is not supported. Use `stdio`, `http`, or `sse`."
                )
                .into());
            }
        },
        Some(_) => return Err(i18n::t!("\"{label}\": `type` must be a string.").into()),
    };

    let definition = if is_remote {
        parse_remote(&label, &mut server, dialect)?
    } else {
        parse_local(&label, &mut server, dialect)?
    };

    let enabled = match dialect {
        Dialect::Zed => {
            // `source` only marked a custom server in an older Zed format.
            server.shift_remove("source");
            match server.shift_remove("enabled") {
                None | Some(Value::Null) => true,
                Some(Value::Bool(enabled)) => enabled,
                Some(_) => {
                    return Err(i18n::t!("\"{label}\": `enabled` must be true or false.").into());
                }
            }
        }
        Dialect::Other => true,
    };

    Ok(ImportedServer {
        name,
        definition,
        enabled,
        ignored_keys: server.keys().cloned().collect(),
    })
}

fn parse_local(
    label: &str,
    server: &mut Map<String, Value>,
    dialect: Dialect,
) -> Result<ImportedDefinition, SharedString> {
    let (command, mut args, mut env) = match server.shift_remove("command") {
        Some(Value::String(command)) => (command, Vec::new(), Vec::new()),
        // Zed's earlier format nested the whole launch under `command`.
        Some(Value::Object(mut nested)) if nested.contains_key("path") => {
            let Some(Value::String(path)) = nested.shift_remove("path") else {
                return Err(i18n::t!("\"{label}\": `command.path` must be a string.").into());
            };
            let args = take_args(&mut nested, label)?;
            let env = take_string_map(&mut nested, "env", label)?;
            (path, args, env)
        }
        Some(_) => return Err(i18n::t!("\"{label}\": `command` must be a string.").into()),
        None => {
            return Err(i18n::t!("\"{label}\": neither `command` nor `url` is set.").into());
        }
    };
    if command.trim().is_empty() {
        return Err(i18n::t!("\"{label}\": `command` is empty.").into());
    }
    if server.contains_key("args") {
        args = take_args(server, label)?;
    }
    if server.contains_key("env") {
        env = take_string_map(server, "env", label)?;
    }
    let (timeout, remote) = match dialect {
        Dialect::Zed => (take_timeout(server, label)?, take_remote(server, label)?),
        Dialect::Other => (None, false),
    };
    Ok(ImportedDefinition::Local {
        command,
        args,
        env,
        timeout,
        remote,
    })
}

fn parse_remote(
    label: &str,
    server: &mut Map<String, Value>,
    dialect: Dialect,
) -> Result<ImportedDefinition, SharedString> {
    let Some(key) = URL_KEYS.into_iter().find(|key| server.contains_key(*key)) else {
        return Err(i18n::t!("\"{label}\": `url` is not set.").into());
    };
    let url = match server.shift_remove(key) {
        Some(Value::String(url)) if !url.trim().is_empty() => url,
        _ => {
            return Err(i18n::t!("\"{label}\": `{key}` must be a URL string.").into());
        }
    };
    let headers = take_string_map(server, "headers", label)?;
    let (timeout, oauth_client_id, oauth_client_secret) = match dialect {
        Dialect::Zed => {
            let timeout = take_timeout(server, label)?;
            let (client_id, client_secret) = take_oauth(server, label)?;
            (timeout, client_id, client_secret)
        }
        Dialect::Other => (None, None, None),
    };
    Ok(ImportedDefinition::Remote {
        url,
        headers,
        timeout,
        oauth_client_id,
        oauth_client_secret,
    })
}

/// JSON numbers and booleans are accepted where a string is expected, since
/// snippets write `"--port", 3000` and `"DEBUG": true` as often as not.
fn scalar_text(value: Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn take_args(server: &mut Map<String, Value>, label: &str) -> Result<Vec<String>, SharedString> {
    let invalid =
        || SharedString::from(i18n::t!("\"{label}\": `args` must be an array of strings."));
    match server.shift_remove("args") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|item| scalar_text(item).ok_or_else(invalid))
            .collect(),
        Some(_) => Err(invalid()),
    }
}

fn take_string_map(
    server: &mut Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<Vec<(String, String)>, SharedString> {
    match server.shift_remove(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(entries)) => entries
            .into_iter()
            .map(|(name, value)| match scalar_text(value) {
                Some(text) => Ok((name, text)),
                None => {
                    Err(i18n::t!("\"{label}\": \"{name}\" in `{key}` must be a string.").into())
                }
            })
            .collect(),
        Some(_) => Err(i18n::t!("\"{label}\": `{key}` must be an object ({{ … }}).").into()),
    }
}

fn take_timeout(server: &mut Map<String, Value>, label: &str) -> Result<Option<u64>, SharedString> {
    match server.shift_remove("timeout") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) if number.as_u64().is_some() => Ok(number.as_u64()),
        Some(_) => Err(i18n::t!(
            "\"{label}\": `timeout` must be a positive whole number of seconds."
        )
        .into()),
    }
}

fn take_remote(server: &mut Map<String, Value>, label: &str) -> Result<bool, SharedString> {
    match server.shift_remove("remote") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(remote)) => Ok(remote),
        Some(_) => Err(i18n::t!("\"{label}\": `remote` must be true or false.").into()),
    }
}

fn take_oauth(
    server: &mut Map<String, Value>,
    label: &str,
) -> Result<(Option<String>, Option<String>), SharedString> {
    let invalid = || {
        SharedString::from(i18n::t!(
            "\"{label}\": `oauth` must look like {{ \"client_id\": \"…\" }}."
        ))
    };
    match server.shift_remove("oauth") {
        None | Some(Value::Null) => Ok((None, None)),
        Some(Value::Object(mut oauth)) => {
            let Some(Value::String(client_id)) = oauth.shift_remove("client_id") else {
                return Err(invalid());
            };
            let client_secret = match oauth.shift_remove("client_secret") {
                None | Some(Value::Null) => None,
                Some(Value::String(secret)) => Some(secret),
                Some(_) => return Err(invalid()),
            };
            Ok((Some(client_id), client_secret))
        }
        Some(_) => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn local(command: &str, args: &[&str], env: &[(&str, &str)]) -> ImportedDefinition {
        ImportedDefinition::Local {
            command: command.into(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            env: env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            timeout: None,
            remote: false,
        }
    }

    fn remote(url: &str, headers: &[(&str, &str)]) -> ImportedDefinition {
        ImportedDefinition::Remote {
            url: url.into(),
            headers: headers
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            timeout: None,
            oauth_client_id: None,
            oauth_client_secret: None,
        }
    }

    fn server(name: &str, definition: ImportedDefinition) -> ImportedServer {
        ImportedServer {
            name: name.into(),
            definition,
            enabled: true,
            ignored_keys: Vec::new(),
        }
    }

    fn parse(text: &str) -> Vec<ImportedServer> {
        parse_mcp_json_snippet(text).unwrap_or_else(|error| panic!("{text}\n→ {error}"))
    }

    fn parse_error(text: &str) -> String {
        match parse_mcp_json_snippet(text) {
            Ok(servers) => panic!("expected an error for {text}, got {servers:?}"),
            Err(error) => error.to_string(),
        }
    }

    /// The filesystem server's README (Claude Desktop), with the Windows paths
    /// it shows: backslashes must come out as single backslashes.
    #[test]
    fn reads_claude_desktop_snippet_with_windows_paths() {
        let servers = parse(
            r#"{
              "mcpServers": {
                "filesystem": {
                  "command": "npx",
                  "args": [
                    "-y",
                    "@modelcontextprotocol/server-filesystem",
                    "C:\\Users\\username\\Desktop",
                    "C:\\Users\\username\\My Documents"
                  ]
                }
              }
            }"#,
        );
        assert_eq!(
            servers,
            vec![server(
                "filesystem",
                local(
                    "npx",
                    &[
                        "-y",
                        "@modelcontextprotocol/server-filesystem",
                        r"C:\Users\username\Desktop",
                        r"C:\Users\username\My Documents",
                    ],
                    &[],
                ),
            )]
        );
    }

    /// The GitHub MCP server's README (Docker, with a token placeholder in env).
    #[test]
    fn reads_env_from_a_docker_snippet() {
        let servers = parse(
            r#"{
              "mcpServers": {
                "github": {
                  "command": "docker",
                  "args": ["run", "-i", "--rm", "-e", "GITHUB_PERSONAL_ACCESS_TOKEN", "ghcr.io/github/github-mcp-server"],
                  "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "<YOUR_TOKEN>" }
                }
              }
            }"#,
        );
        assert_eq!(
            servers,
            vec![server(
                "github",
                local(
                    "docker",
                    &[
                        "run",
                        "-i",
                        "--rm",
                        "-e",
                        "GITHUB_PERSONAL_ACCESS_TOKEN",
                        "ghcr.io/github/github-mcp-server",
                    ],
                    &[("GITHUB_PERSONAL_ACCESS_TOKEN", "<YOUR_TOKEN>")],
                ),
            )]
        );
    }

    /// VS Code's `.vscode/mcp.json`: typed servers next to an `inputs` list.
    #[test]
    fn reads_vs_code_mcp_json() {
        let servers = parse(
            r#"{
              "inputs": [
                { "type": "promptString", "id": "github_token", "description": "GitHub Personal Access Token", "password": true }
              ],
              "servers": {
                "github": {
                  "type": "http",
                  "url": "https://api.githubcopilot.com/mcp/",
                  "headers": { "Authorization": "Bearer ${input:github_token}" }
                },
                "fetch": { "type": "stdio", "command": "uvx", "args": ["mcp-server-fetch"] }
              }
            }"#,
        );
        assert_eq!(
            servers,
            vec![
                server(
                    "github",
                    remote(
                        "https://api.githubcopilot.com/mcp/",
                        &[("Authorization", "Bearer ${input:github_token}")],
                    ),
                ),
                server("fetch", local("uvx", &["mcp-server-fetch"], &[])),
            ]
        );
    }

    /// VS Code's user `settings.json`, with the comments and trailing commas it
    /// allows.
    #[test]
    fn reads_vs_code_settings_with_comments_and_trailing_commas() {
        let servers = parse(
            r#"// settings.json
            {
              "editor.fontSize": 14,
              "mcp": {
                "servers": {
                  "memory": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-memory",], // trailing comma
                  },
                },
              },
            }"#,
        );
        assert_eq!(
            servers,
            vec![server(
                "memory",
                local("npx", &["-y", "@modelcontextprotocol/server-memory"], &[]),
            )]
        );
    }

    /// Context7's Cursor snippet (remote, with an API key header) and Windsurf's
    /// `serverUrl` / Gemini CLI's `httpUrl` spellings of the URL.
    #[test]
    fn reads_remote_servers_under_every_url_key() {
        let servers = parse(
            r#"{
              "mcpServers": {
                "context7": {
                  "url": "https://mcp.context7.com/mcp",
                  "headers": { "CONTEXT7_API_KEY": "YOUR_API_KEY" }
                },
                "windsurf": { "serverUrl": "https://example.com/sse" },
                "gemini": { "httpUrl": "https://example.com/mcp" },
                "legacy-sse": { "type": "sse", "url": "http://localhost:3001/sse" }
              }
            }"#,
        );
        assert_eq!(
            servers,
            vec![
                server(
                    "context7",
                    remote(
                        "https://mcp.context7.com/mcp",
                        &[("CONTEXT7_API_KEY", "YOUR_API_KEY")],
                    ),
                ),
                server("windsurf", remote("https://example.com/sse", &[])),
                server("gemini", remote("https://example.com/mcp", &[])),
                server("legacy-sse", remote("http://localhost:3001/sse", &[])),
            ]
        );
    }

    #[test]
    fn reads_zed_context_servers_including_seconds_and_execution_host() {
        let servers = parse(
            r#"{
              "context_servers": {
                "example-mcp": {
                  "enabled": true,
                  "command": "C:\\tools\\example-mcp\\.venv\\Scripts\\example-mcp.exe",
                  "args": ["--stdio"],
                  "timeout": 120,
                  "remote": true
                },
                "figma": {
                  "url": "https://mcp.figma.com/mcp",
                  "timeout": 30,
                  "oauth": { "client_id": "client-id", "client_secret": "client-secret" }
                },
                // Zed's earlier format, with the launch nested under `command`.
                "legacy": {
                  "source": "custom",
                  "command": { "path": "node", "args": ["server.js"], "env": { "A": "1" } }
                },
                "parked": { "enabled": false, "command": "uvx", "args": ["mcp-server-time"] }
              }
            }"#,
        );
        let mut parked = server("parked", local("uvx", &["mcp-server-time"], &[]));
        parked.enabled = false;
        assert_eq!(
            servers,
            vec![
                server(
                    "example-mcp",
                    ImportedDefinition::Local {
                        command: r"C:\tools\example-mcp\.venv\Scripts\example-mcp.exe".into(),
                        args: vec!["--stdio".into()],
                        env: Vec::new(),
                        timeout: Some(120),
                        remote: true,
                    },
                ),
                server(
                    "figma",
                    ImportedDefinition::Remote {
                        url: "https://mcp.figma.com/mcp".into(),
                        headers: Vec::new(),
                        timeout: Some(30),
                        oauth_client_id: Some("client-id".into()),
                        oauth_client_secret: Some("client-secret".into()),
                    },
                ),
                server("legacy", local("node", &["server.js"], &[("A", "1")])),
                parked,
            ]
        );
    }

    /// Cline's format carries keys Zed has no use for; they are reported rather
    /// than silently lost, and a timeout in someone else's units is not taken.
    #[test]
    fn reports_keys_it_does_not_carry_over() {
        let servers = parse(
            r#"{
              "mcpServers": {
                "weather": {
                  "command": "node",
                  "args": ["build/index.js"],
                  "disabled": false,
                  "autoApprove": [],
                  "timeout": 60,
                  "cwd": "C:\\weather"
                }
              }
            }"#,
        );
        assert_eq!(
            servers,
            vec![ImportedServer {
                name: "weather".into(),
                definition: local("node", &["build/index.js"], &[]),
                enabled: true,
                ignored_keys: vec![
                    "disabled".into(),
                    "autoApprove".into(),
                    "timeout".into(),
                    "cwd".into(),
                ],
            }]
        );
    }

    #[test]
    fn reads_a_bare_map_a_lone_server_and_a_fragment() {
        assert_eq!(
            parse(r#"{ "time": { "command": "uvx", "args": ["mcp-server-time"] } }"#),
            vec![server("time", local("uvx", &["mcp-server-time"], &[]))]
        );
        assert_eq!(
            parse(r#"{ "command": "python", "args": ["-m", "my_server"] }"#),
            vec![server("", local("python", &["-m", "my_server"], &[]))]
        );
        // Copied out of the middle of a README block, trailing comma included.
        assert_eq!(
            parse(
                r#""sqlite": { "command": "uvx", "args": ["mcp-server-sqlite", "--db-path", "test.db"] },"#
            ),
            vec![server(
                "sqlite",
                local("uvx", &["mcp-server-sqlite", "--db-path", "test.db"], &[]),
            )]
        );
    }

    #[test]
    fn a_server_named_like_a_container_is_still_a_server() {
        assert_eq!(
            parse(r#"{ "servers": { "command": "servers-mcp" } }"#),
            vec![server("servers", local("servers-mcp", &[], &[]))]
        );
    }

    #[test]
    fn strips_a_markdown_code_fence() {
        assert_eq!(
            parse("```json\n{ \"mcpServers\": { \"time\": { \"command\": \"uvx\" } } }\n```"),
            vec![server("time", local("uvx", &[], &[]))]
        );
    }

    #[test]
    fn accepts_numbers_and_booleans_where_strings_are_expected() {
        assert_eq!(
            parse(
                r#"{ "mcpServers": { "api": { "command": "node", "args": ["--port", 3000], "env": { "DEBUG": true } } } }"#
            ),
            vec![server(
                "api",
                local("node", &["--port", "3000"], &[("DEBUG", "true")]),
            )]
        );
    }

    #[test]
    fn reports_invalid_input() {
        assert_eq!(parse_error("   "), "Paste some JSON.");
        assert!(
            parse_error(r#"{ "mcpServers": "#).starts_with("Couldn't read this as JSON: "),
            "an unfinished object should be reported as unreadable"
        );
        assert_eq!(parse_error(r#"["npx"]"#), "Paste a JSON object ({ … }).");
        assert_eq!(
            parse_error(r#"{ "theme": "One Dark" }"#),
            not_found().to_string()
        );
        assert_eq!(
            parse_error(r#"{ "mcpServers": {} }"#),
            not_found().to_string()
        );
        assert_eq!(
            parse_error(r#"{ "mcpServers": { "broken": { "args": ["x"] } } }"#),
            "\"broken\": neither `command` nor `url` is set."
        );
        assert_eq!(
            parse_error(r#"{ "mcpServers": { "x": { "command": "node", "args": "a b" } } }"#),
            "\"x\": `args` must be an array of strings."
        );
        assert_eq!(
            parse_error(r#"{ "mcpServers": { "x": { "command": "node", "env": ["A=1"] } } }"#),
            "\"x\": `env` must be an object ({ … })."
        );
        assert_eq!(
            parse_error(r#"{ "servers": { "x": { "type": "websocket", "url": "ws://x" } } }"#),
            "\"x\": `type` \"websocket\" is not supported. Use `stdio`, `http`, or `sse`."
        );
        assert_eq!(
            parse_error(r#"{ "mcpServers": { "x": { "command": "  " } } }"#),
            "\"x\": `command` is empty."
        );
        assert_eq!(
            parse_error(r#"{ "command": 42 }"#),
            "\"(unnamed)\": `command` must be a string."
        );
    }
}
