//! ACP の境界でのパスの変換 (ADR-F49 決定 23)。
//!
//! Windows のローカルのプロジェクトで外部エージェントを WSL のサンドボックスの中で動かすと、
//! ACP のメッセージのパスはサンドボックスのパス (`/mnt/i/…`・`/home/…`) になり、Zed の中では
//! ホストのパス (`I:\…`・`\\wsl.localhost\…`) が要る。ACP の出入口でメッセージごとに直し、
//! サンドボックスにはワークツリーの外のパスを見せない。
//!
//! - エージェント → Zed の fs と terminal の要求は、通せないパスなら ACP のエラーにする
//! - エージェント → Zed の表示だけのもの (locations・Diff・リンク・terminal_info・session/list) は、
//!   通せない項目を落とす
//! - Zed → エージェントで通せない項目は省き、ログに残す。省いた MCP サーバーは戻り値でも返す
//! - 変換の仕方を知らない型 (ACP に後から足される種類など) は、パスを運びうるので落とす
//!
//! **ここで確かめるのは、綴りの上でワークツリーの中にあることだけ。** ワークツリーの中に置かれた
//! リンク (サンドボックスはワークツリーに書けるので作れる) の行き先は見ないので、ホストの Zed が
//! ファイルを読み書きする前に、1 つのハンドルで確かめる `fs::Fs::verify_read` (読み) と
//! `fs::Fs::write_verified` (書き) を通す。どちらも包んだ接続では `acp_thread` が既に通している。
//!
//! URI は、表示側 (`MentionUri::parse`) がどう読むかで分類する。表示側は先頭の空白や
//! バッククォート、素のパス、`zed:///agent/file?path=…` もファイルへのメンションとして読むので、
//! 元の綴りは通さず、パスを運ぶメンションはパスを変換して組み直す。http・https は正規の綴りの
//! ものだけを通し、それ以外の URI は落とす。

use std::mem;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acp_thread::MentionUri;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use collections::HashMap;
use http_client::{Host, Url};
use serde_json::Value;
use util::path_mapping::{
    PathMapper, SandboxPathError, SandboxPaths, WslPathMapper, WslWorktreeRootsError,
};
use util::paths::PathStyle;

/// セッション 1 本ぶんの、ACP のメッセージのパスの変換。
#[derive(Clone)]
pub struct AcpPathTranslator {
    paths: Arc<SandboxPaths>,
}

/// サンドボックスの中で実行する形のコマンド。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// サンドボックスに渡さなかった MCP サーバー。配線側がユーザーに知らせるために返す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroppedMcpServer {
    pub name: String,
    pub reason: McpServerDropReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerDropReason {
    /// コマンド・引数・環境変数の値が、サンドボックスから見えないホストのパス。
    HostPath(String),
    /// ホストのループバックなど、サンドボックスのネットワークからは届かない URL。
    UnreachableUrl,
    /// 変換の仕方を知らない転送方式。
    UnknownTransport,
}

#[derive(Clone, Copy)]
enum Direction {
    ToHost,
    ToClient,
}

impl Direction {
    fn log_dropped(self, what: &str) {
        match self {
            Self::ToHost => log::debug!("acp: dropped {what} from the agent"),
            Self::ToClient => log::warn!("acp: left out {what} from a message to the agent"),
        }
    }
}

impl AcpPathTranslator {
    pub fn new(paths: Arc<SandboxPaths>) -> Self {
        Self { paths }
    }

    /// Windows のローカルのプロジェクト (ワークツリーのルートが `worktree_roots`) を、WSL の
    /// `sandbox_distro` (`None` は既定のディストリビューション) のサンドボックスで動くエージェントに
    /// 見せる。複数のディストリビューションにまたがるプロジェクトなどは断る
    /// ([`WslPathMapper::for_worktree_roots`])。
    pub fn for_wsl(
        worktree_roots: Vec<PathBuf>,
        sandbox_distro: Option<&str>,
    ) -> Result<Self, WslWorktreeRootsError> {
        let mapper = WslPathMapper::for_worktree_roots(&worktree_roots, sandbox_distro)?;
        Ok(Self::new(Arc::new(SandboxPaths::new(
            Arc::new(mapper),
            worktree_roots,
        ))))
    }

    pub fn paths(&self) -> &SandboxPaths {
        &self.paths
    }

    /// 同じ写像を別のところでも使うための持ち手。制限 IDE セッション (ADR-F49 決定 24) は
    /// これを受け取り、エージェントとの引数・結果の変換 (決定 23) と同じ実体を共有する。
    pub fn shared_paths(&self) -> Arc<SandboxPaths> {
        self.paths.clone()
    }

