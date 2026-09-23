//! Main application module

use std::collections::HashSet;

use chrono::Utc;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::ui::data::SyncStatus;
use crate::ui::{
    account_detail::AccountDetailView, accounts::AccountsView, alerts::AlertsView,
    attribution::AttributionView, overview::OverviewView, query::QueryView, rules::RulesView,
    settings::SettingsView,
};
use crate::ui::{fmt, theme};

actions!(
    cloudbridge,
    [
        SwitchToOverview,
        SwitchToAlerts,
        SwitchToAttribution,
        SwitchToQuery,
        SwitchToAccounts,
        SwitchToRules,
        SwitchToSettings,
        SwitchToAccountDetail,
        ReloadCurrentView,
    ]
);

/// State shared across pages.
///
/// Pages can subscribe to the entity; the app shell refreshes it on
/// creation and on every view switch.
pub struct AppState {
    /// Open alerts, shown as the sidebar badge. 0 hides the badge.
    pub open_alerts: usize,
    /// The status bar's sync data; `None` until the first load lands.
    pub sync: Option<SyncStatus>,
    /// A view switch a page asked for; the shell observes this entity,
    /// applies the request, and clears it. The optional string is the
    /// target's payload — the account id for the Account detail page.
    navigate_to: Option<(CurrentView, Option<String>)>,
    /// A page changed data the current view shows (demo data load/clear);
    /// the shell reloads the current page and status bar when set.
    reload_requested: bool,
    /// The stores finished opening (desktop init, see desktop.rs); the
    /// shell starts loading pages when this flips.
    stores_opened: bool,
}

impl AppState {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            open_alerts: 0,
            sync: None,
            navigate_to: None,
            reload_requested: false,
            stores_opened: false,
        }
    }

    /// Ask the app shell to switch to `view`. The shell observes this
    /// entity and applies the request on notify.
    pub fn navigate(&mut self, view: CurrentView, cx: &mut Context<Self>) {
        self.navigate_to = Some((view, None));
        cx.notify();
    }

    /// Ask the app shell to open the Account detail page for an account.
    pub fn navigate_to_account(&mut self, account_id: String, cx: &mut Context<Self>) {
        self.navigate_to = Some((CurrentView::AccountDetail, Some(account_id)));
        cx.notify();
    }

    /// Ask the app shell to reload the current page and the status bar,
    /// after a change they read has landed behind their backs.
    pub fn request_reload(&mut self, cx: &mut Context<Self>) {
        self.reload_requested = true;
        cx.notify();
    }

    /// Tell the app shell the stores are open, so it can start loading.
    pub fn mark_stores_opened(&mut self, cx: &mut Context<Self>) {
        self.stores_opened = true;
        cx.notify();
    }
}

/// Global handle to the shared [`AppState`] entity, set when the app shell
/// is created. Pages reach it from any event listener via
/// `cx.global::<GlobalAppState>()` — see [`navigate_to`].
pub struct GlobalAppState(pub Entity<AppState>);

impl Global for GlobalAppState {}

/// Ask the app shell to switch views, from any click handler:
///
/// ```rust,ignore
/// .on_click(|_, _, cx| crate::app::navigate_to(CurrentView::Rules, cx))
/// ```
pub fn navigate_to(view: CurrentView, cx: &mut App) {
    let app_state = cx.global::<GlobalAppState>().0.clone();
    app_state.update(cx, |state, cx| state.navigate(view, cx));
}

/// Ask the app shell to reload the current page and the status bar, e.g.
/// after loading or clearing demo data from Settings.
pub fn request_reload(cx: &mut App) {
    let app_state = cx.global::<GlobalAppState>().0.clone();
    app_state.update(cx, |state, cx| state.request_reload(cx));
}

/// Tell the app shell the desktop's stores finished opening; it then loads
/// the page on screen and the status bar. Until this lands no page loads,
/// since every read would fail with "not initialized".
#[cfg(not(target_family = "wasm"))]
pub fn stores_opened(cx: &mut App) {
    let app_state = cx.global::<GlobalAppState>().0.clone();
    app_state.update(cx, |state, cx| state.mark_stores_opened(cx));
}

/// Ask the app shell to open the Account detail page for an account.
pub fn navigate_to_account(account_id: String, cx: &mut App) {
    let app_state = cx.global::<GlobalAppState>().0.clone();
    app_state.update(cx, |state, cx| state.navigate_to_account(account_id, cx));
}

