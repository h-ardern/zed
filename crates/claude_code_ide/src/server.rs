use crate::{
    diff_view::ClaudeDiffView,
    lock_file::{self, LockFile},
    tools,
};
use anyhow::{Context as _, Result};
use async_tungstenite::tungstenite::{
    Message,
    handshake::server::{Callback, ErrorResponse, Request, Response},
    http::{HeaderValue, StatusCode, header},
};
use collections::HashMap;
use editor::{Editor, EditorEvent, ToPoint as _};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Entity, EntityId, Subscription, Task,
    TaskExt as _, WeakEntity, Window,
};
use project::Project;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use workspace::{SaveIntent, Workspace};

const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
const MCP_SUBPROTOCOL: &str = "mcp";
const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const SELECTION_DEBOUNCE: Duration = Duration::from_millis(100);

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

pub type ClientId = usize;

/// Lets Claude Code connect to one workspace window over the same local WebSocket
/// protocol the VS Code extension uses.
pub struct IdeServer {
    pub(crate) workspace: WeakEntity<Workspace>,
    pub(crate) project: WeakEntity<Project>,
    pub(crate) window: AnyWindowHandle,
    port: u16,
    auth_token: String,
    lock_path: PathBuf,
    clients: HashMap<ClientId, mpsc::UnboundedSender<String>>,
    next_client_id: ClientId,
    pub(crate) latest_selection: Option<SelectionInfo>,
    last_sent_selection: Option<SelectionInfo>,
    pub(crate) diffs: HashMap<String, OpenDiff>,
    _active_editor_subscription: Option<Subscription>,
    _selection_task: Option<Task<()>>,
    _accept_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct OpenDiff {
    pub client_id: ClientId,
    pub view: WeakEntity<ClaudeDiffView>,
}

impl IdeServer {
    pub fn start(
        workspace: Entity<Workspace>,
        window: AnyWindowHandle,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .context("binding the Claude Code IDE server")?;
        let port = listener.local_addr()?.port();
        let listener = smol::net::TcpListener::try_from(listener)?;
        let lock_path = lock_file::lock_path(&lock_file::lock_dir(), port);
        Ok(cx.new(|cx| Self::new(workspace, window, listener, port, lock_path, cx)))
    }

    fn new(
        workspace: Entity<Workspace>,
        window: AnyWindowHandle,
        listener: smol::net::TcpListener,
        port: u16,
        lock_path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let auth_token = lock_file::generate_auth_token();
        let project = workspace.read(cx).project().clone();

        let accept_task = cx.spawn({
            let auth_token = auth_token.clone();
            async move |this, cx| {
                loop {
                    let stream = match listener.accept().await {
                        Ok((stream, _)) => stream,
                        Err(error) => {
                            log::error!("Claude Code IDE server stopped accepting: {error}");
                            break;
                        }
                    };
                    let this = this.clone();
                    let auth_token = auth_token.clone();
                    cx.spawn(async move |cx| {
                        if let Err(error) = handle_connection(this, stream, auth_token, cx).await {
                            log::debug!("Claude Code connection ended: {error:#}");
                        }
                    })
                    .detach();
                }
            }
        });

        let subscriptions = vec![
            cx.subscribe(
                &workspace,
                |this, workspace, event: &workspace::Event, cx| {
                    if let workspace::Event::ActiveItemChanged = event {
                        this.watch_active_editor(&workspace, cx);
                    }
                },
            ),
            cx.subscribe(&project, |this, _, event: &project::Event, cx| {
                if matches!(
                    event,
                    project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
                ) {
                    this.write_lock_file(cx);
                }
            }),
            // Dropping the server removes the lock file, but quitting may skip drops.
            cx.on_app_quit(|this, _| {
                lock_file::remove(&this.lock_path);
                async {}
            }),
        ];

        let mut this = Self {
            workspace: workspace.downgrade(),
            project: project.downgrade(),
            window,
            port,
            auth_token,
            lock_path,
            clients: HashMap::default(),
            next_client_id: 0,
            latest_selection: None,
            last_sent_selection: None,
            diffs: HashMap::default(),
            _active_editor_subscription: None,
            _selection_task: None,
            _accept_task: accept_task,
            _subscriptions: subscriptions,
        };
        this.write_lock_file(cx);
        this.set_terminal_env(true, cx);
        this.watch_active_editor(&workspace, cx);
        this
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stops new terminals from pointing Claude Code at this server. The lock file is
    /// removed when the server is dropped.
    pub fn shutdown(&mut self, cx: &mut App) {
        self.set_terminal_env(false, cx);
    }

    fn workspace_folders(&self, cx: &App) -> Vec<String> {
        let Some(project) = self.project.upgrade() else {
            return Vec::new();
        };
        project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
            .collect()
    }

    fn write_lock_file(&self, cx: &App) {
        let lock_file = LockFile::new(self.workspace_folders(cx), self.auth_token.clone());
        if let Err(error) = lock_file::write(&self.lock_path, &lock_file) {
            log::error!("failed to write Claude Code lock file: {error:#}");
        }
    }

    /// Terminals opened in this window get the variables Claude Code uses to connect
    /// automatically. Remote projects are skipped because their terminals can't reach
    /// this machine's loopback address.
    fn set_terminal_env(&self, enabled: bool, cx: &mut App) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let mut env = HashMap::default();
        if enabled {
            env.extend(crate::connect_env(self.port));
        }
        project.update(cx, |project, _| {
            if project.is_local() {
                project.set_extra_terminal_env(env);
            }
        });
    }

