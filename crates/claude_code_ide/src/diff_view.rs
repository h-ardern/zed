use crate::{AcceptDiff, RejectDiff, server::close_item_in_window};
use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use editor::{
    Editor, EditorEvent, EditorSettings, HiddenUnstagedDiffHunkRenderer, MultiBuffer,
    SplittableEditor,
};
use futures::channel::oneshot;
use gpui::{
    AnyEntity, AnyWindowHandle, App, AppContext as _, AsyncApp, AsyncWindowContext, Context,
    Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, Task, WeakEntity, Window,
};
use language::{Buffer, Capability, Point};
use project::Project;
use settings::Settings as _;
use std::{
    any::TypeId,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use ui::{KeyBinding, prelude::*};
use workspace::{Item, Workspace, item::ItemEvent};

const RECALCULATE_DIFF_DEBOUNCE: Duration = Duration::from_millis(250);

pub enum DiffOutcome {
    /// The user accepted, and the file now holds `final_contents`, including any edits
    /// they made to the proposal before accepting.
    Accepted {
        final_contents: String,
    },
    Rejected,
}

/// A tab showing a change Claude Code proposed, which the user accepts or rejects.
/// The proposal is editable, so the user can adjust it before accepting.
pub struct ClaudeDiffView {
    diff_editor: Entity<SplittableEditor>,
    proposed_buffer: Entity<Buffer>,
    target_buffer: Option<Entity<Buffer>>,
    target_path: PathBuf,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    title: SharedString,
    path_label: SharedString,
    /// Dropping this without sending tells the waiting `openDiff` call it was rejected.
    outcome_tx: Option<oneshot::Sender<DiffOutcome>>,
    buffer_changes_tx: watch::Sender<()>,
    _recalculate_diff_task: Task<()>,
}

pub struct DiffRequest {
    pub target_path: PathBuf,
    pub target_buffer: Option<Entity<Buffer>>,
    pub base_text: String,
    pub proposed_text: String,
    pub tab_name: String,
    pub outcome_tx: oneshot::Sender<DiffOutcome>,
}

impl ClaudeDiffView {
    /// Creates the view and adds it to the workspace's active pane.
    pub async fn open(
        request: DiffRequest,
        workspace: WeakEntity<Workspace>,
        window: AnyWindowHandle,
        cx: &mut AsyncApp,
    ) -> Result<Entity<Self>> {
        let workspace = workspace.upgrade().context("the workspace was closed")?;
        let (project, base_buffer, proposed_buffer, diff) = cx.update(|cx| {
            let project = workspace.read(cx).project().clone();
            let language = request
                .target_buffer
                .as_ref()
                .and_then(|buffer| buffer.read(cx).language().cloned());
            let language_registry = project.read(cx).languages().clone();

            let base_buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(request.base_text, cx);
                buffer.set_language_registry(language_registry.clone());
                buffer.set_language(language.clone(), cx);
                buffer.set_capability(Capability::ReadOnly, cx);
                buffer
            });
            let proposed_buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(request.proposed_text, cx);
                buffer.set_language_registry(language_registry);
                buffer.set_language(language, cx);
                buffer
            });
            let diff = cx.new(|cx| {
                let mut diff = BufferDiff::new_with_base_text_buffer(
                    &proposed_buffer.read(cx).snapshot().text,
                    base_buffer.clone(),
                    cx,
                );
                diff.set_operations(Arc::new(buffer_diff::RestoreDiffOperations));
                diff
            });
            (project, base_buffer, proposed_buffer, diff)
        });

        // The split editor lays out both sides from the diff it's given, so the diff has
        // to be computed before the editor is built, or the original side shows up empty.
        update_diff(&diff, &proposed_buffer, &base_buffer, cx).await;

        let path_label: SharedString = request
            .target_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| request.target_path.to_string_lossy().into_owned())
            .into();
        cx.update_window(window, |_, window, cx| {
            let view = cx.new(|cx| {
                Self::new(
                    workspace.clone(),
                    project,
                    diff,
                    base_buffer,
                    proposed_buffer,
                    request.target_buffer,
                    request.target_path,
                    request.tab_name.into(),
                    path_label,
                    request.outcome_tx,
                    window,
                    cx,
                )
            });
            workspace.update(cx, |workspace, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
                });
            });
            view
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        diff: Entity<BufferDiff>,
        base_buffer: Entity<Buffer>,
        proposed_buffer: Entity<Buffer>,
        target_buffer: Option<Entity<Buffer>>,
        target_path: PathBuf,
        title: SharedString,
        path_label: SharedString,
        outcome_tx: oneshot::Sender<DiffOutcome>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            let max_point = proposed_buffer.read(cx).max_point();
            multibuffer.set_excerpts_for_buffer(
                proposed_buffer.clone(),
                [Point::new(0, 0)..max_point],
                0,
                cx,
            );
            multibuffer.add_diff(diff.clone(), cx);
            multibuffer
        });
        let diff_editor = cx.new(|cx| {
            let splittable = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project.clone(),
                workspace.clone(),
                window,
                cx,
            );
            splittable.set_diff_hunk_renderer(Some(Arc::new(HiddenUnstagedDiffHunkRenderer)), cx);
            splittable
        });

        let (buffer_changes_tx, mut buffer_changes_rx) = watch::channel(());
        cx.subscribe(
            &proposed_buffer,
            |this, _, event: &language::BufferEvent, _| {
                if let language::BufferEvent::Edited { .. } = event {
                    this.buffer_changes_tx.send(()).ok();
                }
            },
        )
        .detach();

        let recalculate_diff_task = cx.spawn({
            let proposed_buffer = proposed_buffer.clone();
            async move |_, cx| {
                while buffer_changes_rx.recv().await.is_ok() {
                    cx.background_executor()
                        .timer(RECALCULATE_DIFF_DEBOUNCE)
                        .await;
                    update_diff(&diff, &proposed_buffer, &base_buffer, cx).await;
                }
            }
        });

        Self {
            diff_editor,
            proposed_buffer,
            target_buffer,
            target_path,
            project,
            workspace: workspace.downgrade(),
            title,
            path_label,
            outcome_tx: Some(outcome_tx),
            buffer_changes_tx,
            _recalculate_diff_task: recalculate_diff_task,
        }
    }

    fn accept(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(outcome_tx) = self.outcome_tx.take() else {
            return;
        };
        let contents = self.proposed_buffer.read(cx).text();
        let target_buffer = self.target_buffer.clone();
        let target_path = self.target_path.clone();
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let outcome =
                match write_contents(&project, target_buffer, &target_path, &contents, cx).await {
                    Ok(()) => DiffOutcome::Accepted {
                        final_contents: contents,
                    },
                    Err(error) => {
                        log::error!(
                            "failed to apply Claude Code's change to {}: {error:#}",
                            target_path.display()
                        );
                        DiffOutcome::Rejected
                    }
                };
            outcome_tx.send(outcome).ok();
            this.update_in(cx, |this, window, cx| this.close(window, cx))
                .ok();
        })
        .detach();
    }

    fn reject(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(outcome_tx) = self.outcome_tx.take() {
            outcome_tx.send(DiffOutcome::Rejected).ok();
        }
        self.close(window, cx);
    }

    fn close(&self, window: &mut Window, cx: &mut Context<Self>) {
        close_item_in_window(&self.workspace, cx.entity_id(), window, cx);
    }
}

