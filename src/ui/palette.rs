//! The command palette: ⌘K (Ctrl+K elsewhere) from anywhere in the window.
//!
//! One search over the three things a person jumps to: a page, an account,
//! or a command. Pages and commands carry the shell's actions, so the
//! palette shows each one's shortcut beside it and doubles as the list of
//! keyboard shortcuts; accounts open their detail page.
//!
//! The shell owns the [`CommandState`] and opens the palette in a dialog;
//! the entries are rebuilt from the account list at each open, so an
//! account added or deleted since the last one is already right.

use std::rc::Rc;

use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{IconName, IndexPath, StyledExt as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::app;
use crate::model::CloudAccount;
use crate::ui::theme;

/// What confirming a palette row does beyond its action, if it has one.
#[derive(Clone)]
enum Target {
    /// The row's action already did everything (a page switch, a reload).
    Action,
    /// Open the Accounts page's add dialog.
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    AddAccount,
    /// Open this account's detail page.
    Account(String),
}

/// One page of the sidebar, in its order, with the action that opens it.
struct Page {
    label: &'static str,
    icon: IconName,
    action: fn() -> Box<dyn Action>,
    /// Extra words the search matches, for a page whose name is not the
    /// word someone would type.
    keywords: &'static [&'static str],
}

/// The sidebar's pages, in its order; the desktop-only ones are left out
/// of the browser build just as the sidebar leaves them out.
fn pages() -> Vec<Page> {
    let mut pages = vec![
        Page {
            label: "Overview",
            icon: IconName::LayoutDashboard,
            action: || Box::new(app::SwitchToOverview),
            keywords: &["dashboard", "home", "spend"],
        },
        Page {
            label: "Alerts",
            icon: IconName::Bell,
            action: || Box::new(app::SwitchToAlerts),
            keywords: &["anomaly", "notification"],
        },
        Page {
            label: "Attribution",
            icon: IconName::ChartPie,
            action: || Box::new(app::SwitchToAttribution),
            keywords: &["sankey", "treemap", "business line", "tag"],
        },
        Page {
            label: "Models",
            icon: IconName::Cpu,
            action: || Box::new(app::SwitchToModels),
            keywords: &["tokens", "llm", "ai"],
        },
        Page {
            label: "Insights",
            icon: IconName::Eye,
            action: || Box::new(app::SwitchToInsights),
            keywords: &["idle", "savings", "resources"],
        },
    ];
    #[cfg(not(target_family = "wasm"))]
    pages.push(Page {
        label: "Query",
        icon: IconName::Search,
        action: || Box::new(app::SwitchToQuery),
        keywords: &["sql"],
    });
    pages.push(Page {
        label: "Accounts",
        icon: IconName::Building2,
        action: || Box::new(app::SwitchToAccounts),
        keywords: &["sources", "providers"],
    });
    #[cfg(not(target_family = "wasm"))]
    pages.extend([
        Page {
            label: "Rules",
            icon: IconName::SquareTerminal,
            action: || Box::new(app::SwitchToRules),
            keywords: &["alert rules", "allocation"],
        },
        Page {
            label: "Settings",
            icon: IconName::Settings,
            action: || Box::new(app::SwitchToSettings),
            keywords: &["preferences", "theme", "currency"],
        },
    ]);
    pages
}

/// The palette's groups and, in the same shape, what each row does —
/// `targets[section][row]` is the row at that [`IndexPath`].
fn entries(accounts: &[CloudAccount]) -> (Vec<CommandGroup>, Vec<Vec<Target>>) {
    let pages = pages();
    let page_group = CommandGroup::new()
        .label("Pages")
        .items(pages.iter().map(|page| {
            CommandItem::new()
                .label(page.label)
                .icon(page.icon.clone())
                .keywords(page.keywords.iter().copied())
                .action((page.action)())
        }));
    let page_targets = vec![Target::Action; pages.len()];

    // Add account is the desktop's alone, so the browser never pushes.
    #[cfg_attr(target_family = "wasm", allow(unused_mut))]
    let mut commands = vec![CommandItem::new()
        .label("Reload page")
        .icon(gpui_kit::assets::IconName::RefreshCw)
        .keywords(["refresh"])
        .action(Box::new(app::ReloadCurrentView))];
    #[cfg_attr(target_family = "wasm", allow(unused_mut))]
    let mut command_targets = vec![Target::Action];
    #[cfg(not(target_family = "wasm"))]
    {
        commands.push(
            CommandItem::new()
                .label("Add account…")
                .icon(IconName::Plus)
                .keywords(["connect", "source", "import"]),
        );
        command_targets.push(Target::AddAccount);
    }
    let command_group = CommandGroup::new().label("Commands").items(commands);

    let mut groups = vec![page_group, command_group];
    let mut targets = vec![page_targets, command_targets];

    if !accounts.is_empty() {
        groups.push(
            CommandGroup::new()
                .label("Accounts")
                .items(accounts.iter().map(|account| {
                    let provider = account
                        .descriptor()
                        .map(|source| source.display_name)
                        .unwrap_or_else(|| account.source_id.as_str());
                    CommandItem::new()
                        .label(account.name.clone())
                        .icon(IconName::Building2)
                        .keywords([provider.to_string(), account.source_id.as_str().to_string()])
                })),
        );
        targets.push(
            accounts
                .iter()
                .map(|account| Target::Account(account.id.clone()))
                .collect(),
        );
    }
    (groups, targets)
}

