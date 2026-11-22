//! The MCP tools Claude Code calls on the IDE. Names, inputs and result shapes follow the
//! VS Code extension, as documented by claudecode.nvim's PROTOCOL.md.

use crate::{
    diff_view::{ClaudeDiffView, DiffOutcome, DiffRequest},
    server::{ClientId, IdeServer, OpenDiff, RpcError, SelectionInfo, file_url},
};
use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use futures::channel::oneshot;
use gpui::{AppContext as _, AsyncApp, Context, Entity, WeakEntity, Window};
use language::{Buffer, BufferSnapshot, Point, PointUtf16};
use lsp::DiagnosticSeverity;
use project::Project;
use serde_json::{Value, json};
use std::{
    ops::Range,
    path::{Path, PathBuf},
};
use workspace::{OpenOptions, Workspace};

pub(crate) fn definitions() -> Vec<Value> {
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
            },
        })
    };
    let file_path =
        json!({ "filePath": { "type": "string", "description": "Absolute path of the file" } });
    vec![
        tool(
            "openFile",
            "Open a file in the editor, optionally selecting a range of text",
            json!({
                "filePath": { "type": "string", "description": "Absolute path of the file to open" },
                "preview": { "type": "boolean", "default": false },
                "startText": { "type": "string", "description": "Text that starts the selection" },
                "endText": { "type": "string", "description": "Text that ends the selection" },
                "selectToEndOfLine": { "type": "boolean", "default": false },
                "makeFrontmost": { "type": "boolean", "default": true },
            }),
            &["filePath"],
        ),
        tool(
            "openDiff",
            "Show proposed changes to a file and wait for the user to accept or reject them",
            json!({
                "old_file_path": { "type": "string" },
                "new_file_path": { "type": "string" },
                "new_file_contents": { "type": "string" },
                "tab_name": { "type": "string" },
            }),
            &[
                "old_file_path",
                "new_file_path",
                "new_file_contents",
                "tab_name",
            ],
        ),
        tool(
            "getCurrentSelection",
            "Get the selection in the active editor",
            json!({}),
            &[],
        ),
        tool(
            "getLatestSelection",
            "Get the most recent non-empty selection in any editor",
            json!({}),
            &[],
        ),
        tool(
            "getOpenEditors",
            "List the files open in editor tabs",
            json!({}),
            &[],
        ),
        tool(
            "getWorkspaceFolders",
            "List the folders open in this window",
            json!({}),
            &[],
        ),
        tool(
            "getDiagnostics",
            "Get language server diagnostics for one file, or for every file that has any",
            json!({ "uri": { "type": "string", "description": "file:// URI; omit for all files" } }),
            &[],
        ),
        tool(
            "checkDocumentDirty",
            "Check whether an open file has unsaved changes",
            file_path.clone(),
            &["filePath"],
        ),
        tool(
            "saveDocument",
            "Save an open file",
            file_path,
            &["filePath"],
        ),
        tool(
            "close_tab",
            "Close a diff tab by name",
            json!({ "tab_name": { "type": "string" } }),
            &["tab_name"],
        ),
        tool("closeAllDiffTabs", "Close every diff tab", json!({}), &[]),
        tool(
            "executeCode",
            "Run code in a Jupyter kernel (not supported in Ded)",
            json!({ "code": { "type": "string" } }),
            &["code"],
        ),
    ]
}

pub(crate) async fn call(
    server: &WeakEntity<IdeServer>,
    client_id: ClientId,
    name: &str,
    arguments: Value,
    cx: &mut AsyncApp,
) -> Result<Value, RpcError> {
    let result = match name {
        "openFile" => open_file(server, &arguments, cx).await,
        "openDiff" => return open_diff(server, client_id, &arguments, cx).await,
        "getCurrentSelection" => get_current_selection(server, cx),
        "getLatestSelection" => get_latest_selection(server, cx),
        "getOpenEditors" => get_open_editors(server, cx),
        "getWorkspaceFolders" => get_workspace_folders(server, cx),
        "getDiagnostics" => get_diagnostics(server, &arguments, cx).await,
        "checkDocumentDirty" => check_document_dirty(server, &arguments, cx),
        "saveDocument" => save_document(server, &arguments, cx).await,
        "close_tab" => close_tab(server, &arguments, cx),
        "closeAllDiffTabs" => close_all_diff_tabs(server, cx),
        "executeCode" => Err(anyhow!(
            "executeCode runs Jupyter notebook cells, which Ded doesn't support"
        )),
        _ => return Err(RpcError::method_not_found(name)),
    };
    Ok(match result {
        Ok(contents) => tool_result(contents, false),
        Err(error) => tool_result(vec![format!("{error:#}")], true),
    })
}