/// Main application view
pub struct CloudBridgeApp {
    /// Current navigation item
    current_view: CurrentView,
    /// Shared app state (alert badge count, ...)
    app_state: Entity<AppState>,
    /// Overview view
    overview_view: Entity<OverviewView>,
    /// Alerts view
    alerts_view: Entity<AlertsView>,
    /// Attribution view
    attribution_view: Entity<AttributionView>,
    /// Query view
    query_view: Entity<QueryView>,
    /// Accounts view
    accounts_view: Entity<AccountsView>,
    /// Account detail view (opened from an Accounts row)
    account_detail_view: Entity<AccountDetailView>,
    /// Rules view
    rules_view: Entity<RulesView>,
    /// Settings view
    settings_view: Entity<SettingsView>,
    /// Pages whose deferred first load has been kicked off; a first visit
    /// loads, later visits reload.
    activated_views: HashSet<CurrentView>,
    /// Focus target for the shell's root element. Keystrokes dispatch from
    /// the focused element up; with nothing focused inside the shell they
    /// would stop at the window root and never reach the shortcut
    /// listeners in `render`.
    focus_handle: FocusHandle,
    /// The stores are open and pages may load. The browser seeds its
    /// ledger before the window opens, so it starts `true` there; the
    /// desktop opens the window first and flips it when init finishes.
    stores_ready: bool,
    /// Keeps the AppState observer and the focus-lost fallback alive.
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CurrentView {
    #[default]
    Overview,
    Alerts,
    Attribution,
    Query,
    Accounts,
    /// Per-account drill-down; not a sidebar entry — the Accounts nav item
    /// stays highlighted while it shows.
    AccountDetail,
    Rules,
    Settings,
}

impl CloudBridgeApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app_state = cx.new(|cx| AppState::new(window, cx));
        // Pages reach the shared state through the global rather than being
        // handed the entity one constructor at a time.
        cx.set_global(GlobalAppState(app_state.clone()));
        let overview_view = cx.new(|cx| OverviewView::new(window, cx));
        let alerts_view = cx.new(|cx| AlertsView::new(window, cx));
        let attribution_view = cx.new(|cx| AttributionView::new(window, cx));
        let query_view = cx.new(|cx| QueryView::new(window, cx));
        let accounts_view = cx.new(|cx| AccountsView::new(window, cx));
        let account_detail_view = cx.new(|cx| AccountDetailView::new(window, cx));
        let rules_view = cx.new(|cx| RulesView::new(window, cx));
        let settings_view = cx.new(|cx| SettingsView::new(window, cx));

        // Shell-wide shortcuts: ⌘1…⌘8 switch pages in sidebar order
        // (⌘8 opens the Account drill-down with the account it last
        // showed), ⌘R reloads the page on screen. `secondary` is ⌘ on macOS
        // and Ctrl elsewhere — `cmd` would be the Windows key there, whose
        // digit and R chords the OS keeps for itself. No key context, so
        // they fire regardless of which page or input has focus; the action
        // listeners sit on the shell's root element in `render`.
        cx.bind_keys([
            KeyBinding::new("secondary-1", SwitchToOverview, None),
            KeyBinding::new("secondary-2", SwitchToAlerts, None),
            KeyBinding::new("secondary-3", SwitchToAttribution, None),
            KeyBinding::new("secondary-4", SwitchToQuery, None),
            KeyBinding::new("secondary-5", SwitchToAccounts, None),
            KeyBinding::new("secondary-6", SwitchToRules, None),
            KeyBinding::new("secondary-7", SwitchToSettings, None),
            KeyBinding::new("secondary-8", SwitchToAccountDetail, None),
            KeyBinding::new("secondary-r", ReloadCurrentView, None),
        ]);

        // Start with focus on the shell, and take it back whenever the
        // focused element leaves the tree — a page's dialog closing leaves
        // focus on a handle nothing renders, which would strand the
        // shortcuts at the window root.
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let focus_lost = cx.on_focus_lost(window, |this, window, cx| {
            this.focus_handle.focus(window, cx);
        });

