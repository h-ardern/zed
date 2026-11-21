use std::sync::Arc;

use gpui::{IntoElement, ParentElement};
use language_model::LanguageModelRegistry;
use ui::{Tooltip, prelude::*};

use crate::{AgentPanelOnboardingCard, ApiKeysWithoutProviders};

pub struct AgentPanelOnboarding {
    dismiss: Arc<dyn Fn(&mut Window, &mut App)>,
}

impl AgentPanelOnboarding {
    pub fn new(dismiss: impl Fn(&mut Window, &mut App) + 'static, cx: &mut Context<Self>) -> Self {
        cx.subscribe(
            &LanguageModelRegistry::global(cx),
            |_this: &mut Self, _registry, event: &language_model::Event, cx| match event {
                language_model::Event::ProviderStateChanged(_)
                | language_model::Event::AddedProvider(_)
                | language_model::Event::RemovedProvider(_)
                | language_model::Event::ProvidersChanged => cx.notify(),
                _ => {}
            },
        )
        .detach();

        Self {
            dismiss: Arc::new(dismiss),
        }
    }

    pub fn has_configured_providers(cx: &App) -> bool {
        LanguageModelRegistry::read_global(cx)
            .visible_providers()
            .iter()
            .any(|provider| provider.is_authenticated(cx))
    }
}

impl Render for AgentPanelOnboarding {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let dismiss = self.dismiss.clone();

        AgentPanelOnboardingCard::new().child(
            v_flex()
                .relative()
                .min_w_0()
                .gap_1()
                .child(Headline::new("Welcome to the Agent Panel"))
                .child(
                    Label::new("Connect a language model provider to start your first thread.")
                        .color(Color::Muted),
                )
                .child(ApiKeysWithoutProviders::new())
                .child(
                    h_flex().absolute().top_0().right_0().child(
                        IconButton::new("dismiss_onboarding", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Dismiss"))
                            .on_click(move |_, window, cx| dismiss(window, cx)),
                    ),
                ),
        )
    }
}