fn tool_result(contents: Vec<String>, is_error: bool) -> Value {
    let content = contents
        .into_iter()
        .map(|text| json!({ "type": "text", "text": text }))
        .collect::<Vec<_>>();
    if is_error {
        json!({ "content": content, "isError": true })
    } else {
        json!({ "content": content })
    }
}

fn required_str(arguments: &Value, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("missing required argument `{key}`"))
}

fn optional_str(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn optional_bool(arguments: &Value, key: &str, default: bool) -> bool {
    arguments
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn server_handles(
    server: &WeakEntity<IdeServer>,
    cx: &mut AsyncApp,
) -> Result<(
    WeakEntity<Workspace>,
    Entity<Project>,
    gpui::AnyWindowHandle,
)> {
    let (workspace, project, window) = server.read_with(cx, |server, _| {
        (
            server.workspace.clone(),
            server.project.clone(),
            server.window,
        )
    })?;
    let project = project.upgrade().context("the project was closed")?;
    Ok((workspace, project, window))
}

/// Runs `f` against this server's workspace with its window, which most UI actions need.
fn with_workspace<R>(
    server: &WeakEntity<IdeServer>,
    cx: &mut AsyncApp,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> R,
) -> Result<R> {
    let (workspace, _, window) = server_handles(server, cx)?;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| f(workspace, window, cx))
    })?
}

async fn open_file(
    server: &WeakEntity<IdeServer>,
    arguments: &Value,
    cx: &mut AsyncApp,
) -> Result<Vec<String>> {
    let file_path = required_str(arguments, "filePath")?;
    let start_text = optional_str(arguments, "startText");
    let end_text = optional_str(arguments, "endText");
    let select_to_end_of_line = optional_bool(arguments, "selectToEndOfLine", false);
    let make_frontmost = optional_bool(arguments, "makeFrontmost", true);

    let open_task = with_workspace(server, cx, |workspace, window, cx| {
        workspace.open_abs_path(
            PathBuf::from(&file_path),
            OpenOptions {
                focus: Some(make_frontmost),
                ..OpenOptions::default()
            },
            window,
            cx,
        )
    })?;
    let item = open_task.await?;

    let mut language_id = "plaintext".to_string();
    let mut line_count = 0;
    if let Some(editor) = cx.update(|cx| item.act_as::<Editor>(cx)) {
        let (_, _, window) = server_handles(server, cx)?;
        cx.update_window(window, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
                    return;
                };
                let snapshot = buffer.read(cx).snapshot();
                line_count = snapshot.max_point().row + 1;
                if let Some(language) = snapshot.language() {
                    language_id = language.lsp_id();
                }
                if let Some(range) = text_range(
                    &snapshot,
                    start_text.as_deref(),
                    end_text.as_deref(),
                    select_to_end_of_line,
                ) {
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_ranges([range]);
                    });
                }
            });
        })?;
    }

    if make_frontmost {
        Ok(vec![format!("Opened file: {file_path}")])
    } else {
        Ok(vec![
            json!({
                "success": true,
                "filePath": file_path,
                "languageId": language_id,
                "lineCount": line_count,
            })
            .to_string(),
        ])
    }
}

/// Finds the range from the first `start_text` to the end of the following `end_text`.
fn text_range(
    snapshot: &BufferSnapshot,
    start_text: Option<&str>,
    end_text: Option<&str>,
    select_to_end_of_line: bool,
) -> Option<Range<Point>> {
    let start_text = start_text.filter(|text| !text.is_empty())?;
    let text = snapshot.text();
    let start = text.find(start_text)?;
    let end = match end_text.filter(|text| !text.is_empty()) {
        Some(end_text) => start + text.get(start..)?.find(end_text)? + end_text.len(),
        None => start + start_text.len(),
    };
    let start_point = snapshot.offset_to_point(start);
    let mut end_point = snapshot.offset_to_point(end);
    if select_to_end_of_line {
        end_point = Point::new(end_point.row, snapshot.line_len(end_point.row));
    }
    Some(start_point..end_point)
}