        // A page's navigation request lands on AppState::navigate_to; apply
        // it and clear it. The update in refresh_status_bar does not notify,
        // so this observer cannot loop.
        let observer = cx.observe(&app_state, |this, app_state, cx| {
            let (target, reload, opened) = app_state.update(cx, |state, _| {
                (
                    state.navigate_to.take(),
                    std::mem::take(&mut state.reload_requested),
                    std::mem::take(&mut state.stores_opened),
                )
            });
            if opened {
                this.stores_ready = true;
            }
            if let Some((view, payload)) = target {
                // The detail page's payload names the account before the
                // switch, so the page never renders another account's data.
                if let (CurrentView::AccountDetail, Some(account_id)) = (view, payload) {
                    this.account_detail_view
                        .update(cx, |v, cx| v.show(account_id, cx));
                }
                if this.current_view != view {
                    this.current_view = view;
                    this.reload_view(view, cx);
                    this.refresh_status_bar(cx);
                }
                cx.notify();
            }
            if reload || opened {
                this.reload_current(cx);
            }
        });

        let mut this = Self {
            current_view: CurrentView::Overview,
            app_state,
            overview_view,
            alerts_view,
            attribution_view,
            query_view,
            accounts_view,
            account_detail_view,
            rules_view,
            settings_view,
            activated_views: HashSet::new(),
            focus_handle,
            stores_ready: cfg!(target_family = "wasm"),
            _subscriptions: vec![observer, focus_lost],
        };

        // The browser's stores are ready now, so the page on screen and
        // the status bar load right away. On the desktop both calls no-op
        // until `stores_opened` lands (see desktop.rs).
        this.reload_view(this.current_view, cx);
        this.refresh_status_bar(cx);