/// Open the palette over `accounts`, focused on its search field. Opening
/// it again while it is up does nothing: a second ⌘K must not stack
/// another dialog on top.
pub fn open(
    state: &Entity<CommandState>,
    accounts: Vec<CloudAccount>,
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let (groups, targets) = entries(&accounts);
    let groups = Rc::new(groups);
    let targets = Rc::new(targets);

    state.update(cx, |state, cx| state.set_query("", window, cx));
    let palette = state.clone();
    window.open_dialog(cx, move |dialog, window, _| {
        let palette = palette.clone();
        let groups = groups.clone();
        let targets = targets.clone();
        dialog
            .close_button(false)
            .p_0()
            // 560px at the default 16px rem: a search line, not a form.
            .w(rems(35.).to_pixels(window.rem_size()))
            .content(move |content, _, _| {
                let targets = targets.clone();
                content.child(
                    Command::new(&palette)
                        .bordered(false)
                        .placeholder("Search pages, accounts and commands")
                        .max_h(rems(24.))
                        .map(|command| {
                            groups
                                .iter()
                                .fold(command, |command, group| command.group(group.clone()))
                        })
                        .empty(|_, _, cx| {
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(theme::text_muted(cx))
                                .child("Nothing matches")
                        })
                        .on_confirm(move |index: IndexPath, window, cx| {
                            let target = targets
                                .get(index.section)
                                .and_then(|rows| rows.get(index.row))
                                .cloned();
                            window.close_dialog(cx);
                            match target {
                                Some(Target::AddAccount) => app::open_add_account(cx),
                                Some(Target::Account(id)) => app::navigate_to_account(id, cx),
                                Some(Target::Action) | None => {}
                            }
                        })
                        .footer(|_, _, cx| {
                            div()
                                .h_flex()
                                .gap_4()
                                .px_3()
                                .py_2()
                                .border_t_1()
                                .border_color(theme::card_border(cx))
                                .text_xs()
                                .text_color(theme::text_muted(cx))
                                .child("↑↓ to move")
                                .child("Enter to open")
                                .child("Esc to close")
                        }),
                )
            })
    });
    state.update(cx, |state, cx| state.focus(window, cx));
}

/// The views the palette's pages switch to — kept beside [`pages`] so a
/// page added to the sidebar and not the palette fails a test.
#[cfg(test)]
fn page_views() -> Vec<app::CurrentView> {
    use app::CurrentView;

    let mut views = vec![
        CurrentView::Overview,
        CurrentView::Alerts,
        CurrentView::Attribution,
        CurrentView::Models,
        CurrentView::Insights,
    ];
    #[cfg(not(target_family = "wasm"))]
    views.push(CurrentView::Query);
    views.push(CurrentView::Accounts);
    #[cfg(not(target_family = "wasm"))]
    views.extend([CurrentView::Rules, CurrentView::Settings]);
    views
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings gpui_kit's own `test` attribute, which
    // shadows the standard one.
    use super::{entries, page_views, pages, Target};
    use crate::model::CloudAccount;

    fn account(id: &str, name: &str) -> CloudAccount {
        crate::demo_data::account("aws", id, name, chrono::Utc::now())
    }

    #[test]
    fn every_sidebar_page_is_in_the_palette() {
        assert_eq!(pages().len(), page_views().len());
    }

    #[test]
    fn targets_line_up_with_rows() {
        let accounts = [account("a", "Prod"), account("b", "Staging")];
        let (groups, targets) = entries(&accounts);
        assert_eq!(groups.len(), targets.len());
        assert_eq!(targets[0].len(), pages().len());
        assert_eq!(groups[2].heading().map(|h| h.as_ref()), Some("Accounts"));
        assert!(matches!(&targets[2][1], Target::Account(id) if id == "b"));
    }

    #[test]
    fn no_accounts_no_accounts_group() {
        let (groups, _) = entries(&[]);
        assert!(groups
            .iter()
            .all(|group| group.heading().map(|h| h.as_ref()) != Some("Accounts")));
    }
}
