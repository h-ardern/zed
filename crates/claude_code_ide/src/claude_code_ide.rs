//! Lets the Claude Code CLI connect to Ded the way it connects to VS Code: each window
//! runs a local WebSocket MCP server and advertises it in `~/.claude/ide/<port>.lock`.
//! Claude started in Ded's terminal connects automatically; Claude started in any other
//! terminal connects with `/ide`.

mod diff_view;
mod lock_file;
mod server;
mod tools;

use collections::HashMap;
use editor::Editor;
use gpui::{AnyWindowHandle, App, ClipboardItem, Entity, EntityId, Global, WeakEntity, actions};
use settings::{RegisterSetting, Settings, SettingsStore};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use workspace::Workspace;

pub use server::{IdeServer, SelectionInfo};

actions!(
    claude_code,
    [
        /// Sends the current selection to connected Claude Code sessions as an @-mention.
        SendSelection,
        /// Opens a terminal running Claude Code, connected to this window.
        Launch,
        /// Copies a command that starts Claude Code connected to this window, for use in
        /// a terminal outside Ded.
        CopyConnectCommand,
        /// Accepts the change Claude Code proposed in this diff.
        AcceptDiff,
        /// Rejects the change Claude Code proposed in this diff.
        RejectDiff,
    ]
);

#[derive(Clone, Copy, Debug, RegisterSetting)]
pub struct ClaudeCodeSettings {
    pub enabled: bool,
}

impl Settings for ClaudeCodeSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            enabled: content
                .claude_code
                .as_ref()
                .and_then(|claude_code| claude_code.enabled)
                .unwrap_or(true),
        }
    }
}

struct WindowEntry {
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    server: Option<Entity<IdeServer>>,
}

/// One server per workspace window, keyed by the workspace's entity id.
struct IdeServers {
    entries: HashMap<EntityId, WindowEntry>,
    enabled: bool,
}

impl Global for IdeServers {}

pub fn init(cx: &mut App) {
    lock_file::remove_stale(&lock_file::lock_dir());
    cx.set_global(IdeServers {
        entries: HashMap::default(),
        enabled: ClaudeCodeSettings::get_global(cx).enabled,
    });

    cx.observe_global::<SettingsStore>(|cx| {
        let enabled = ClaudeCodeSettings::get_global(cx).enabled;
        if cx.global::<IdeServers>().enabled == enabled {
            return;
        }
        cx.global_mut::<IdeServers>().enabled = enabled;
        let workspace_ids = cx
            .global::<IdeServers>()
            .entries
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for workspace_id in workspace_ids {
            if enabled {
                start_server(workspace_id, cx);
            } else {
                stop_server(workspace_id, cx);
            }
        }
    })
    .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace_id = cx.entity_id();
        let entry = WindowEntry {
            workspace: cx.weak_entity(),
            window: window.window_handle(),
            server: None,
        };
        cx.global_mut::<IdeServers>()
            .entries
            .insert(workspace_id, entry);
        cx.on_release(move |_, cx| {
            stop_server(workspace_id, cx);
            cx.global_mut::<IdeServers>().entries.remove(&workspace_id);
        })
        .detach();
        // The workspace is still being built here, and the server reads it when starting.
        cx.defer(move |cx| {
            if cx.global::<IdeServers>().enabled {
                start_server(workspace_id, cx);
            }
        });
        register_actions(workspace);
    })
    .detach();
}

fn start_server(workspace_id: EntityId, cx: &mut App) {
    let Some(entry) = cx.global::<IdeServers>().entries.get(&workspace_id) else {
        return;
    };
    if entry.server.is_some() {
        return;
    }
    let window = entry.window;
    let Some(workspace) = entry.workspace.upgrade() else {
        return;
    };
    match IdeServer::start(workspace, window, cx) {
        Ok(server) => {
            if let Some(entry) = cx.global_mut::<IdeServers>().entries.get_mut(&workspace_id) {
                entry.server = Some(server);
            }
        }
        Err(error) => log::error!("failed to start the Claude Code IDE server: {error:#}"),
    }
}

fn stop_server(workspace_id: EntityId, cx: &mut App) {
    let server = cx
        .global_mut::<IdeServers>()
        .entries
        .get_mut(&workspace_id)
        .and_then(|entry| entry.server.take());
    if let Some(server) = server {
        server.update(cx, |server, cx| server.shutdown(cx));
    }
}

fn server_for(workspace_id: EntityId, cx: &App) -> Option<Entity<IdeServer>> {
    cx.global::<IdeServers>()
        .entries
        .get(&workspace_id)?
        .server
        .clone()
}

fn register_actions(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &SendSelection, _, cx| {
        let Some(server) = server_for(cx.entity_id(), cx) else {
            return;
        };
        let Some(selection) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            .and_then(|editor| SelectionInfo::from_editor(&editor, cx))
        else {
            return;
        };
        server.update(cx, |server, _| server.mention(&selection));
    });

    workspace.register_action(|workspace, _: &Launch, window, cx| {
        let Some(server) = server_for(cx.entity_id(), cx) else {
            log::warn!("Claude Code IDE integration is disabled; enable `claude_code.enabled`");
            return;
        };
        let port = server.read(cx).port();
        let cwd = workspace
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
        let spawn = SpawnInTerminal {
            id: TaskId("claude-code".to_string()),
            full_label: "Claude Code".to_string(),
            label: "Claude Code".to_string(),
            command: Some("claude".to_string()),
            command_label: "claude".to_string(),
            cwd,
            env: connect_env(port).into_iter().collect(),
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            ..SpawnInTerminal::default()
        };
        workspace.spawn_in_terminal(spawn, window, cx).detach();
    });

    workspace.register_action(|_, _: &CopyConnectCommand, _, cx| {
        let Some(server) = server_for(cx.entity_id(), cx) else {
            log::warn!("Claude Code IDE integration is disabled; enable `claude_code.enabled`");
            return;
        };
        let port = server.read(cx).port();
        let env = connect_env(port)
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        cx.write_to_clipboard(ClipboardItem::new_string(format!("{env} claude")));
    });
}

/// The variables that make Claude Code connect to this window's server on startup.
fn connect_env(port: u16) -> [(String, String); 2] {
    [
        ("CLAUDE_CODE_SSE_PORT".to_string(), port.to_string()),
        ("ENABLE_IDE_INTEGRATION".to_string(), "true".to_string()),
    ]
}