        this
    }

    /// Reload the page data of `view`, called whenever the shell switches
    /// to it. Views are created once and kept alive, so without this a
    /// revisited page would show what it loaded at startup.
    ///
    /// A page's first load is deferred until its first visit, so the pages
    /// that are not on screen at startup do not race the visible one for
    /// the ledger. `ensure_loaded` and `reload` both no-op while a load is
    /// in flight, so a revisit during the first load cannot double it.
    fn reload_view(&mut self, view: CurrentView, cx: &mut Context<Self>) {
        // Before the stores open every read fails; the page stays unvisited
        // so its first load runs once they are.
        if !self.stores_ready {
            return;
        }
        let first_visit = self.activated_views.insert(view);
        match view {
            CurrentView::Overview => self.overview_view.update(cx, |v, cx| {
                if first_visit {
                    v.ensure_loaded(cx);
                } else {
                    v.reload(cx);
                }
            }),
            CurrentView::Alerts => self.alerts_view.update(cx, |v, cx| {
                if first_visit {
                    v.ensure_loaded(cx);
                } else {
                    v.reload(cx);
                }
            }),
            CurrentView::Attribution => self.attribution_view.update(cx, |v, cx| {
                if first_visit {
                    v.ensure_loaded(cx);
                } else {
                    v.reload(cx);
                }
            }),
            CurrentView::Query => self.query_view.update(cx, |v, cx| v.reload(cx)),
            CurrentView::Accounts => self.accounts_view.update(cx, |v, cx| {
                if first_visit {
                    v.ensure_loaded(cx);
                } else {
                    v.reload(cx);
                }
            }),
            // show() already started a fresh load; reload() no-ops while
            // it is in flight.
            CurrentView::AccountDetail => self.account_detail_view.update(cx, |v, cx| v.reload(cx)),
            CurrentView::Rules => self.rules_view.update(cx, |v, cx| {
                if first_visit {
                    v.ensure_loaded(cx);
                } else {
                    v.reload(cx);
                }
            }),
            CurrentView::Settings => {}
        }
    }

    /// Reload the page on screen and the status bar — the ⌘R handler and
    /// the AppState `reload_requested` path share this.
    fn reload_current(&mut self, cx: &mut Context<Self>) {
        self.reload_view(self.current_view, cx);
        self.refresh_status_bar(cx);
        cx.notify();
    }

    /// Switch to `view` through AppState, the same path the sidebar's
    /// click handlers take; the observer applies the switch.
    fn switch_to(&mut self, view: CurrentView, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.navigate(view, cx));
    }

    /// Reload the status bar's sync status and open-alert count.
    ///
    /// Both loaders are blocking DuckDB reads, so they run in
    /// `smol::unblock` and the result lands back on `AppState` — the same
    /// thread + unblock + spawn + notify pattern as `accounts.rs`.
    fn refresh_status_bar(&mut self, cx: &mut Context<Self>) {
        if !self.stores_ready {
            return;
        }
        let app_state = self.app_state.clone();

        cx.spawn(async move |this, cx| {
            let (sync, open_alerts) = smol::unblock(|| {
                let sync = crate::ui::data::load_sync_status().ok();
                let open_alerts = crate::alerts::open_alerts()
                    .map(|alerts| alerts.len())
                    .unwrap_or(0);
                (sync, open_alerts)
            })
            .await;

            cx.update(|cx| {
                app_state.update(cx, |state, _| {
                    state.sync = sync;
                    state.open_alerts = open_alerts;
                });
                this.update(cx, |_, cx| cx.notify()).ok();
            });
        })
        .detach();
    }

    fn render_sidebar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current_view;
        let open_alerts = self.app_state.read(cx).open_alerts;
        // No open alerts, no badge.
        let alerts_badge = (open_alerts > 0).then_some(open_alerts);

        div()
            // 14rem (182px at the 13px base), rem-based so the sidebar
            // zooms with the base font.
            .w_56()
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme::card_border(cx))
            .bg(theme::sidebar_bg(cx))
            .p_4()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .pb_4()
                    .mb_2()
                    .border_b_1()
                    .border_color(theme::card_border(cx))
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::accent(cx))
                            .child("CloudBridge"),
                    )
                    .child(theme::caption(
                        cx,
                        format!("LOCAL LEDGER · V{}", env!("CARGO_PKG_VERSION")),
                    )),
            )
            .child(self.nav_item(
                "Overview",
                IconName::LayoutDashboard,
                CurrentView::Overview,
                current == CurrentView::Overview,
                None,
                cx,
            ))
            .child(self.nav_item(
                "Alerts",
                IconName::Bell,
                CurrentView::Alerts,
                current == CurrentView::Alerts,
                alerts_badge,
                cx,
            ))
            .child(self.nav_item(
                "Attribution",
                IconName::ChartPie,
                CurrentView::Attribution,
                current == CurrentView::Attribution,
                None,
                cx,
            ))
            // Query, Rules and Settings are the desktop's own: a query
            // console needs an engine underneath it, and neither an
            // allocation rule nor a setting can be acted on in a tab that
            // forgets everything when it reloads. The demo keeps the five
            // pages whose figures it can actually stand behind.
            .when(cfg!(not(target_family = "wasm")), |el| {
                el.child(self.nav_item(
                    "Query",
                    IconName::Search,
                    CurrentView::Query,
                    current == CurrentView::Query,
                    None,
                    cx,
                ))
            })
            .child(self.nav_item(
                "Accounts",
                IconName::Building2,
                CurrentView::Accounts,
                // The Account detail page is a drill-down of Accounts;
                // keep the parent highlighted while it shows.
                matches!(current, CurrentView::Accounts | CurrentView::AccountDetail),
                None,
                cx,
            ))
            .when(cfg!(not(target_family = "wasm")), |el| {
                el.child(self.nav_item(
                    "Rules",
                    IconName::SquareTerminal,
                    CurrentView::Rules,
                    current == CurrentView::Rules,
                    None,
                    cx,
                ))
            })
            .child(div().flex_1())
            .when(cfg!(not(target_family = "wasm")), |el| {
                el.child(self.nav_item(
                    "Settings",
                    IconName::Settings,
                    CurrentView::Settings,
                    current == CurrentView::Settings,
                    None,
                    cx,
                ))
            })
    }

    fn nav_item(
        &self,
        label: &'static str,
        icon: IconName,
        view: CurrentView,
        is_active: bool,
        badge: Option<usize>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let text_color = if is_active {
            theme::on_accent(cx)
        } else {
            theme::text_muted(cx)
        };

        let mut item = div()
            .id(SharedString::from(label))
            .h_flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            // Theme radius, not a pill: dense data-tool chrome.
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .text_color(text_color)
            .when(is_active, |el| {
                el.bg(theme::accent(cx))
                    .hover(|s| s.bg(theme::accent_hover(cx)))
            })
            .when(!is_active, |el| {
                el.hover(|s| s.bg(theme::card_bg(cx)))
                    .active(|s| s.bg(theme::card_border(cx)))
            })
            .child(Icon::new(icon).size_4().text_color(text_color))
            .child(label);

        if let Some(count) = badge {
            item = item.child(div().flex_1()).child(theme::count_badge(
                cx,
                count,
                if is_active {
                    theme::on_accent(cx)
                } else {
                    theme::accent(cx)
                },
                if is_active {
                    theme::accent(cx)
                } else {
                    theme::on_accent(cx)
                },
            ));
        }

        item.on_click(move |_, _, cx| crate::app::navigate_to(view, cx))
    }

    /// The window's bottom status bar: sync state as one muted line, in
    /// the desktop convention, instead of a card competing with the nav.
    fn render_status_bar(&self, cx: &App) -> impl IntoElement {
        let sync = self.app_state.read(cx).sync.as_ref();

        let (text, dot_color) = match sync {
            Some(sync) => {
                let synced = match sync.last_synced_at {
                    Some(at) => format!("Synced {}", fmt::relative_time(at)),
                    None => "Never synced".to_string(),
                };
                // sync_detail carries "N sources · next auto-fetch …".
                // The dot follows the same due-now cutoff as that line.
                let dot_color = match sync.next_fetch_at {
                    Some(at) if at > Utc::now() => theme::olive(cx),
                    Some(_) => theme::warning_text(cx),
                    None => theme::grey(cx),
                };
                (format!("{synced} · {}", sync_detail(sync)), dot_color)
            }
            None => ("Syncing…".to_string(), theme::grey(cx)),
        };

        div()
            .w_full()
            .h_flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_1()
            .border_t_1()
            .border_color(theme::card_border(cx))
            .child(theme::dot(dot_color))
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_muted(cx))
                    .child(text),
            )
    }

    fn render_content(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pages that read the stores have nothing to show until they open;
        // Overview has its own skeleton for this, Settings needs no store.
        if !self.stores_ready
            && !matches!(
                self.current_view,
                CurrentView::Overview | CurrentView::Settings
            )
        {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::app_bg(cx))
                .text_sm()
                .text_color(theme::text_muted(cx))
                .child("Opening the ledger…");
        }
        match self.current_view {
            CurrentView::Overview => div().size_full().child(self.overview_view.clone()),
            CurrentView::Alerts => div().size_full().child(self.alerts_view.clone()),
            CurrentView::Attribution => div().size_full().child(self.attribution_view.clone()),
            CurrentView::Query => div().size_full().child(self.query_view.clone()),
            CurrentView::Accounts => div().size_full().child(self.accounts_view.clone()),
            CurrentView::AccountDetail => div().size_full().child(self.account_detail_view.clone()),
            CurrentView::Rules => div().size_full().child(self.rules_view.clone()),
            CurrentView::Settings => div().size_full().child(self.settings_view.clone()),
        }
    }
}

