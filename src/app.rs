//! Main application module

use gpui::*;
use gpui_component::*;

use crate::ui::{
    accounts::AccountsView,
    dashboard::DashboardView,
    settings::SettingsView,
};

/// Main application view
pub struct CloudBridgeApp {
    /// Current navigation item
    current_view: CurrentView,
    /// Dashboard view
    dashboard_view: Entity<DashboardView>,
    /// Accounts view
    accounts_view: Entity<AccountsView>,
    /// Settings view
    settings_view: Entity<SettingsView>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum CurrentView {
    #[default]
    Dashboard,
    Accounts,
    Settings,
}

impl CloudBridgeApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dashboard_view = cx.new(|cx| DashboardView::new(window, cx));
        let accounts_view = cx.new(|cx| AccountsView::new(window, cx));
        let settings_view = cx.new(|cx| SettingsView::new(window, cx));

        Self {
            current_view: CurrentView::Dashboard,
            dashboard_view,
            accounts_view,
            settings_view,
        }
    }

    fn render_sidebar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current_view;

        div()
            .w(px(220.0))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .p_4()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(cx.theme().foreground)
                    .child("CloudBridge")
                    .pb_4()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .mb_4(),
            )
            .child(self.nav_item("Dashboard", CurrentView::Dashboard, current == CurrentView::Dashboard, cx))
            .child(self.nav_item("Accounts", CurrentView::Accounts, current == CurrentView::Accounts, cx))
            .child(
                div().flex_1(), // Flexible space
            )
            .child(self.nav_item("Settings", CurrentView::Settings, current == CurrentView::Settings, cx))
    }

    fn nav_item(
        &self,
        label: &'static str,
        view: CurrentView,
        is_active: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let bg = if is_active {
            cx.theme().accent.opacity(0.1)
        } else {
            transparent_black()
        };

        let text_color = if is_active {
            cx.theme().accent
        } else {
            cx.theme().foreground
        };

        div()
            .id(SharedString::from(label))
            .px_3()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .bg(bg)
            .hover(|s| s.bg(cx.theme().accent.opacity(0.05)))
            .text_color(text_color)
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.current_view = view;
                cx.notify();
            }))
    }

    fn render_content(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match self.current_view {
            CurrentView::Dashboard => div().size_full().child(self.dashboard_view.clone()),
            CurrentView::Accounts => div().size_full().child(self.accounts_view.clone()),
            CurrentView::Settings => div().size_full().child(self.settings_view.clone()),
        }
    }
}

impl Render for CloudBridgeApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().background)
            .h_flex()
            .child(self.render_sidebar(window, cx))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .overflow_hidden()
                    .child(self.render_content(window, cx)),
            )
    }
}