async fn open_diff(
    server: &WeakEntity<IdeServer>,
    client_id: ClientId,
    arguments: &Value,
    cx: &mut AsyncApp,
) -> Result<Value, RpcError> {
    let old_file_path = PathBuf::from(required_str(arguments, "old_file_path")?);
    let new_file_path = PathBuf::from(required_str(arguments, "new_file_path")?);
    let new_file_contents = required_str(arguments, "new_file_contents")?;
    let tab_name = required_str(arguments, "tab_name")?;

    // Opening a diff under a tab name that's already open replaces that diff.
    server.update(cx, |server, cx| {
        server.close_diff(&tab_name, cx);
    })?;

    let (workspace, project, window) = server_handles(server, cx)?;
    let target_buffer = open_existing_buffer(&project, &new_file_path, cx).await?;
    let base_text = if old_file_path == new_file_path {
        target_buffer
            .as_ref()
            .map(|buffer| buffer.read_with(cx, |buffer, _| buffer.text()))
    } else {
        open_existing_buffer(&project, &old_file_path, cx)
            .await?
            .map(|buffer| buffer.read_with(cx, |buffer, _| buffer.text()))
    }
    .unwrap_or_default();

    let (outcome_tx, outcome_rx) = oneshot::channel();
    let request = DiffRequest {
        target_path: new_file_path,
        target_buffer,
        base_text,
        proposed_text: new_file_contents,
        tab_name: tab_name.clone(),
        outcome_tx,
    };
    let view = ClaudeDiffView::open(request, workspace, window, cx)
        .await?
        .downgrade();
    // Only a weak handle is kept, so closing the tab drops the view and resolves the
    // outcome as rejected.
    let view_id = view.entity_id();
    server.update(cx, |server, _| {
        server
            .diffs
            .insert(tab_name.clone(), OpenDiff { client_id, view });
    })?;

    let outcome = outcome_rx.await.unwrap_or(DiffOutcome::Rejected);
    server
        .update(cx, |server, _| {
            if server
                .diffs
                .get(&tab_name)
                .is_some_and(|diff| diff.view.entity_id() == view_id)
            {
                server.diffs.remove(&tab_name);
            }
        })
        .ok();

    Ok(match outcome {
        DiffOutcome::Accepted { final_contents } => {
            tool_result(vec!["FILE_SAVED".to_string(), final_contents], false)
        }
        DiffOutcome::Rejected => tool_result(vec!["DIFF_REJECTED".to_string(), tab_name], false),
    })
}

/// Opens the file as a buffer if it exists on disk. A missing file is a new file Claude
/// wants to create, which has no buffer yet.
async fn open_existing_buffer(
    project: &Entity<Project>,
    path: &Path,
    cx: &mut AsyncApp,
) -> Result<Option<Entity<Buffer>>> {
    if smol::fs::metadata(path).await.is_err() {
        return Ok(None);
    }
    let buffer = project
        .update(cx, |project, cx| project.open_local_buffer(path, cx))
        .await?;
    Ok(Some(buffer))
}

fn get_current_selection(server: &WeakEntity<IdeServer>, cx: &mut AsyncApp) -> Result<Vec<String>> {
    let (workspace, _, _) = server_handles(server, cx)?;
    let selection = workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            .and_then(|editor| SelectionInfo::from_editor(&editor, cx))
    })?;
    let result = match selection {
        Some(selection) => selection.to_tool_result(),
        None => json!({ "success": false, "message": "No active editor found" }),
    };
    Ok(vec![result.to_string()])
}

fn get_latest_selection(server: &WeakEntity<IdeServer>, cx: &mut AsyncApp) -> Result<Vec<String>> {
    let selection = server.read_with(cx, |server, _| server.latest_selection.clone())?;
    let result = match selection {
        Some(selection) => selection.to_tool_result(),
        None => json!({ "success": false, "message": "No selection available" }),
    };
    Ok(vec![result.to_string()])
}

