use std::sync::Arc;

use gpui::{
    Action, Context, Entity, IntoElement, ParentElement, Render, Subscription, TaskExt as _,
    WeakEntity, Window, px,
};
use ui::{IconButton, IconSize, Tooltip, prelude::*};

use crate::dock::{Dock, PanelHandle};
use crate::{CloseActiveItem, DeploySearch, SaveIntent, Workspace};

/// Left-dock panels, in the order VS Code shows them on the activity bar.
const PANEL_ORDER: &[&str] = &["ProjectPanel", "GitPanel", "DebugPanel", "OutlinePanel"];

const PROJECT_SEARCH_TELEMETRY: &str = "Project Search Opened";

pub struct ActivityBar {
    workspace: WeakEntity<Workspace>,
    dock: Entity<Dock>,
    _subscriptions: Vec<Subscription>,
}

impl ActivityBar {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        dock: Entity<Dock>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![cx.observe(&dock, |_, _, cx| cx.notify())];
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(cx.observe(&workspace, |_, _, cx| cx.notify()));
        }
        Self {
            workspace,
            dock,
            _subscriptions: subscriptions,
        }
    }
}

enum ActivitySlot {
    Panel {
        index: usize,
        panel: Arc<dyn PanelHandle>,
    },
    Search,
}

impl Render for ActivityBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dock = self.dock.read(cx);
        let active_index = dock.active_panel_index();
        let is_open = dock.is_open();
        let focus_handle = dock.panel_button_focus_handle(window, cx);

        let mut ordered = Vec::new();
        let mut leftovers = Vec::new();
        for (index, panel) in dock.panels() {
            if PANEL_ORDER.contains(&panel.panel_key()) {
                ordered.push((index, panel.clone()));
            } else {
                leftovers.push((index, panel.clone()));
            }
        }

        let search_active = self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .active_item(cx)
                .is_some_and(|item| item.telemetry_event_text(cx) == Some(PROJECT_SEARCH_TELEMETRY))
        });

        let mut slots = Vec::new();
        let mut inserted_search = false;
        for key in PANEL_ORDER {
            if let Some(position) = ordered
                .iter()
                .position(|(_, panel)| panel.panel_key() == *key)
            {
                let (index, panel) = ordered.remove(position);
                slots.push(ActivitySlot::Panel { index, panel });
            }
            if *key == "ProjectPanel" {
                slots.push(ActivitySlot::Search);
                inserted_search = true;
            }
        }
        if !inserted_search {
            slots.insert(0, ActivitySlot::Search);
        }
        for (index, panel) in leftovers {
            slots.push(ActivitySlot::Panel { index, panel });
        }

        let mut buttons = Vec::new();
        for slot in slots {
            match slot {
                ActivitySlot::Search => {
                    let action: Box<dyn Action> = if search_active {
                        Box::new(CloseActiveItem {
                            save_intent: None,
                            close_pinned: true,
                        })
                    } else {
                        Box::new(DeploySearch::default())
                    };
                    let tooltip_action = action.boxed_clone();
                    let click_action = action.boxed_clone();
                    let focus_handle = focus_handle.clone();
                    let workspace = self.workspace.clone();
                    buttons.push(
                        IconButton::new("activity-search", ui::IconName::MagnifyingGlass)
                            .icon_size(IconSize::Medium)
                            .toggle_state(search_active)
                            .tab_index(0isize)
                            .aria_label("Search")
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action("Search", &*tooltip_action, cx)
                            })
                            .on_click({
                                move |_, window, cx| {
                                    // Dispatching `CloseActiveItem` from the dock's focus never
                                    // reaches the pane, so close the search tab directly.
                                    if search_active {
                                        workspace
                                            .update(cx, |workspace, cx| {
                                                let pane = workspace.active_pane().clone();
                                                pane.update(cx, |pane, cx| {
                                                    if let Some(item) = pane.active_item() {
                                                        pane.close_item_by_id(
                                                            item.item_id(),
                                                            SaveIntent::Close,
                                                            window,
                                                            cx,
                                                        )
                                                        .detach_and_log_err(cx);
                                                    }
                                                });
                                            })
                                            .ok();
                                    } else {
                                        window.focus(&focus_handle, cx);
                                        window.dispatch_action(click_action.boxed_clone(), cx);
                                    }
                                }
                            })
                            .into_any_element(),
                    );
                }
                ActivitySlot::Panel { index, panel } => {
                    let Some(icon) = panel.icon(window, cx) else {
                        continue;
                    };
                    let Some(icon_tooltip) = panel.icon_tooltip(window, cx) else {
                        continue;
                    };
                    let is_active = Some(index) == active_index && is_open;
                    let action = if is_active {
                        dock.toggle_action()
                    } else {
                        panel.toggle_action(window, cx)
                    };
                    let tooltip_action = action.boxed_clone();
                    let click_action = action.boxed_clone();
                    let focus_handle = focus_handle.clone();
                    let tooltip_text: SharedString = if is_active {
                        "Hide Sidebar".into()
                    } else {
                        icon_tooltip.into()
                    };
                    buttons.push(
                        IconButton::new(("activity", index), icon)
                            .icon_size(IconSize::Medium)
                            .toggle_state(is_active)
                            .tab_index(0isize)
                            .aria_label(icon_tooltip)
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action(tooltip_text.clone(), &*tooltip_action, cx)
                            })
                            .on_click({
                                move |_, window, cx| {
                                    window.focus(&focus_handle, cx);
                                    window.dispatch_action(click_action.boxed_clone(), cx);
                                }
                            })
                            .into_any_element(),
                    );
                }
            }
        }

        v_flex()
            .id("activity-bar")
            .h_full()
            .w(px(48.))
            .flex_none()
            .items_center()
            .gap_1()
            .py_1()
            .bg(cx.theme().colors().panel_background)
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .children(buttons)
    }
}