impl Render for CloudBridgeApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            // The shell's root is an ancestor of every page in the
            // dispatch tree, so these listeners see the shortcuts wherever
            // focus sits inside it; the bindings are registered in `new`.
            .on_action(cx.listener(|this, _: &SwitchToOverview, _, cx| {
                this.switch_to(CurrentView::Overview, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchToAlerts, _, cx| {
                this.switch_to(CurrentView::Alerts, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchToAttribution, _, cx| {
                this.switch_to(CurrentView::Attribution, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchToQuery, _, cx| {
                this.switch_to(CurrentView::Query, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchToAccounts, _, cx| {
                this.switch_to(CurrentView::Accounts, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchToRules, _, cx| {
                this.switch_to(CurrentView::Rules, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchToSettings, _, cx| {
                this.switch_to(CurrentView::Settings, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchToAccountDetail, _, cx| {
                this.switch_to(CurrentView::AccountDetail, cx);
            }))
            .on_action(cx.listener(|this, _: &ReloadCurrentView, _, cx| {
                this.reload_current(cx);
            }))
            .bg(theme::app_bg(cx))
            .text_color(theme::text_primary(cx))
            .h_flex()
            .child(self.render_sidebar(window, cx))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .v_flex()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_content(window, cx)),
                    )
                    .child(self.render_status_bar(cx)),
            )
    }
}

/// The muted line under the sync title: source count plus when the next
/// automatic fetch is due.
fn sync_detail(sync: &SyncStatus) -> String {
    if sync.source_count == 0 {
        return "No sources configured".to_string();
    }

    let sources = format!(
        "{} source{}",
        sync.source_count,
        if sync.source_count == 1 { "" } else { "s" }
    );

    match sync.next_fetch_at {
        None => format!("{} · waiting for the first sync", sources),
        Some(at) => {
            let remaining = at - Utc::now();
            if remaining.num_seconds() <= 0 {
                format!("{} · next auto-fetch due now", sources)
            } else if remaining.num_hours() >= 1 {
                format!(
                    "{} · next auto-fetch in {}h {}m",
                    sources,
                    remaining.num_hours(),
                    remaining.num_minutes() % 60
                )
            } else {
                format!(
                    "{} · next auto-fetch in {} min",
                    sources,
                    remaining.num_minutes().max(1)
                )
            }
        }
    }
}