async fn write_contents(
    project: &Entity<Project>,
    target_buffer: Option<Entity<Buffer>>,
    target_path: &Path,
    contents: &str,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    match target_buffer {
        Some(buffer) => {
            buffer.update(cx, |buffer, cx| {
                buffer.set_text(contents, cx);
            });
            project
                .update(cx, |project, cx| project.save_buffer(buffer, cx))
                .await
        }
        None => {
            if let Some(parent) = target_path.parent() {
                smol::fs::create_dir_all(parent).await?;
            }
            smol::fs::write(target_path, contents).await?;
            Ok(())
        }
    }
}

async fn update_diff(
    diff: &Entity<BufferDiff>,
    buffer: &Entity<Buffer>,
    base_buffer: &Entity<Buffer>,
    cx: &mut AsyncApp,
) {
    let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
    let base_snapshot = base_buffer.read_with(cx, |buffer, _| buffer.snapshot());
    let base_text = Arc::<str>::from(base_snapshot.text());
    let update = diff
        .update(cx, |diff, cx| {
            diff.update_diff(
                buffer_snapshot.text.clone(),
                &base_snapshot,
                Some(base_text),
                cx,
            )
        })
        .await;
    diff.update(cx, |diff, cx| diff.set_snapshot(update, cx));
}

impl EventEmitter<EditorEvent> for ClaudeDiffView {}

impl Focusable for ClaudeDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff_editor.focus_handle(cx)
    }
}

impl Item for ClaudeDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.target_path.to_string_lossy().into_owned().into())
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff_editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.diff_editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.diff_editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff_editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for ClaudeDiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);
        v_flex()
            .size_full()
            .key_context("ClaudeDiffView")
            .on_action(cx.listener(|this, _: &AcceptDiff, window, cx| this.accept(window, cx)))
            .on_action(cx.listener(|this, _: &RejectDiff, window, cx| this.reject(window, cx)))
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(format!(
                            "Claude Code proposes changes to {}",
                            self.path_label
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("claude-code-reject-diff", "Reject")
                                    .key_binding(KeyBinding::for_action_in(
                                        &RejectDiff,
                                        &focus_handle,
                                        cx,
                                    ))
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.reject(window, cx)),
                                    ),
                            )
                            .child(
                                Button::new("claude-code-accept-diff", "Accept")
                                    .style(ButtonStyle::Filled)
                                    .key_binding(KeyBinding::for_action_in(
                                        &AcceptDiff,
                                        &focus_handle,
                                        cx,
                                    ))
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.accept(window, cx)),
                                    ),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.diff_editor.clone()))
    }
}