    /// `fs/read_text_file` の要求のパスをホストのパスに直す。ワークツリーの外は
    /// `resource_not_found`、変換できないパス (`..`・相対パス・URI・Windows で使えない名前) は
    /// `invalid_params`。リンクの行き先は見ないので、読む前に `fs::Fs::verify_read`
    /// (`AcpThread::read_text_file` が包んだ接続で通す) で確かめる。
    pub fn read_text_file_to_host(
        &self,
        mut request: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileRequest, acp::Error> {
        request.path = self.requested_file_to_host(&request.path)?;
        Ok(request)
    }

    /// `fs/write_text_file` の要求。エラーとリンクの扱いは [`Self::read_text_file_to_host`] と同じ。
    pub fn write_text_file_to_host(
        &self,
        mut request: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileRequest, acp::Error> {
        request.path = self.requested_file_to_host(&request.path)?;
        Ok(request)
    }

    /// `terminal/create`。コマンド・引数・環境変数はサンドボックスの中で実行するものなので、
    /// 変換しない。
    pub fn create_terminal_to_host(
        &self,
        mut request: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalRequest, acp::Error> {
        if let Some(cwd) = request.cwd.take() {
            let host_cwd = self.client_path_to_host(&cwd).map_err(|error| {
                acp::Error::invalid_params().data(serde_json::json!({
                    "cwd": cwd.display().to_string(),
                    "reason": error.to_string(),
                }))
            })?;
            request.cwd = Some(host_cwd);
        }
        Ok(request)
    }

    /// `session/update`。通せない項目 (ワークツリーの外の locations・Diff・リンクなど) は落とす。
    /// メッセージの断片の中身が丸ごと落ちたときと、知らない種類の更新は `None` で、その通知は
    /// 捨てる。
    pub fn session_update_to_host(&self, update: acp::SessionUpdate) -> Option<acp::SessionUpdate> {
        Some(match update {
            acp::SessionUpdate::UserMessageChunk(chunk) => {
                acp::SessionUpdate::UserMessageChunk(self.chunk_to_host(chunk)?)
            }
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                acp::SessionUpdate::AgentMessageChunk(self.chunk_to_host(chunk)?)
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                acp::SessionUpdate::AgentThoughtChunk(self.chunk_to_host(chunk)?)
            }
            acp::SessionUpdate::ToolCall(tool_call) => {
                acp::SessionUpdate::ToolCall(self.tool_call_to_host(tool_call))
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                acp::SessionUpdate::ToolCallUpdate(self.tool_call_update_to_host(update))
            }
            // パスを運ばない種類。
            acp::SessionUpdate::Plan(_)
            | acp::SessionUpdate::AvailableCommandsUpdate(_)
            | acp::SessionUpdate::CurrentModeUpdate(_)
            | acp::SessionUpdate::ConfigOptionUpdate(_)
            | acp::SessionUpdate::SessionInfoUpdate(_)
            | acp::SessionUpdate::UsageUpdate(_) => update,
            _ => {
                Direction::ToHost.log_dropped("a kind of session update Zed does not know");
                return None;
            }
        })
    }

    pub fn tool_call_to_host(&self, mut tool_call: acp::ToolCall) -> acp::ToolCall {
        tool_call.locations = self.locations_to_host(tool_call.locations);
        tool_call.content = self.tool_call_contents_to_host(tool_call.content);
        tool_call.meta = tool_call.meta.map(|meta| self.meta_to_host(meta));
        tool_call
    }

    pub fn tool_call_update_to_host(&self, mut update: acp::ToolCallUpdate) -> acp::ToolCallUpdate {
        update.fields.locations = update
            .fields
            .locations
            .map(|locations| self.locations_to_host(locations));
        update.fields.content = update
            .fields
            .content
            .map(|contents| self.tool_call_contents_to_host(contents));
        update.meta = update.meta.map(|meta| self.meta_to_host(meta));
        update
    }

    /// `session/request_permission`。許可を求める tool call の locations と Diff を直す。
    pub fn request_permission_to_host(
        &self,
        mut request: acp::RequestPermissionRequest,
    ) -> acp::RequestPermissionRequest {
        request.tool_call = self.tool_call_update_to_host(request.tool_call);
        request
    }

    /// `session/list` の応答。フォルダーがワークツリーの外のセッションは落とす。
    pub fn list_sessions_response_to_host(
        &self,
        mut response: acp::ListSessionsResponse,
    ) -> acp::ListSessionsResponse {
        response.sessions = response
            .sessions
            .into_iter()
            .filter_map(|mut session| {
                session.cwd = self.displayed_path_to_host(&session.cwd)?;
                session.additional_directories = session
                    .additional_directories
                    .iter()
                    .filter_map(|directory| self.displayed_path_to_host(directory))
                    .collect();
                Some(session)
            })
            .collect();
        response
    }

    /// `session/new`。フォルダー (cwd) を通せなければ、セッションを始められないのでエラー。
    /// 通せない追加のフォルダーは省き、渡さなかった MCP サーバーを一緒に返す。
    pub fn new_session_to_client(
        &self,
        mut request: acp::NewSessionRequest,
    ) -> Result<(acp::NewSessionRequest, Vec<DroppedMcpServer>)> {
        let dropped = self.session_request_to_client(
            &mut request.cwd,
            &mut request.additional_directories,
            &mut request.mcp_servers,
        )?;
        Ok((request, dropped))
    }

    /// `session/load`。扱いは [`Self::new_session_to_client`] と同じ。
    pub fn load_session_to_client(
        &self,
        mut request: acp::LoadSessionRequest,
    ) -> Result<(acp::LoadSessionRequest, Vec<DroppedMcpServer>)> {
        let dropped = self.session_request_to_client(
            &mut request.cwd,
            &mut request.additional_directories,
            &mut request.mcp_servers,
        )?;
        Ok((request, dropped))
    }

    /// `session/resume`。扱いは [`Self::new_session_to_client`] と同じ。
    pub fn resume_session_to_client(
        &self,
        mut request: acp::ResumeSessionRequest,
    ) -> Result<(acp::ResumeSessionRequest, Vec<DroppedMcpServer>)> {
        let dropped = self.session_request_to_client(
            &mut request.cwd,
            &mut request.additional_directories,
            &mut request.mcp_servers,
        )?;
        Ok((request, dropped))
    }

    /// `session/fork`。扱いは [`Self::new_session_to_client`] と同じ。
    pub fn fork_session_to_client(
        &self,
        mut request: acp::ForkSessionRequest,
    ) -> Result<(acp::ForkSessionRequest, Vec<DroppedMcpServer>)> {
        let dropped = self.session_request_to_client(
            &mut request.cwd,
            &mut request.additional_directories,
            &mut request.mcp_servers,
        )?;
        Ok((request, dropped))
    }

    /// `session/list` の要求。フォルダーを通せなければ `None` で、エージェントには問い合わせずに
    /// 空の一覧とする (フォルダーを省くと、すべてのセッションの問い合わせになる)。
    pub fn list_sessions_to_client(
        &self,
        mut request: acp::ListSessionsRequest,
    ) -> Option<acp::ListSessionsRequest> {
        if let Some(cwd) = request.cwd.take() {
            request.cwd = Some(self.sent_path_to_client(&cwd)?);
        }
        Some(request)
    }

    /// `mcpServers`。サンドボックスに渡すものと、渡さなかったものを返す。
    ///
    /// stdio のサーバーは、コマンド・引数・環境変数の値のうち全体がホストの絶対パスであるもの
    /// だけをワークツリーの中に限って変換し、外を指すものがあればサーバーごと渡さない (サンドボックス
    /// の中からは開けない)。名前だけのコマンドはサンドボックスの PATH で探すのでそのまま。HTTP・SSE
    /// のサーバーは、ホストのループバックを指すものを渡さない (サンドボックスのループバックは別の
    /// 場所)。
    pub fn mcp_servers_to_client(
        &self,
        servers: Vec<acp::McpServer>,
    ) -> (Vec<acp::McpServer>, Vec<DroppedMcpServer>) {
        let mut kept = Vec::new();
        let mut dropped = Vec::new();
        for server in servers {
            let name = mcp_server_name(&server);
            let translated = match server {
                acp::McpServer::Stdio(server) => self
                    .stdio_server_to_client(server)
                    .map(acp::McpServer::Stdio),
                acp::McpServer::Http(server) => reachable_from_sandbox(&server.url)
                    .then_some(acp::McpServer::Http(server))
                    .ok_or(McpServerDropReason::UnreachableUrl),
                acp::McpServer::Sse(server) => reachable_from_sandbox(&server.url)
                    .then_some(acp::McpServer::Sse(server))
                    .ok_or(McpServerDropReason::UnreachableUrl),
                _ => Err(McpServerDropReason::UnknownTransport),
            };
            match translated {
                Ok(server) => kept.push(server),
                Err(reason) => {
                    // URL はトークンを含みうるので、ログには名前と理由だけを出す。配線側
                    // (`note_dropped_mcp_servers`) が戻り値から同じことを warn で出すので、
                    // ここは重ねない。
                    log::debug!("acp: left out the MCP server {name}: {reason:?}");
                    dropped.push(DroppedMcpServer { name, reason });
                }
            }
        }
        (kept, dropped)
    }

    /// `session/prompt` の本文。ワークツリーの外のファイルへのリンクは省き、ユーザーが添付した
    /// ファイルの本文 (埋め込み) はパスだけを落として文字として送る。画像はデータを残し、URI
    /// だけを省く。
    pub fn prompt_to_client(&self, prompt: Vec<acp::ContentBlock>) -> Vec<acp::ContentBlock> {
        prompt
            .into_iter()
            .filter_map(|block| self.translate_content_block(block, Direction::ToClient))
            .collect()
    }

    /// ログイン (terminal-auth) で、Zed が持つエージェントのコマンドをサンドボックスの中で実行する
    /// 形に直す。エージェントの実行ファイルと node はワークツリーの外 (WSL の中の置き場所) に
    /// あるので、ワークツリーに閉じ込めずに変換する。全体がホストの絶対パスである値だけを変換し、
    /// 名前だけのコマンドとサンドボックスのパスはそのまま。エージェントが `_meta` で渡すコマンドは
    /// 初めからサンドボックスの値なので、ここを通さない。
    pub fn host_command_to_client(
        &self,
        program: &Path,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<ClientCommand> {
        let program = program.to_str().context("the command is not UTF-8")?;
        Ok(ClientCommand {
            program: self.unconfined_value_to_client(program)?,
            args: args
                .iter()
                .map(|argument| self.unconfined_value_to_client(argument))
                .collect::<Result<_>>()?,
            env: env
                .iter()
                .map(|(name, value)| Ok((name.clone(), self.unconfined_value_to_client(value)?)))
                .collect::<Result<_>>()?,
        })
    }

    fn client_path_to_host(&self, client_path: &Path) -> Result<PathBuf, SandboxPathError> {
        let client_path = client_path.to_str().ok_or(SandboxPathError::Unmappable)?;
        self.paths.client_path_to_host(client_path)
    }

    fn requested_file_to_host(&self, client_path: &Path) -> Result<PathBuf, acp::Error> {
        self.client_path_to_host(client_path)
            .map_err(|error| match error {
                SandboxPathError::OutsideWorkspace => {
                    acp::Error::resource_not_found(Some(client_path.display().to_string()))
                }
                SandboxPathError::Unmappable => {
                    acp::Error::invalid_params().data(serde_json::json!({
                        "path": client_path.display().to_string(),
                        "reason": error.to_string(),
                    }))
                }
            })
    }

    fn displayed_path_to_host(&self, client_path: &Path) -> Option<PathBuf> {
        self.client_path_to_host(client_path)
            .inspect_err(|error| {
                log::debug!(
                    "acp: dropped {} from the agent: {error}",
                    client_path.display()
                )
            })
            .ok()
    }

    fn chunk_to_host(&self, mut chunk: acp::ContentChunk) -> Option<acp::ContentChunk> {
        chunk.content = self.translate_content_block(chunk.content, Direction::ToHost)?;
        Some(chunk)
    }

    fn locations_to_host(
        &self,
        locations: Vec<acp::ToolCallLocation>,
    ) -> Vec<acp::ToolCallLocation> {
        locations
            .into_iter()
            .filter_map(|mut location| {
                location.path = self.displayed_path_to_host(&location.path)?;
                Some(location)
            })
            .collect()
    }

    fn tool_call_contents_to_host(
        &self,
        contents: Vec<acp::ToolCallContent>,
    ) -> Vec<acp::ToolCallContent> {
        contents
            .into_iter()
            .filter_map(|content| match content {
                acp::ToolCallContent::Diff(mut diff) => {
                    diff.path = self.displayed_path_to_host(&diff.path)?;
                    Some(acp::ToolCallContent::Diff(diff))
                }
                acp::ToolCallContent::Content(mut content) => {
                    content.content =
                        self.translate_content_block(content.content, Direction::ToHost)?;
                    Some(acp::ToolCallContent::Content(content))
                }
                acp::ToolCallContent::Terminal(_) => Some(content),
                _ => {
                    Direction::ToHost.log_dropped("a kind of tool call content Zed does not know");
                    None
                }
            })
            .collect()
    }

    /// 開けない URI の扱いは向きによらない。リンクは落とし、埋め込みの本文は文字として残し
    /// (受け取りならエージェントが書いたもの、送りならユーザーが添付したもの)、画像は URI だけを
    /// 落とす。
    fn translate_content_block(
        &self,
        block: acp::ContentBlock,
        direction: Direction,
    ) -> Option<acp::ContentBlock> {
        match block {
            acp::ContentBlock::Text(_) | acp::ContentBlock::Audio(_) => Some(block),
            acp::ContentBlock::ResourceLink(mut link) => {
                link.uri = self.translate_uri(&link.uri, direction)?;
                Some(acp::ContentBlock::ResourceLink(link))
            }
            acp::ContentBlock::Resource(resource) => {
                self.translate_embedded_resource(resource, direction)
            }
            acp::ContentBlock::Image(mut image) => {
                image.uri = image
                    .uri
                    .and_then(|uri| self.translate_uri(&uri, direction));
                Some(acp::ContentBlock::Image(image))
            }
            _ => {
                direction.log_dropped("a kind of content Zed does not know");
                None
            }
        }
    }

    fn translate_embedded_resource(
        &self,
        mut resource: acp::EmbeddedResource,
        direction: Direction,
    ) -> Option<acp::ContentBlock> {
        match &mut resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(contents) => {
                match self.translate_uri(&contents.uri, direction) {
                    Some(uri) => contents.uri = uri,
                    None => {
                        return Some(acp::ContentBlock::Text(acp::TextContent::new(mem::take(
                            &mut contents.text,
                        ))));
                    }
                }
            }
            acp::EmbeddedResourceResource::BlobResourceContents(contents) => {
                contents.uri = self.translate_uri(&contents.uri, direction)?;
            }
            _ => {
                direction.log_dropped("a kind of embedded resource Zed does not know");
                return None;
            }
        }
        Some(acp::ContentBlock::Resource(resource))
    }

    /// URI を相手側の綴りに直す。通せなければ `None` で、ログに残す。
    fn translate_uri(&self, uri: &str, direction: Direction) -> Option<String> {
        let translated = if is_canonical_web_url(uri) {
            Some(uri.to_string())
        } else {
            self.translate_mention_uri(uri, direction)
        };
        if translated.is_none() {
            direction.log_dropped(&format!("a link to {uri:?}"));
        }
        translated
    }

    fn translate_mention_uri(&self, uri: &str, direction: Direction) -> Option<String> {
        let (source_style, target_style) = match direction {
            Direction::ToHost => (PathStyle::Unix, self.paths.host_path_style()),
            Direction::ToClient => (self.paths.host_path_style(), PathStyle::Unix),
        };
        let mention = MentionUri::parse(uri, source_style).ok()?;
        let translated = self.translate_mention(mention, direction, target_style)?;
        let translated_uri = translated.to_uri().to_string();
        // `to_uri` は名前の中の `%` をエンコードしないので、`%` を含む名前は受け取る側で別の名前
        // (`%2e%2e` なら上の階層) に読まれうる。受け取る側と同じ読み方で読み直し、同じメンション
        // になるものだけを渡す。
        let reparsed = MentionUri::parse(&translated_uri, target_style).ok()?;
        (reparsed == translated).then_some(translated_uri)
    }

    fn translate_mention(
        &self,
        mention: MentionUri,
        direction: Direction,
        target_style: PathStyle,
    ) -> Option<MentionUri> {
        Some(match mention {
            MentionUri::File { abs_path } => MentionUri::File {
                abs_path: self.translate_path(&abs_path, direction)?,
            },
            MentionUri::Directory { abs_path } => MentionUri::Directory {
                // 表示側はディレクトリを末尾の区切り付きで読み直すので、比べられるように付けておく。
                abs_path: with_trailing_separator(
                    self.translate_path(&abs_path, direction)?,
                    target_style,
                ),
            },
            MentionUri::Symbol {
                abs_path,
                name,
                line_range,
            } => MentionUri::Symbol {
                abs_path: self.translate_path(&abs_path, direction)?,
                name,
                line_range,
            },
            MentionUri::Selection {
                abs_path: Some(abs_path),
                line_range,
                column,
            } => MentionUri::Selection {
                abs_path: Some(self.translate_path(&abs_path, direction)?),
                line_range,
                column,
            },
            MentionUri::Skill {
                name,
                source,
                skill_file_path,
            } => MentionUri::Skill {
                name,
                source,
                skill_file_path: self.translate_path(&skill_file_path, direction)?,
            },
            MentionUri::MergeConflict { file_path } => MentionUri::MergeConflict {
                file_path: self
                    .translate_path(Path::new(&file_path), direction)?
                    .to_string_lossy()
                    .into_owned(),
            },
            _ => return None,
        })
    }

    fn translate_path(&self, path: &Path, direction: Direction) -> Option<PathBuf> {
        match direction {
            Direction::ToHost => self.client_path_to_host(path).ok(),
            Direction::ToClient => self.paths.to_client(path).map(PathBuf::from),
        }
    }

    /// 表示だけの端末 (`_meta.terminal_info`) の作業フォルダー。通せなければ消す。
    fn meta_to_host(&self, mut meta: acp::Meta) -> acp::Meta {
        if let Some(Value::Object(terminal_info)) = meta.get_mut("terminal_info")
            && let Some(cwd) = terminal_info.get("cwd").and_then(Value::as_str)
        {
            match self.displayed_path_to_host(Path::new(cwd)) {
                Some(host_cwd) => {
                    terminal_info.insert(
                        "cwd".to_string(),
                        Value::String(host_cwd.to_string_lossy().into_owned()),
                    );
                }
                None => {
                    terminal_info.remove("cwd");
                }
            }
        }
        meta
    }

    fn session_request_to_client(
        &self,
        cwd: &mut PathBuf,
        additional_directories: &mut Vec<PathBuf>,
        mcp_servers: &mut Vec<acp::McpServer>,
    ) -> Result<Vec<DroppedMcpServer>> {
        // ADR-F07: この文はセッションを始められなかった理由としてそのまま UI に出る
        // (`AcpConnection::new_session` / `load_session` / `resume_session`) ので日本語。
        // エージェントに返す ACP のエラーの `reason` は英語のまま。
        *cwd = self
            .paths
            .to_client(cwd)
            .map(PathBuf::from)
            .with_context(|| {
                i18n::t!("The folder {} can't be shown to the sandbox", cwd.display())
            })?;
        *additional_directories = additional_directories
            .iter()
            .filter_map(|folder| self.sent_path_to_client(folder))
            .collect();
        let (kept, dropped) = self.mcp_servers_to_client(mem::take(mcp_servers));
        *mcp_servers = kept;
        Ok(dropped)
    }

    fn sent_path_to_client(&self, host_path: &Path) -> Option<PathBuf> {
        let client_path = self.paths.to_client(host_path);
        if client_path.is_none() {
            log::warn!(
                "acp: left out {}, which the sandbox cannot see",
                host_path.display()
            );
        }
        client_path.map(PathBuf::from)
    }

    fn stdio_server_to_client(
        &self,
        mut server: acp::McpServerStdio,
    ) -> Result<acp::McpServerStdio, McpServerDropReason> {
        let command = server.command.to_str().ok_or_else(|| {
            McpServerDropReason::HostPath(server.command.to_string_lossy().into_owned())
        })?;
        server.command = PathBuf::from(self.confined_value_to_client(command)?);
        for argument in &mut server.args {
            *argument = self.confined_value_to_client(argument)?;
        }
        for variable in &mut server.env {
            variable.value = self.confined_value_to_client(&variable.value)?;
        }
        Ok(server)
    }

    fn confined_value_to_client(&self, value: &str) -> Result<String, McpServerDropReason> {
        if !is_host_absolute(value, self.paths.host_path_style()) {
            return Ok(value.to_string());
        }
        self.paths
            .to_client(Path::new(value))
            .ok_or_else(|| McpServerDropReason::HostPath(value.to_string()))
    }

    fn unconfined_value_to_client(&self, value: &str) -> Result<String> {
        if !is_host_absolute(value, self.paths.host_path_style()) {
            return Ok(value.to_string());
        }
        PathMapper::to_client(self.paths.mapper(), Path::new(value))
            .with_context(|| format!("{value} has no path inside the sandbox"))
    }
}

/// 全体がホストの絶対パスとして書かれているか。Windows のホストでは `/` で始まるものは
/// ドライブからの相対パスで、ホストのパスとしては使えないので、サンドボックスの値とみなす。
fn is_host_absolute(value: &str, style: PathStyle) -> bool {
    match style {
        PathStyle::Windows => {
            let mut characters = value.chars();
            value.starts_with(r"\\")
                || matches!(
                    (characters.next(), characters.next(), characters.next()),
                    (Some(letter), Some(':'), Some('\\' | '/')) if letter.is_ascii_alphabetic()
                )
        }
        PathStyle::Unix => value.starts_with('/'),
    }
}

/// `url` で読み直した綴りと同じもの。前後の空白・大文字のスキーム・末尾の `/` の省略などは、
/// 表示側の読み方が変わりうるので通さない。
fn is_canonical_web_url(uri: &str) -> bool {
    Url::parse(uri).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.as_str() == uri)
}

fn with_trailing_separator(path: PathBuf, style: PathStyle) -> PathBuf {
    let mut text = path.to_string_lossy().into_owned();
    let has_separator = match style {
        PathStyle::Windows => text.ends_with(['\\', '/']),
        PathStyle::Unix => text.ends_with('/'),
    };
    if !has_separator {
        text.push_str(style.primary_separator());
    }
    PathBuf::from(text)
}

fn mcp_server_name(server: &acp::McpServer) -> String {
    match server {
        acp::McpServer::Stdio(server) => server.name.clone(),
        acp::McpServer::Http(server) => server.name.clone(),
        acp::McpServer::Sse(server) => server.name.clone(),
        // 知らない転送方式でも、どのサーバーを渡さなかったかは知らせたい。
        other => serde_json::to_value(other)
            .ok()
            .and_then(|value| value.get("name")?.as_str().map(str::to_string))
            .unwrap_or_default(),
    }
}

fn reachable_from_sandbox(url: &str) -> bool {
    match Url::parse(url).map(|url| url.host().map(|host| host.to_owned())) {
        Ok(Some(Host::Domain(domain))) => !is_localhost_name(&domain),
        Ok(Some(Host::Ipv4(address))) => is_routable_from_sandbox(IpAddr::V4(address)),
        Ok(Some(Host::Ipv6(address))) => is_routable_from_sandbox(IpAddr::V6(address)),
        Ok(None) | Err(_) => false,
    }
}

/// `::ffff:127.0.0.1` のような IPv4 射影アドレスも、IPv4 のループバックとして扱う。
fn is_routable_from_sandbox(address: IpAddr) -> bool {
    let address = address.to_canonical();
    !address.is_loopback() && !address.is_unspecified()
}

fn is_localhost_name(domain: &str) -> bool {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    domain == "localhost" || domain.ends_with(".localhost")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use util::path_mapping::IdentityPathMapper;

    fn translator() -> AcpPathTranslator {
        AcpPathTranslator::for_wsl(
            vec![
                PathBuf::from(r"I:\proj"),
                PathBuf::from(r"\\wsl.localhost\Ubuntu\root\p"),
            ],
            Some("Ubuntu"),
        )
        .expect("the project is in one distribution")
    }

    fn session_id() -> acp::SessionId {
        acp::SessionId::new("session")
    }

    fn link(uri: &str) -> acp::ContentBlock {
        acp::ContentBlock::ResourceLink(acp::ResourceLink::new("a.rs", uri))
    }

    fn embedded(uri: &str) -> acp::ContentBlock {
        acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents::new(
                "fn main() {}",
                uri,
            )),
        ))
    }

    fn blob(uri: &str) -> acp::ContentBlock {
        acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::BlobResourceContents(acp::BlobResourceContents::new(
                "AAAA", uri,
            )),
        ))
    }

    fn image(uri: Option<&str>) -> acp::ContentBlock {
        acp::ContentBlock::Image(
            acp::ImageContent::new("AAAA", "image/png").uri(uri.map(str::to_string)),
        )
    }

    fn text(text: &str) -> acp::ContentBlock {
        acp::ContentBlock::Text(acp::TextContent::new(text))
    }

    fn message(block: acp::ContentBlock) -> acp::SessionUpdate {
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(block))
    }

    fn tool_call(title: &str) -> acp::ToolCall {
        acp::ToolCall::new(acp::ToolCallId::new("call"), title)
    }

    fn skill(path: &str) -> String {
        MentionUri::Skill {
            name: "s".to_string(),
            source: "user".to_string(),
            skill_file_path: PathBuf::from(path),
        }
        .to_uri()
        .to_string()
    }

    fn conflict(path: &str) -> String {
        MentionUri::MergeConflict {
            file_path: path.to_string(),
        }
        .to_uri()
        .to_string()
    }

    /// エージェントのメッセージの中のリンクが、Zed に届くときの URI (落ちれば `None`)。
    fn link_to_host(translator: &AcpPathTranslator, uri: &str) -> Option<String> {
        match translator.session_update_to_host(message(link(uri)))? {
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                content: acp::ContentBlock::ResourceLink(link),
                ..
            }) => Some(link.uri),
            other => panic!("{uri:?} became {other:?}"),
        }
    }

    #[test]
    fn maps_file_requests_into_the_workspace_or_fails() {
        let translator = translator();
        let read = |path: &str| {
            translator
                .read_text_file_to_host(acp::ReadTextFileRequest::new(session_id(), path))
                .map(|request| request.path)
        };
        assert_eq!(
            read("/mnt/i/proj/src/日本語 a.rs"),
            Ok(PathBuf::from(r"I:\proj\src\日本語 a.rs"))
        );
        assert_eq!(
            read("/root/p/a.rs"),
            Ok(PathBuf::from(r"\\wsl.localhost\Ubuntu\root\p\a.rs"))
        );
        assert_eq!(read("/mnt/i/proj"), Ok(PathBuf::from(r"I:\proj")));
        // パスは URI ではないので、`%` はそのまま名前の一部。
        assert_eq!(
            read("/mnt/i/proj/a%20b.rs"),
            Ok(PathBuf::from(r"I:\proj\a%20b.rs"))
        );

        let code = |path: &str| read(path).map_err(|error| error.code);
        for outside in [
            "/mnt/i/other/a.rs",
            "/mnt/i/projX/a.rs",
            "/mnt/i/PROJ~1/a.rs",
            "/root/other",
            "/mnt/c/Users/me/.ssh/id_rsa",
        ] {
            assert_eq!(
                code(outside),
                Err(acp::ErrorCode::ResourceNotFound),
                "{outside}"
            );
        }
        for unmappable in [
            "/mnt/i/proj/../other/a.rs",
            "src/a.rs",
            "file:///mnt/i/proj/a.rs",
            r"I:\proj\a.rs",
            " /mnt/i/proj/a.rs",
            "/mnt/i/proj/a?.rs",
            "/mnt/i/proj/a.rs::$DATA",
        ] {
            assert_eq!(
                code(unmappable),
                Err(acp::ErrorCode::InvalidParams),
                "{unmappable}"
            );
        }

        let written = translator
            .write_text_file_to_host(acp::WriteTextFileRequest::new(
                session_id(),
                "/root/p/テスト.rs",
                "text",
            ))
            .map(|request| (request.path, request.content));
        assert_eq!(
            written,
            Ok((
                PathBuf::from(r"\\wsl.localhost\Ubuntu\root\p\テスト.rs"),
                "text".to_string()
            ))
        );
        assert_eq!(
            translator
                .write_text_file_to_host(acp::WriteTextFileRequest::new(
                    session_id(),
                    "/mnt/i/proj/.git/../../x",
                    "text",
                ))
                .map_err(|error| error.code),
            Err(acp::ErrorCode::InvalidParams)
        );
    }

    /// ADR-F49 決定 23: WSL の共有 (ext4) をプロジェクトとして開くと、ワークツリーの
    /// `abs_path` は Windows の `canonicalize` が返す `\\?\UNC\wsl.localhost\…` になる。
    /// `Project` がパスからワークツリーを見つけるのは綴りの前方一致なので、fs と terminal の
    /// 要求はその綴りのまま返す (`\\wsl.localhost\…` で返すと、同じ場所でも読み取りは
    /// `resource_not_found`、書き込みは「プロジェクトのフォルダーの中にない」と断られる)。
    #[test]
    fn maps_file_requests_into_a_canonicalized_wsl_worktree() {
        let root = r"\\?\UNC\wsl.localhost\Ubuntu\root\p";
        let translator = AcpPathTranslator::for_wsl(vec![PathBuf::from(root)], Some("Ubuntu"))
            .expect("a canonicalized WSL share names one distribution");
        assert_eq!(
            translator
                .read_text_file_to_host(acp::ReadTextFileRequest::new(
                    session_id(),
                    "/root/p/seed.txt"
                ))
                .map(|request| request.path),
            Ok(PathBuf::from(format!(r"{root}\seed.txt")))
        );
        assert_eq!(
            translator
                .write_text_file_to_host(acp::WriteTextFileRequest::new(
                    session_id(),
                    "/root/p/observed.txt",
                    "text",
                ))
                .map(|request| request.path),
            Ok(PathBuf::from(format!(r"{root}\observed.txt")))
        );
        assert_eq!(
            translator
                .create_terminal_to_host(
                    acp::CreateTerminalRequest::new(session_id(), "./run.sh")
                        .cwd(Some(PathBuf::from("/root/p/sub")))
                )
                .map(|request| request.cwd),
            Ok(Some(PathBuf::from(format!(r"{root}\sub"))))
        );
        assert_eq!(
            translator
                .tool_call_to_host(
                    tool_call("Read")
                        .locations(vec![acp::ToolCallLocation::new("/root/p/seed.txt")])
                )
                .locations,
            vec![acp::ToolCallLocation::new(format!(r"{root}\seed.txt"))],
            "what the agent points at is shown in the worktree's own spelling"
        );
        assert_eq!(
            translator
                .paths()
                .to_client(Path::new(&format!(r"{root}\seed.txt"))),
            Some("/root/p/seed.txt".to_string()),
            "and Zed's own spelling still reaches the agent"
        );
        assert_eq!(
            translator
                .read_text_file_to_host(acp::ReadTextFileRequest::new(session_id(), "/root/q/a.rs"))
                .map_err(|error| error.code),
            Err(acp::ErrorCode::ResourceNotFound)
        );
    }

    #[test]
    fn maps_the_terminal_folder_and_leaves_the_command_alone() {
        let translator = translator();
        let create = |cwd: Option<&str>| {
            translator.create_terminal_to_host(
                acp::CreateTerminalRequest::new(session_id(), "/mnt/i/proj/run.sh")
                    .args(vec!["/mnt/c/x".to_string()])
                    .cwd(cwd.map(PathBuf::from)),
            )
        };
        let request = create(Some("/mnt/i/proj/sub")).expect("the folder is inside");
        assert_eq!(request.cwd, Some(PathBuf::from(r"I:\proj\sub")));
        assert_eq!(request.command, "/mnt/i/proj/run.sh");
        assert_eq!(request.args, vec!["/mnt/c/x".to_string()]);
        assert_eq!(create(None).map(|request| request.cwd), Ok(None));
        for cwd in ["/mnt/i/other", "/mnt/i/proj/..", "relative"] {
            assert_eq!(
                create(Some(cwd)).map_err(|error| error.code).map(|_| ()),
                Err(acp::ErrorCode::InvalidParams),
                "{cwd}"
            );
        }
    }

    #[test]
    fn drops_displayed_items_outside_the_workspace() {
        let translator = translator();
        let mut meta = acp::Meta::new();
        meta.insert(
            "terminal_info".to_string(),
            json!({ "terminal_id": "t1", "cwd": "/mnt/i/proj/sub" }),
        );
        let call = tool_call("Edit")
            .locations(vec![
                acp::ToolCallLocation::new("/mnt/i/proj/a.rs").line(3),
                acp::ToolCallLocation::new("/mnt/i/other/b.rs"),
                acp::ToolCallLocation::new("/mnt/i/proj/../c.rs"),
            ])
            .content(vec![
                acp::ToolCallContent::Diff(acp::Diff::new("/root/p/日本語.rs", "new")),
                acp::ToolCallContent::Diff(acp::Diff::new("/etc/passwd", "new")),
                acp::ToolCallContent::Content(acp::Content::new(link(
                    "file:///mnt/i/proj/%E6%97%A5.rs#L10:20",
                ))),
                acp::ToolCallContent::Content(acp::Content::new(link("file:///mnt/c/secret.rs"))),
                acp::ToolCallContent::Content(acp::Content::new(link("https://example.com/a"))),
                acp::ToolCallContent::Terminal(acp::Terminal::new(acp::TerminalId::new("t1"))),
            ])
            .meta(meta);
        let Some(acp::SessionUpdate::ToolCall(call)) =
            translator.session_update_to_host(acp::SessionUpdate::ToolCall(call))
        else {
            panic!("a tool call is never dropped as a whole");
        };
        assert_eq!(
            call.locations,
            vec![acp::ToolCallLocation::new(r"I:\proj\a.rs").line(3)]
        );
        assert_eq!(
            call.content,
            vec![
                acp::ToolCallContent::Diff(acp::Diff::new(
                    r"\\wsl.localhost\Ubuntu\root\p\日本語.rs",
                    "new"
                )),
                acp::ToolCallContent::Content(acp::Content::new(link(
                    "file:///I:/proj/%E6%97%A5.rs#L10:20"
                ))),
                acp::ToolCallContent::Content(acp::Content::new(link("https://example.com/a"))),
                acp::ToolCallContent::Terminal(acp::Terminal::new(acp::TerminalId::new("t1"))),
            ]
        );
        assert_eq!(
            call.meta
                .as_ref()
                .and_then(|meta| meta.get("terminal_info")),
            Some(&json!({ "terminal_id": "t1", "cwd": r"I:\proj\sub" }))
        );

        let mut meta = acp::Meta::new();
        meta.insert(
            "terminal_info".to_string(),
            json!({ "terminal_id": "t2", "cwd": "/mnt/c/Windows" }),
        );
        let call = translator.tool_call_to_host(tool_call("Run").meta(meta));
        assert_eq!(
            call.meta
                .and_then(|meta| meta.get("terminal_info").cloned()),
            Some(json!({ "terminal_id": "t2" }))
        );

        // パスを運ばない種類の更新はそのまま。
        let mode = acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
            acp::SessionModeId::new("default"),
        ));
        assert_eq!(translator.session_update_to_host(mode.clone()), Some(mode));
    }

    #[test]
    fn asks_for_permission_only_with_paths_in_the_workspace() {
        let request = translator().request_permission_to_host(acp::RequestPermissionRequest::new(
            session_id(),
            acp::ToolCallUpdate::new(
                acp::ToolCallId::new("call"),
                acp::ToolCallUpdateFields::new()
                    .locations(vec![
                        acp::ToolCallLocation::new("/mnt/i/proj/a.rs"),
                        acp::ToolCallLocation::new("/home/u/.bashrc"),
                    ])
                    .content(vec![
                        acp::ToolCallContent::Diff(acp::Diff::new(
                            "/mnt/i/proj/.claude/settings.json",
                            "{}",
                        )),
                        acp::ToolCallContent::Diff(acp::Diff::new("/mnt/c/Windows/x.ini", "")),
                    ]),
            ),
            vec![acp::PermissionOption::new(
                acp::PermissionOptionId::new("allow"),
                "Allow",
                acp::PermissionOptionKind::AllowOnce,
            )],
        ));
        assert_eq!(
            request.tool_call.fields.locations,
            Some(vec![acp::ToolCallLocation::new(r"I:\proj\a.rs")])
        );
        assert_eq!(
            request.tool_call.fields.content,
            Some(vec![acp::ToolCallContent::Diff(acp::Diff::new(
                r"I:\proj\.claude\settings.json",
                "{}"
            ))])
        );
        assert_eq!(request.options.len(), 1);
    }

    /// 表示側 (`MentionUri::parse`) がファイルとして読むものは、元の綴りを通さず、パスを変換して
    /// 組み直したものだけを渡す。
    #[test]
    fn classifies_agent_uris_the_way_the_display_reads_them() {
        let translator = translator();
        for (uri, expected) in [
            ("file:///mnt/i/proj/a.rs", Some("file:///I:/proj/a.rs")),
            ("/mnt/i/proj/a.rs", Some("file:///I:/proj/a.rs")),
            ("`file:///mnt/i/proj/a.rs`", Some("file:///I:/proj/a.rs")),
            ("\u{1}file:///mnt/i/proj/a.rs", Some("file:///I:/proj/a.rs")),
            (
                "zed:///agent/file?path=/mnt/i/proj/a.rs",
                Some("file:///I:/proj/a.rs"),
            ),
            (
                "zed:///agent/directory?path=/mnt/i/proj/src",
                Some("file:///I:/proj/src/"),
            ),
            ("file:///mnt/i/proj/src/", Some("file:///I:/proj/src/")),
            (
                "file:///mnt/i/proj/a.rs#L10:20",
                Some("file:///I:/proj/a.rs#L10:20"),
            ),
            (
                "file:///mnt/i/proj/a.rs?symbol=main#L1:3",
                Some("file:///I:/proj/a.rs?symbol=main#L1:3"),
            ),
            (
                "file:///mnt/i/proj/%E6%97%A5%20a.rs",
                Some("file:///I:/proj/%E6%97%A5%20a.rs"),
            ),
            ("https://example.com/a", Some("https://example.com/a")),
            (" file:///C:/Users/me/.ssh/id_rsa", None),
            ("\tfile:///mnt/c/Users/me/.ssh/id_rsa", None),
            ("`file:///mnt/c/secret.rs`", None),
            (r"C:\Users\me\.ssh\id_rsa", None),
            ("/mnt/c/Users/me/.ssh/id_rsa", None),
            (r"zed:///agent/file?path=C:\Users\me\.ssh\id_rsa", None),
            ("zed:///agent/directory?path=/mnt/c/Users", None),
            ("zed:///agent/selection?path=/mnt/c/x.rs#L1:2", None),
            ("zed:///agent/symbol/main?path=/mnt/c/x.rs#L1:2", None),
            (
                "zed:///agent/skill?name=s&source=u&path=/mnt/c/s/SKILL.md",
                None,
            ),
            ("zed:///agent/merge-conflict?path=/mnt/c/x.rs", None),
            ("zed:///agent/merge-conflict?path=src/main.rs", None),
            ("zed:///agent/thread/1?name=x", None),
            ("zed:///agent/pasted-image?name=Image", None),
            ("https://example.com", None),
            (" https://example.com/a", None),
            ("HTTPS://example.com/a", None),
            ("file:///mnt/i/proj/%2e%2e/other/a.rs", None),
            // `%2e%2e` という名前は、`to_uri` を通すと上の階層として読まれてしまう。
            ("file:///mnt/i/proj/%252e%252e/secret", None),
            ("file:///mnt/i/proj/a%2520b.rs", None),
            ("file:///mnt/i/proj/a.rs::$DATA", None),
            (
                "file:///root/p/a.rs",
                Some("file://wsl.localhost/Ubuntu/root/p/a.rs"),
            ),
        ] {
            assert_eq!(
                link_to_host(&translator, uri).as_deref(),
                expected,
                "{uri:?}"
            );
        }
        // 冗長な UTF-8 (`%C0%AE`) は `.` にならず、ワークツリーの外へは出ない。
        let overlong = link_to_host(&translator, "file:///mnt/i/proj/%C0%AE%C0%AE/x");
        assert!(
            overlong
                .as_deref()
                .is_none_or(|uri| uri.starts_with("file:///I:/proj/")),
            "{overlong:?}"
        );
    }

    #[test]
    fn keeps_message_text_and_drops_links_it_cannot_open() {
        let translator = translator();
        let to_host = |block| translator.session_update_to_host(message(block));
        assert_eq!(
            to_host(embedded("file:///mnt/i/proj/main.rs")),
            Some(message(embedded("file:///I:/proj/main.rs")))
        );
        // 埋め込みの本文は残し、開けないリンクだけを落とす。
        assert_eq!(
            to_host(embedded("file:///mnt/c/other/main.rs")),
            Some(message(text("fn main() {}")))
        );
        assert_eq!(
            to_host(embedded("zed:///agent/thread/1?name=x")),
            Some(message(text("fn main() {}")))
        );
        assert_eq!(to_host(blob("file:///mnt/c/other/a.png")), None);
        assert_eq!(
            to_host(blob("file:///mnt/i/proj/a.png")),
            Some(message(blob("file:///I:/proj/a.png")))
        );
        assert_eq!(
            to_host(blob("file:///root/p/a.png")),
            Some(message(blob("file://wsl.localhost/Ubuntu/root/p/a.png")))
        );
        assert_eq!(to_host(link("file:///mnt/c/a.rs")), None);
        assert_eq!(
            to_host(image(Some("file:///mnt/c/a.png"))),
            Some(message(image(None)))
        );
        assert_eq!(
            to_host(image(Some("file:///mnt/i/proj/a.png"))),
            Some(message(image(Some("file:///I:/proj/a.png"))))
        );
        assert_eq!(
            to_host(text("see /mnt/c/secret")),
            Some(message(text("see /mnt/c/secret")))
        );
        let thought = acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(link(
            "file:///mnt/i/proj/a.rs",
        )));
        assert_eq!(
            translator.session_update_to_host(thought),
            Some(acp::SessionUpdate::AgentThoughtChunk(
                acp::ContentChunk::new(link("file:///I:/proj/a.rs"))
            ))
        );
    }

    #[test]
    fn lists_only_sessions_in_the_workspace() {
        let translator = translator();
        let response =
            translator.list_sessions_response_to_host(acp::ListSessionsResponse::new(vec![
                acp::SessionInfo::new(session_id(), "/mnt/i/proj").additional_directories(vec![
                    PathBuf::from("/root/p"),
                    PathBuf::from("/mnt/c/other"),
                ]),
                acp::SessionInfo::new(acp::SessionId::new("other"), "/mnt/c/other"),
            ]));
        assert_eq!(
            response.sessions,
            vec![
                acp::SessionInfo::new(session_id(), r"I:\proj")
                    .additional_directories(vec![PathBuf::from(r"\\wsl.localhost\Ubuntu\root\p")])
            ]
        );

        let list = |cwd: Option<&str>| {
            translator
                .list_sessions_to_client(
                    acp::ListSessionsRequest::new().cwd(cwd.map(PathBuf::from)),
                )
                .map(|request| request.cwd)
        };
        assert_eq!(
            list(Some(r"I:\proj")),
            Some(Some(PathBuf::from("/mnt/i/proj")))
        );
        assert_eq!(list(None), Some(None));
        assert_eq!(list(Some(r"C:\elsewhere")), None);
    }

    #[test]
    fn sends_session_folders_to_the_sandbox() {
        let translator = translator();
        let (request, dropped) = translator
            .new_session_to_client(
                acp::NewSessionRequest::new(r"I:\proj")
                    .additional_directories(vec![
                        PathBuf::from(r"\\wsl$\Ubuntu\root\p"),
                        PathBuf::from(r"C:\elsewhere"),
                    ])
                    .mcp_servers(vec![acp::McpServer::Http(acp::McpServerHttp::new(
                        "zed",
                        "http://127.0.0.1:4321/mcp",
                    ))]),
            )
            .expect("the project folder maps");
        assert_eq!(request.cwd, PathBuf::from("/mnt/i/proj"));
        assert_eq!(
            request.additional_directories,
            vec![PathBuf::from("/root/p")]
        );
        assert_eq!(request.mcp_servers, Vec::new());
        assert_eq!(
            dropped,
            vec![DroppedMcpServer {
                name: "zed".to_string(),
                reason: McpServerDropReason::UnreachableUrl,
            }]
        );
        assert!(
            translator
                .new_session_to_client(acp::NewSessionRequest::new(r"C:\elsewhere"))
                .is_err()
        );

        let (load, _) = translator
            .load_session_to_client(acp::LoadSessionRequest::new(session_id(), r"i:\proj\"))
            .expect("the project folder maps");
        assert_eq!(load.cwd, PathBuf::from("/mnt/i/proj"));
        let (resume, _) = translator
            .resume_session_to_client(acp::ResumeSessionRequest::new(
                session_id(),
                r"\\wsl.localhost\Ubuntu\root\p",
            ))
            .expect("the project folder maps");
        assert_eq!(resume.cwd, PathBuf::from("/root/p"));
        let (fork, dropped) = translator
            .fork_session_to_client(
                acp::ForkSessionRequest::new(session_id(), r"I:\proj")
                    .additional_directories(vec![PathBuf::from(r"I:\proj\sub")]),
            )
            .expect("the project folder maps");
        assert_eq!(fork.cwd, PathBuf::from("/mnt/i/proj"));
        assert_eq!(
            fork.additional_directories,
            vec![PathBuf::from("/mnt/i/proj/sub")]
        );
        assert!(dropped.is_empty());
        assert!(
            translator
                .fork_session_to_client(acp::ForkSessionRequest::new(session_id(), r"C:\x"))
                .is_err()
        );
    }

    fn stdio(name: &str, command: &str, args: &[&str], env: &[(&str, &str)]) -> acp::McpServer {
        acp::McpServer::Stdio(
            acp::McpServerStdio::new(name, command)
                .args(args.iter().map(|argument| argument.to_string()).collect())
                .env(
                    env.iter()
                        .map(|(name, value)| acp::EnvVariable::new(*name, *value))
                        .collect(),
                ),
        )
    }

    fn http(name: &str, url: &str) -> acp::McpServer {
        acp::McpServer::Http(acp::McpServerHttp::new(name, url))
    }

    fn dropped(name: &str, reason: McpServerDropReason) -> DroppedMcpServer {
        DroppedMcpServer {
            name: name.to_string(),
            reason,
        }
    }

    #[test]
    fn sends_only_mcp_servers_the_sandbox_can_use() {
        let (servers, left_out) = translator().mcp_servers_to_client(vec![
            stdio("npx", "npx", &["-y", "@scope/server"], &[]),
            stdio(
                "tools",
                r"I:\proj\tools\server",
                &[r"I:\proj\config.json", "--verbose", "/usr/lib/x.js"],
                &[
                    ("DATA", r"\\wsl.localhost\Ubuntu\root\p\data"),
                    ("MODE", "fast"),
                ],
            ),
            stdio("script", "node", &[r"C:\Users\me\server.js"], &[]),
            stdio("python", r"C:\Python\python.exe", &[], &[]),
            stdio("path", "python", &[], &[("PATH", r"C:\Python;C:\Windows")]),
            http("localhost", "http://localhost:3000/mcp"),
            http("loopback", "http://127.0.0.2/mcp"),
            http("unspecified", "http://0.0.0.0:8080/mcp"),
            http("mapped", "http://[::ffff:127.0.0.1]:3000/mcp"),
            acp::McpServer::Sse(acp::McpServerSse::new("ipv6", "http://[::1]:3000/sse")),
            http("remote", "https://mcp.example.com/mcp"),
            http("broken", "not a url"),
            acp::McpServer::Acp(acp::McpServerAcp::new(
                "inline",
                acp::McpServerAcpId::new("inline"),
            )),
        ]);
        assert_eq!(
            servers,
            vec![
                stdio("npx", "npx", &["-y", "@scope/server"], &[]),
                stdio(
                    "tools",
                    "/mnt/i/proj/tools/server",
                    &["/mnt/i/proj/config.json", "--verbose", "/usr/lib/x.js"],
                    &[("DATA", "/root/p/data"), ("MODE", "fast")],
                ),
                http("remote", "https://mcp.example.com/mcp"),
            ]
        );
        let host_path = |path: &str| McpServerDropReason::HostPath(path.to_string());
        assert_eq!(
            left_out,
            vec![
                dropped("script", host_path(r"C:\Users\me\server.js")),
                dropped("python", host_path(r"C:\Python\python.exe")),
                dropped("path", host_path(r"C:\Python;C:\Windows")),
                dropped("localhost", McpServerDropReason::UnreachableUrl),
                dropped("loopback", McpServerDropReason::UnreachableUrl),
                dropped("unspecified", McpServerDropReason::UnreachableUrl),
                dropped("mapped", McpServerDropReason::UnreachableUrl),
                dropped("ipv6", McpServerDropReason::UnreachableUrl),
                dropped("broken", McpServerDropReason::UnreachableUrl),
                dropped("inline", McpServerDropReason::UnknownTransport),
            ]
        );
    }

    #[test]
    fn sends_mentions_inside_the_workspace_only() {
        let prompt = translator().prompt_to_client(vec![
            text("look at"),
            link("file:///I:/proj/%E6%97%A5%E6%9C%AC%E8%AA%9E%20a.rs"),
            embedded("file:///I:/proj/a.rs?symbol=main#L1:3"),
            link("file:///I:/proj/src/"),
            link(r"I:\proj\b.rs"),
            link(&skill(r"I:\proj\.zed\skills\s\SKILL.md")),
            embedded(&conflict(r"I:\proj\src\main.rs")),
            link("https://docs.rs/x"),
            link("file:////wsl.localhost/Ubuntu/root/p/a.rs"),
            // ここから下は、ワークツリーの外・パスを運ぶ Zed の URI・読み直せない URI。
            link("file:///C:/Users/me/.ssh/config"),
            link(" file:///C:/Users/me/.ssh/config"),
            link(r"C:\Users\me\.ssh\config"),
            link(&skill(r"C:\Users\me\.zed\skills\s\SKILL.md")),
            link("zed:///agent/thread/1?name=x"),
            // ユーザーが添付したものは、パスだけを落として本文を送る。
            embedded("file:///C:/Users/me/notes.md"),
            embedded(&conflict("src/main.rs")),
            embedded("zed:///agent/thread/1?name=x"),
            blob("file:///I:/proj/../secret.png"),
            image(Some("file:///C:/Users/me/a.png")),
            image(Some("zed:///agent/pasted-image?name=Image")),
        ]);
        assert_eq!(
            prompt,
            vec![
                text("look at"),
                link("file:///mnt/i/proj/%E6%97%A5%E6%9C%AC%E8%AA%9E%20a.rs"),
                embedded("file:///mnt/i/proj/a.rs?symbol=main#L1:3"),
                link("file:///mnt/i/proj/src/"),
                link("file:///mnt/i/proj/b.rs"),
                link(&skill("/mnt/i/proj/.zed/skills/s/SKILL.md")),
                embedded(&conflict("/mnt/i/proj/src/main.rs")),
                link("https://docs.rs/x"),
                link("file:///root/p/a.rs"),
                text("fn main() {}"),
                text("fn main() {}"),
                text("fn main() {}"),
                image(None),
                image(None),
            ]
        );
    }

    #[test]
    fn runs_the_login_command_inside_the_sandbox() {
        let translator = translator();
        let env = [
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                r"\\wsl.localhost\Ubuntu\home\u\.zed-jp-enhance-claude\projects\k\config"
                    .to_string(),
            ),
            ("NO_BROWSER".to_string(), "1".to_string()),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();
        let command = translator
            .host_command_to_client(
                Path::new(r"\\wsl.localhost\Ubuntu\home\u\.cache\zed-jp-enhance\node\bin\node"),
                &[
                    "/home/u/.cache/zed-jp-enhance/npm/cli.js".to_string(),
                    "login".to_string(),
                ],
                &env,
            )
            .expect("the agent's files are in the sandbox's distribution");
        assert_eq!(
            command,
            ClientCommand {
                program: "/home/u/.cache/zed-jp-enhance/node/bin/node".to_string(),
                args: vec![
                    "/home/u/.cache/zed-jp-enhance/npm/cli.js".to_string(),
                    "login".to_string(),
                ],
                env: [
                    (
                        "CLAUDE_CONFIG_DIR".to_string(),
                        "/home/u/.zed-jp-enhance-claude/projects/k/config".to_string(),
                    ),
                    ("NO_BROWSER".to_string(), "1".to_string()),
                ]
                .into_iter()
                .collect(),
            }
        );
        assert!(
            translator
                .host_command_to_client(Path::new(r"\\server\share\agent"), &[], &env)
                .is_err()
        );
        assert!(
            translator
                .host_command_to_client(
                    Path::new("node"),
                    &[r"\\wsl.localhost\Debian\x".to_string()],
                    &HashMap::default()
                )
                .is_err()
        );
    }

    #[test]
    fn refuses_projects_no_single_sandbox_can_see() {
        for (roots, distro) in [
            (
                vec![r"\\wsl.localhost\Ubuntu\a", r"\\wsl.localhost\Debian\b"],
                Some("Ubuntu"),
            ),
            (vec![r"\\wsl.localhost\Ubuntu\a"], None),
            (vec![r"\\wsl.localhost\Ubuntu\mnt\c\proj"], Some("Ubuntu")),
        ] {
            let roots = roots.into_iter().map(PathBuf::from).collect::<Vec<_>>();
            assert!(
                AcpPathTranslator::for_wsl(roots.clone(), distro).is_err(),
                "{roots:?} in {distro:?}"
            );
        }
        assert!(AcpPathTranslator::for_wsl(vec![PathBuf::from(r"I:\proj")], None).is_ok());
    }

    #[test]
    fn translates_nothing_but_the_workspace_for_an_identity_sandbox() {
        let translator = AcpPathTranslator::new(Arc::new(SandboxPaths::new(
            Arc::new(IdentityPathMapper),
            vec![PathBuf::from("/proj")],
        )));
        assert_eq!(
            translator
                .read_text_file_to_host(acp::ReadTextFileRequest::new(session_id(), "/proj/a.rs"))
                .map(|request| request.path),
            Ok(PathBuf::from("/proj/a.rs"))
        );
        assert_eq!(
            translator
                .read_text_file_to_host(acp::ReadTextFileRequest::new(session_id(), "/etc/passwd"))
                .map_err(|error| error.code),
            Err(acp::ErrorCode::ResourceNotFound)
        );
        assert_eq!(
            translator.prompt_to_client(vec![link("file:///proj/%E6%97%A5.rs")]),
            vec![link("file:///proj/%E6%97%A5.rs")]
        );
        assert_eq!(
            link_to_host(&translator, "file:///proj/src/"),
            Some("file:///proj/src/".to_string())
        );
        assert_eq!(link_to_host(&translator, "file:///etc/passwd"), None);
    }
}