fn get_open_editors(server: &WeakEntity<IdeServer>, cx: &mut AsyncApp) -> Result<Vec<String>> {
    let (workspace, _, _) = server_handles(server, cx)?;
    let tabs = workspace.read_with(cx, |workspace, cx| {
        let project = workspace.project().read(cx);
        let active_item_id = workspace.active_item(cx).map(|item| item.item_id());
        let mut tabs = Vec::new();
        for pane in workspace.panes() {
            for item in pane.read(cx).items() {
                let Some(abs_path) = item
                    .project_path(cx)
                    .and_then(|project_path| project.absolute_path(&project_path, cx))
                else {
                    continue;
                };
                let path = abs_path.to_string_lossy().into_owned();
                let language_id = item
                    .act_as::<Editor>(cx)
                    .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton())
                    .and_then(|buffer| buffer.read(cx).language().map(|language| language.lsp_id()))
                    .unwrap_or_else(|| "plaintext".to_string());
                tabs.push(json!({
                    "uri": file_url(&path),
                    "isActive": Some(item.item_id()) == active_item_id,
                    "label": item.tab_content_text(0, cx).to_string(),
                    "languageId": language_id,
                    "isDirty": item.is_dirty(cx),
                }));
            }
        }
        tabs
    })?;
    Ok(vec![json!({ "tabs": tabs }).to_string()])
}

fn get_workspace_folders(server: &WeakEntity<IdeServer>, cx: &mut AsyncApp) -> Result<Vec<String>> {
    let (workspace, _, _) = server_handles(server, cx)?;
    let folders = workspace.read_with(cx, |workspace, cx| {
        workspace
            .visible_worktrees(cx)
            .map(|worktree| {
                let path = worktree.read(cx).abs_path().to_string_lossy().into_owned();
                let name = Path::new(&path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                json!({ "name": name, "uri": file_url(&path), "path": path })
            })
            .collect::<Vec<_>>()
    })?;
    let root_path = folders
        .first()
        .and_then(|folder| folder.get("path"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(vec![
        json!({ "success": true, "folders": folders, "rootPath": root_path }).to_string(),
    ])
}

async fn get_diagnostics(
    server: &WeakEntity<IdeServer>,
    arguments: &Value,
    cx: &mut AsyncApp,
) -> Result<Vec<String>> {
    let (_, project, _) = server_handles(server, cx)?;
    let mut paths = match optional_str(arguments, "uri") {
        Some(uri) => vec![path_from_uri(&uri)],
        None => project.read_with(cx, |project, cx| {
            project
                .diagnostic_summaries(false, cx)
                .filter(|(_, _, summary)| summary.error_count + summary.warning_count > 0)
                .filter_map(|(project_path, _, _)| project.absolute_path(&project_path, cx))
                .collect::<Vec<_>>()
        }),
    };
    // A file can have diagnostics from several language servers.
    paths.sort();
    paths.dedup();

    let mut files = Vec::new();
    for path in paths {
        let buffer = match project
            .update(cx, |project, cx| project.open_local_buffer(&path, cx))
            .await
        {
            Ok(buffer) => buffer,
            Err(error) => {
                log::debug!("skipping diagnostics for {}: {error:#}", path.display());
                continue;
            }
        };
        let diagnostics = buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            snapshot
                .diagnostics_in_range::<_, PointUtf16>(0..snapshot.len(), false)
                .map(|entry| {
                    json!({
                        "message": entry.diagnostic.message.to_string(),
                        "severity": severity_name(entry.diagnostic.severity),
                        "range": {
                            "start": { "line": entry.range.start.row, "character": entry.range.start.column },
                            "end": { "line": entry.range.end.row, "character": entry.range.end.column },
                        },
                        "source": entry.diagnostic.source,
                    })
                })
                .collect::<Vec<_>>()
        });
        files.push(json!({
            "uri": file_url(&path.to_string_lossy()),
            "diagnostics": diagnostics,
        }));
    }
    Ok(vec![Value::Array(files).to_string()])
}

fn severity_name(severity: DiagnosticSeverity) -> &'static str {
    if severity == DiagnosticSeverity::ERROR {
        "Error"
    } else if severity == DiagnosticSeverity::WARNING {
        "Warning"
    } else if severity == DiagnosticSeverity::INFORMATION {
        "Information"
    } else {
        "Hint"
    }
}