    fn add_client(&mut self, sender: mpsc::UnboundedSender<String>) -> ClientId {
        let client_id = self.next_client_id;
        self.next_client_id += 1;
        self.clients.insert(client_id, sender);
        client_id
    }

    fn remove_client(&mut self, client_id: ClientId, cx: &mut Context<Self>) {
        self.clients.remove(&client_id);
        // A disconnected client can never answer its diffs, so reject them rather than
        // leave tabs waiting forever.
        let orphaned_diffs = self
            .diffs
            .iter()
            .filter(|(_, diff)| diff.client_id == client_id)
            .map(|(tab_name, _)| tab_name.clone())
            .collect::<Vec<_>>();
        for tab_name in orphaned_diffs {
            self.close_diff(&tab_name, cx);
        }
    }

    pub(crate) fn broadcast(&mut self, method: &str, params: Value) {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        })
        .to_string();
        self.clients
            .retain(|_, sender| sender.unbounded_send(message.clone()).is_ok());
    }

    /// Sends Claude an `@file#Lstart-end` mention for the given selection.
    pub fn mention(&mut self, selection: &SelectionInfo) {
        self.broadcast(
            "at_mentioned",
            json!({
                "filePath": selection.file_path,
                "lineStart": selection.start.line,
                "lineEnd": selection.last_line(),
            }),
        );
    }

    fn watch_active_editor(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let editor = workspace
            .read(cx)
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx));
        self._active_editor_subscription = editor.as_ref().map(|editor| {
            cx.subscribe(editor, |this, editor, event: &EditorEvent, cx| {
                if let EditorEvent::SelectionsChanged { .. } = event {
                    this.schedule_selection_update(editor, cx);
                }
            })
        });
        if let Some(editor) = editor {
            self.schedule_selection_update(editor, cx);
        }
    }

    fn schedule_selection_update(&mut self, editor: Entity<Editor>, cx: &mut Context<Self>) {
        self._selection_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SELECTION_DEBOUNCE).await;
            this.update(cx, |this, cx| this.publish_selection(&editor, cx))
                .ok();
        }));
    }

    fn publish_selection(&mut self, editor: &Entity<Editor>, cx: &App) {
        let Some(selection) = SelectionInfo::from_editor(editor, cx) else {
            return;
        };
        if !selection.is_empty() {
            self.latest_selection = Some(selection.clone());
        }
        if self.last_sent_selection.as_ref() != Some(&selection) {
            self.broadcast("selection_changed", selection.to_notification());
            self.last_sent_selection = Some(selection);
        }
    }

    pub(crate) fn close_diff(&mut self, tab_name: &str, cx: &mut App) -> bool {
        let Some(diff) = self.diffs.remove(tab_name) else {
            return false;
        };
        let Some(view) = diff.view.upgrade() else {
            return true;
        };
        close_item(&self.workspace, self.window, view.entity_id(), cx);
        true
    }

    pub(crate) fn close_all_diffs(&mut self, cx: &mut App) -> usize {
        let tab_names = self.diffs.keys().cloned().collect::<Vec<_>>();
        tab_names
            .iter()
            .filter(|tab_name| self.close_diff(tab_name, cx))
            .count()
    }
}

impl Drop for IdeServer {
    fn drop(&mut self) {
        lock_file::remove(&self.lock_path);
    }
}