fn path_from_uri(uri: &str) -> PathBuf {
    url::Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .unwrap_or_else(|| PathBuf::from(uri))
}

fn open_buffer_for_path(project: &Project, path: &Path, cx: &gpui::App) -> Option<Entity<Buffer>> {
    let project_path = project.find_project_path(path, cx)?;
    project.get_open_buffer(&project_path, cx)
}

fn not_open(file_path: &str) -> Vec<String> {
    vec![
        json!({
            "success": false,
            "message": format!("Document not open: {file_path}"),
        })
        .to_string(),
    ]
}

fn check_document_dirty(
    server: &WeakEntity<IdeServer>,
    arguments: &Value,
    cx: &mut AsyncApp,
) -> Result<Vec<String>> {
    let file_path = required_str(arguments, "filePath")?;
    let (_, project, _) = server_handles(server, cx)?;
    let state = project.read_with(cx, |project, cx| {
        open_buffer_for_path(project, Path::new(&file_path), cx).map(|buffer| {
            let buffer = buffer.read(cx);
            (buffer.is_dirty(), buffer.file().is_none())
        })
    });
    Ok(match state {
        Some((is_dirty, is_untitled)) => vec![
            json!({
                "success": true,
                "filePath": file_path,
                "isDirty": is_dirty,
                "isUntitled": is_untitled,
            })
            .to_string(),
        ],
        None => not_open(&file_path),
    })
}

async fn save_document(
    server: &WeakEntity<IdeServer>,
    arguments: &Value,
    cx: &mut AsyncApp,
) -> Result<Vec<String>> {
    let file_path = required_str(arguments, "filePath")?;
    let (_, project, _) = server_handles(server, cx)?;
    let Some(buffer) = project.read_with(cx, |project, cx| {
        open_buffer_for_path(project, Path::new(&file_path), cx)
    }) else {
        return Ok(not_open(&file_path));
    };
    project
        .update(cx, |project, cx| project.save_buffer(buffer, cx))
        .await?;
    Ok(vec![
        json!({
            "success": true,
            "filePath": file_path,
            "saved": true,
            "message": "Document saved successfully",
        })
        .to_string(),
    ])
}

fn close_tab(
    server: &WeakEntity<IdeServer>,
    arguments: &Value,
    cx: &mut AsyncApp,
) -> Result<Vec<String>> {
    let tab_name = required_str(arguments, "tab_name")?;
    server.update(cx, |server, cx| {
        server.close_diff(&tab_name, cx);
    })?;
    Ok(vec!["TAB_CLOSED".to_string()])
}

fn close_all_diff_tabs(server: &WeakEntity<IdeServer>, cx: &mut AsyncApp) -> Result<Vec<String>> {
    let count = server.update(cx, |server, cx| server.close_all_diffs(cx))?;
    Ok(vec![format!("CLOSED_{count}_DIFF_TABS")])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_text_range(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("fn main() {\n    let x = 1;\n}\n", cx));
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());

        assert_eq!(
            text_range(&snapshot, Some("let"), Some("1"), false),
            Some(Point::new(1, 4)..Point::new(1, 13))
        );
        assert_eq!(
            text_range(&snapshot, Some("let"), None, true),
            Some(Point::new(1, 4)..Point::new(1, 14))
        );
        assert_eq!(text_range(&snapshot, Some("missing"), None, false), None);
        assert_eq!(text_range(&snapshot, None, None, false), None);
    }

    #[test]
    fn test_tool_definitions_cover_every_tool() {
        let names = definitions()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "openFile",
                "openDiff",
                "getCurrentSelection",
                "getLatestSelection",
                "getOpenEditors",
                "getWorkspaceFolders",
                "getDiagnostics",
                "checkDocumentDirty",
                "saveDocument",
                "close_tab",
                "closeAllDiffTabs",
                "executeCode",
            ]
        );
    }

    #[test]
    fn test_path_from_uri() {
        assert_eq!(
            path_from_uri("file:///home/user/a.rs"),
            PathBuf::from("/home/user/a.rs")
        );
        assert_eq!(
            path_from_uri("/home/user/a.rs"),
            PathBuf::from("/home/user/a.rs")
        );
    }
}