/// Closes a workspace item from code that isn't running inside that window's update.
pub(crate) fn close_item(
    workspace: &WeakEntity<Workspace>,
    window: AnyWindowHandle,
    item_id: EntityId,
    cx: &mut App,
) {
    window
        .update(cx, |_, window, cx| {
            close_item_in_window(workspace, item_id, window, cx)
        })
        .ok();
}

/// Closes a workspace item without prompting. The close is deferred so it runs after
/// the current update, since the pane reads the item while closing it.
pub(crate) fn close_item_in_window(
    workspace: &WeakEntity<Workspace>,
    item_id: EntityId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let panes = workspace.read(cx).panes().to_vec();
    window.defer(cx, move |window, cx| {
        for pane in panes {
            pane.update(cx, |pane, cx| {
                pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                    .detach_and_log_err(cx);
            });
        }
    });
}

async fn handle_connection(
    this: WeakEntity<IdeServer>,
    stream: smol::net::TcpStream,
    auth_token: String,
    cx: &mut AsyncApp,
) -> Result<()> {
    let websocket = async_tungstenite::accept_hdr_async(stream, AuthCheck { auth_token })
        .await
        .context("Claude Code websocket handshake")?;

    let (mut sink, mut incoming) = websocket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<String>();
    let client_id = this.update(cx, |this, _| this.add_client(outgoing_tx.clone()))?;

    let _writer = cx.background_spawn(async move {
        while let Some(text) = outgoing_rx.next().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(message) = incoming.next().await {
        let text = match message {
            Ok(Message::Text(text)) => text.as_str().to_owned(),
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(error) => {
                log::debug!("Claude Code websocket error: {error}");
                break;
            }
        };
        let request = match serde_json::from_str::<Value>(&text) {
            Ok(request) => request,
            Err(error) => {
                log::warn!("Claude Code sent invalid JSON: {error}");
                continue;
            }
        };
        // Notifications such as `ide_connected` and `notifications/initialized` need no reply.
        let (Some(method), Some(id)) = (
            request.get("method").and_then(Value::as_str),
            request.get("id").cloned(),
        ) else {
            continue;
        };
        let method = method.to_string();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let this = this.clone();
        let outgoing_tx = outgoing_tx.clone();
        // Each request runs on its own, so a blocking `openDiff` doesn't hold up the rest.
        cx.spawn(async move |cx| {
            let response = match handle_request(&this, client_id, &method, params, cx).await {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(error) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": error.code, "message": error.message },
                }),
            };
            outgoing_tx.unbounded_send(response.to_string()).ok();
        })
        .detach();
    }

    this.update(cx, |this, cx| this.remove_client(client_id, cx))
        .ok();
    Ok(())
}

/// Rejects the WebSocket upgrade unless it carries this server's auth token.
struct AuthCheck {
    auth_token: String,
}

impl Callback for AuthCheck {
    fn on_request(
        self,
        request: &Request,
        response: Response,
    ) -> std::result::Result<Response, ErrorResponse> {
        let provided = request
            .headers()
            .get(AUTH_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if constant_time_eq(provided.as_bytes(), self.auth_token.as_bytes()) {
            let mut response = response;
            // Claude Code requests the `mcp` subprotocol, and its WebSocket client drops
            // the connection unless the server echoes back one of the requested protocols.
            if let Some(protocol) = select_subprotocol(request) {
                response
                    .headers_mut()
                    .insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
            }
            Ok(response)
        } else {
            let mut error =
                ErrorResponse::new(Some("invalid Claude Code IDE auth token".to_string()));
            *error.status_mut() = StatusCode::UNAUTHORIZED;
            Err(error)
        }
    }
}

fn select_subprotocol(request: &Request) -> Option<HeaderValue> {
    let requested = request
        .headers()
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|protocol| !protocol.is_empty())
        .collect::<Vec<_>>();
    if requested.contains(&MCP_SUBPROTOCOL) {
        return Some(HeaderValue::from_static(MCP_SUBPROTOCOL));
    }
    requested
        .first()
        .and_then(|protocol| HeaderValue::from_str(protocol).ok())
}

async fn handle_request(
    this: &WeakEntity<IdeServer>,
    client_id: ClientId,
    method: &str,
    params: Value,
    cx: &mut AsyncApp,
) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .cloned()
                .unwrap_or_else(|| json!(MCP_PROTOCOL_VERSION)),
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": {
                "name": "ded-claude-code",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::definitions() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("missing tool name"))?
                .to_string();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            tools::call(this, client_id, &name, arguments, cx).await
        }
        "prompts/list" => Ok(json!({ "prompts": [] })),
        "resources/list" => Ok(json!({ "resources": [] })),
        _ => Err(RpcError::method_not_found(method)),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: METHOD_NOT_FOUND,
            message: format!("method not found: {method}"),
        }
    }

    pub fn invalid_params(message: &str) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: message.to_string(),
        }
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message: format!("{error:#}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    fn to_json(self) -> Value {
        json!({ "line": self.line, "character": self.character })
    }
}

/// A selection in a file, with 0-based lines and UTF-16 columns as VS Code reports them.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionInfo {
    pub text: String,
    pub file_path: String,
    pub start: Position,
    pub end: Position,
}

impl SelectionInfo {
    pub fn from_editor(editor: &Entity<Editor>, cx: &App) -> Option<Self> {
        let editor = editor.read(cx);
        let multibuffer = editor.buffer().read(cx);
        let buffer = multibuffer.as_singleton()?;
        let abs_path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
        let snapshot = multibuffer.snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let start = selection.start.to_point_utf16(&snapshot);
        let end = selection.end.to_point_utf16(&snapshot);
        let text = snapshot
            .text_for_range(selection.start..selection.end)
            .collect();
        Some(Self {
            text,
            file_path: abs_path.to_string_lossy().into_owned(),
            start: Position {
                line: start.row,
                character: start.column,
            },
            end: Position {
                line: end.row,
                character: end.column,
            },
        })
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// The last selected line. A selection that ends at the start of a line doesn't
    /// include that line.
    pub fn last_line(&self) -> u32 {
        if self.end.line > self.start.line && self.end.character == 0 {
            self.end.line - 1
        } else {
            self.end.line
        }
    }

    fn selection_json(&self) -> Value {
        json!({
            "start": self.start.to_json(),
            "end": self.end.to_json(),
            "isEmpty": self.is_empty(),
        })
    }

    fn to_notification(&self) -> Value {
        json!({
            "text": self.text,
            "filePath": self.file_path,
            "fileUrl": file_url(&self.file_path),
            "selection": self.selection_json(),
        })
    }

    pub(crate) fn to_tool_result(&self) -> Value {
        json!({
            "success": true,
            "text": self.text,
            "filePath": self.file_path,
            "selection": self.selection_json(),
        })
    }
}

pub(crate) fn file_url(path: &str) -> String {
    url::Url::from_file_path(path)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| format!("file://{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    /// Runs the handshake check, returning the status and any echoed subprotocol.
    fn handshake(token: &str, protocols: Option<&str>) -> (StatusCode, Option<String>) {
        let mut builder = Request::builder()
            .uri("ws://127.0.0.1/")
            .header(AUTH_HEADER, token);
        if let Some(protocols) = protocols {
            builder = builder.header(header::SEC_WEBSOCKET_PROTOCOL, protocols);
        }
        let request = builder.body(()).expect("valid request");
        let check = AuthCheck {
            auth_token: "secret".to_string(),
        };
        match check.on_request(&request, Response::default()) {
            Ok(response) => (
                response.status(),
                response
                    .headers()
                    .get(header::SEC_WEBSOCKET_PROTOCOL)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
            ),
            Err(error) => (error.status(), None),
        }
    }

    #[test]
    fn test_handshake_echoes_mcp_subprotocol() {
        let accepted = |protocol: Option<&str>| (StatusCode::OK, protocol.map(str::to_string));
        assert_eq!(handshake("secret", Some("mcp")), accepted(Some("mcp")));
        assert_eq!(
            handshake("secret", Some("other, mcp")),
            accepted(Some("mcp"))
        );
        assert_eq!(handshake("secret", Some("other")), accepted(Some("other")));
        assert_eq!(handshake("secret", None), accepted(None));
    }

    #[test]
    fn test_handshake_rejects_wrong_token() {
        assert_eq!(
            handshake("wrong", Some("mcp")),
            (StatusCode::UNAUTHORIZED, None)
        );
    }

    #[test]
    fn test_last_line_excludes_line_where_selection_ends_at_column_zero() {
        let selection = |end_line, end_character| SelectionInfo {
            text: String::new(),
            file_path: "/tmp/file.rs".into(),
            start: Position {
                line: 2,
                character: 0,
            },
            end: Position {
                line: end_line,
                character: end_character,
            },
        };
        assert_eq!(selection(5, 0).last_line(), 4);
        assert_eq!(selection(5, 3).last_line(), 5);
        assert_eq!(selection(2, 0).last_line(), 2);
        assert!(selection(2, 0).is_empty());
    }
}
