//! Bills View — every charge in the range, the price behind it, and the
//! parties it is split between.
//!
//! The third of magpie's Billing page that CloudBridge did not have: a
//! line-item ledger. The summary pages answer "how much" and "why"; this
//! answers "which row", down to the account, the API key and the user that
//! spent it, with the line's cost basis beside it so an estimate never
//! reads as money spent.

use std::collections::BTreeMap;
use std::rc::Rc;

use gpui_kit::component::button::*;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, StyledExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::data::{self, BillsData, Range};
use super::{fmt, theme};
use crate::model::{BillDim, BillFilter, BillKey, BillLine, ChargeCategory};
use crate::ui::theme::CardOutline as _;

/// Header of the line table's leading day column, also its column id.
const DAY: &str = "day";
const ACCOUNT: &str = "account";
const SERVICE: &str = "service";
const DESCRIPTION: &str = "description";
const CATEGORY: &str = "category";
const QUANTITY: &str = "quantity";
const BILLED: &str = "billed";
const LIST: &str = "list";

/// Compact cell padding shared by the line table's header and body cells.
fn cell_paddings() -> Edges<Pixels> {
    Edges {
        top: px(3.),
        bottom: px(3.),
        left: px(10.),
        right: px(10.),
    }
}

/// The line table's data source: the page of charges and the account names
/// their ids resolve to.
struct BillTableDelegate {
    columns: Vec<Column>,
    lines: Rc<Vec<BillLine>>,
    account_names: Rc<BTreeMap<String, String>>,
    currency: String,
}

impl BillTableDelegate {
    fn new() -> Self {
        let column = |id: &str, name: SharedString, width: f32, right: bool| {
            let mut column = Column::new(id.to_string(), name).width(width);
            column = if right { column.text_right() } else { column };
            column.paddings = Some(cell_paddings());
            column
        };

        Self {
            columns: vec![
                column(DAY, tr!("bills.col_day"), 92.0, false),
                column(ACCOUNT, tr!("bills.col_account"), 170.0, false),
                column(SERVICE, tr!("bills.col_service"), 170.0, false),
                column(DESCRIPTION, tr!("bills.col_description"), 260.0, false),
                column(CATEGORY, tr!("bills.col_category"), 96.0, false),
                column(QUANTITY, tr!("bills.col_quantity"), 120.0, true),
                column(BILLED, tr!("bills.col_billed"), 110.0, true),
                column(LIST, tr!("bills.col_list"), 110.0, true),
            ],
            lines: Rc::new(Vec::new()),
            account_names: Rc::new(BTreeMap::new()),
            currency: String::new(),
        }
    }

    fn set(
        &mut self,
        lines: Rc<Vec<BillLine>>,
        account_names: Rc<BTreeMap<String, String>>,
        currency: &str,
    ) {
        self.lines = lines;
        self.account_names = account_names;
        self.currency = currency.to_string();
    }

    /// The account a charge was billed to: the user's name for it when the
    /// account row still exists, its id otherwise.
    fn account_label(&self, line: &BillLine) -> String {
        self.account_names
            .get(&line.account_id)
            .cloned()
            .unwrap_or_else(|| line.account_id.clone())
    }

    /// The service cell, with the model a model-provider row bills under it.
    fn service_text(line: &BillLine) -> String {
        match &line.model {
            Some(model) => format!("{model} · {}", line.service),
            None => line.service.clone(),
        }
    }

    fn cell(&self, row_ix: usize, col_ix: usize) -> Option<String> {
        let line = self.lines.get(row_ix)?;
        let text = match self.columns.get(col_ix)?.key.as_str() {
            DAY => line.day.clone(),
            ACCOUNT => self.account_label(line),
            SERVICE => Self::service_text(line),
            DESCRIPTION => line.description.clone().unwrap_or_default(),
            CATEGORY => category_label(line.category),
            QUANTITY => match line.quantity {
                Some(quantity) => match &line.unit {
                    Some(unit) => format!("{} {unit}", fmt::quantity(quantity)),
                    None => fmt::quantity(quantity),
                },
                None => String::new(),
            },
            BILLED => amount_cell(line.billed, &self.currency),
            LIST => amount_cell(line.list, &self.currency),
            _ => String::new(),
        };
        Some(text)
    }
}

fn amount_cell(value: Option<f64>, currency: &str) -> String {
    match value {
        Some(value) => fmt::amount(value, currency),
        // No amount is not a zero: the row is a usage record.
        None => "—".to_string(),
    }
}

impl TableDelegate for BillTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.lines.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let column = self.column(col_ix, cx);
        div()
            .size_full()
            .when_some(column.paddings, |el, paddings| el.paddings(paddings))
            .h_flex()
            .child(
                div()
                    .flex_1()
                    .text_size(rems(0.85))
                    .text_color(theme::text_muted(cx))
                    .text_align(column.align)
                    .child(column.name.clone()),
            )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let column = self.column(col_ix, cx);
        let text = self.cell(row_ix, col_ix).unwrap_or_default();
        let muted = matches!(column.key.as_str(), DAY | CATEGORY);

        div()
            .size_full()
            .when_some(column.paddings, |el, paddings| el.paddings(paddings))
            .h_flex()
            .overflow_hidden()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_ellipsis()
                    .text_align(column.align)
                    .text_color(if muted {
                        theme::text_muted(cx)
                    } else {
                        theme::text_primary(cx)
                    })
                    .child(text),
            )
    }
}

/// "Bills View"
pub struct BillsView {
    /// Selected range in the header segmented control.
    range: Range,
    /// Which party the breakdown splits the range between.
    dim: BillDim,
    /// The groups the line list is narrowed to; empty for every charge.
    filter: BillFilter,
    /// Which page of the line list is on screen, 0-based.
    page: usize,
    /// The loaded page data; `None` until the first load completes.
    data: Option<BillsData>,
    loading: bool,
    error: Option<String>,
    /// Bumped on every load; a completion stamped with an older generation
    /// is discarded so a slow first load cannot clobber a newer result.
    generation: u64,
    /// The line table; its delegate is fed on every load.
    table: Entity<TableState<BillTableDelegate>>,
    /// A CSV export is being written; the button stays busy.
    exporting: bool,
    /// The last export's outcome, under the header.
    export_status: Option<Result<SharedString, SharedString>>,
}

impl BillsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| TableState::new(BillTableDelegate::new(), window, cx));
        Self {
            range: Range::Mtd,
            dim: BillDim::Account,
            filter: BillFilter::default(),
            page: 0,
            data: None,
            loading: false,
            error: None,
            generation: 0,
            table,
            exporting: false,
            export_status: None,
        }
    }

    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.data.is_none() && !self.loading {
            self.load(cx);
        }
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.load(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        self.generation += 1;
        let generation = self.generation;
        cx.notify();

        let range = self.range;
        let dim = self.dim;
        let filter = self.filter.clone();
        let page = self.page;
        cx.spawn(async move |this, cx| {
            let loaded = smol::unblock(move || data::load_bills(range, dim, filter, page)).await;
            this.update(cx, |this, cx| {
                this.loading = false;
                if this.generation != generation {
                    return;
                }
                match loaded {
                    Ok(loaded) => {
                        // The loader clamps the page to what the filter has;
                        // keep the view's own page in step so the pager
                        // does not offer a page that is not there.
                        this.page = loaded.page;
                        let lines = Rc::new(loaded.lines.clone());
                        let names = Rc::new(loaded.account_names.clone());
                        let currency = loaded.currency.clone();
                        this.table.update(cx, |table, cx| {
                            table.delegate_mut().set(lines, names, &currency);
                            table.refresh(cx);
                        });
                        this.data = Some(loaded);
                        this.error = None;
                    }
                    Err(error) => {
                        this.error = Some(error.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Switch the breakdown dimension. The filter stays: choosing an account
    /// and then keys lists that account's keys, which is how the panel is
    /// read down to "who spent this".
    fn set_dim(&mut self, dim: BillDim, cx: &mut Context<Self>) {
        if self.dim == dim {
            return;
        }
        self.dim = dim;
        self.load(cx);
    }

    /// Narrow the list to one breakdown row, or clear the narrowing when it
    /// is already the active one.
    fn toggle_filter(&mut self, filter: BillFilter, cx: &mut Context<Self>) {
        self.filter = if self.filter == filter {
            BillFilter::default()
        } else {
            filter
        };
        self.page = 0;
        self.load(cx);
    }

    fn set_page(&mut self, page: usize, cx: &mut Context<Self>) {
        if page == self.page {
            return;
        }
        self.page = page;
        self.load(cx);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.range;
        div()
            .w_full()
            .v_flex()
            .gap_3()
            .child(
                div()
                    .w_full()
                    .h_flex()
                    .items_start()
                    .justify_between()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(theme::page_title(cx, tr!("nav.bills")))
                            .when_some(self.data.as_ref(), |el, d| {
                                el.child(theme::caption(
                                    cx,
                                    tr!(
                                        "overview.reported_in",
                                        window = d.window_caption,
                                        currency = d.currency
                                    ),
                                ))
                            }),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .gap_1()
                                    .p_1()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(theme::card_border(cx))
                                    .children(Range::ALL.iter().map(|range| {
                                        let active = *range == selected;
                                        let button = Button::new(range.id())
                                            .label(range.label())
                                            .small()
                                            .rounded_full()
                                            .custom(theme::range_pill(cx, active))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if this.range != *range {
                                                    this.range = *range;
                                                    this.page = 0;
                                                    this.load(cx);
                                                }
                                            }));
                                        if active {
                                            button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                                        } else {
                                            button
                                        }
                                    })),
                            )
                            .when(cfg!(not(target_family = "wasm")), |el| {
                                el.child(self.render_export(cx))
                            }),
                    ),
            )
            .when_some(self.export_status.clone(), |el, status| {
                let (text, color) = match status {
                    Ok(text) => (text, theme::text_muted(cx)),
                    Err(text) => (text, theme::danger(cx)),
                };
                el.child(div().text_sm().text_color(color).child(text))
            })
    }

    fn render_export(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("bills-export")
            .label(tr!("bills.export"))
            .tooltip(tr!("bills.export_hint"))
            .custom(theme::outline_variant(cx))
            .card_outline(cx)
            .loading(self.exporting)
            .disabled(self.exporting)
            .on_click(cx.listener(|this, _, _, cx| this.export(cx)))
    }

    /// Ask where to save, then write every charge the filter selects — not
    /// just the page on screen.
    #[cfg(not(target_family = "wasm"))]
    fn export(&mut self, cx: &mut Context<Self>) {
        if self.exporting {
            return;
        }
        self.exporting = true;
        self.export_status = None;
        cx.notify();

        let directory = directories::UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(std::path::Path::to_path_buf))
            .or_else(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
            .unwrap_or_default();
        let name = format!(
            "cloudbridge-bills-{}.csv",
            chrono::Local::now().format("%Y-%m-%d")
        );
        let chosen = cx.prompt_for_new_path(&directory, Some(&name));
        let range = self.range;
        let filter = self.filter.clone();

        cx.spawn(async move |this, cx| {
            let outcome: Option<Result<SharedString, SharedString>> = match chosen.await {
                Ok(Ok(Some(path))) => Some(
                    smol::unblock(move || {
                        data::bill_export_lines(range, &filter)
                            .and_then(|lines| {
                                crate::ledger::bills_export::export_csv(&path, &lines)
                            })
                            .map(|rows| tr!("bills.exported", rows = rows, path = path.display()))
                            .map_err(|e| tr!("bills.export_failed", error = e))
                    })
                    .await,
                ),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(e)) => Some(Err(tr!("bills.save_dialog_failed", error = e))),
            };
            this.update(cx, |this, cx| {
                this.exporting = false;
                if let Some(Err(error)) = outcome.as_ref() {
                    tracing::error!("{}", error);
                }
                this.export_status = outcome;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The browser demo has no file dialog to choose a path with; the button
    /// is hidden there anyway, and this keeps the handler's call site one
    /// shape for both targets.
    #[cfg(target_family = "wasm")]
    fn export(&mut self, _cx: &mut Context<Self>) {}

    /// The range's price picture: billed, list, amortized, and the counts of
    /// lines no total can hold.
    fn render_provenance(&self, cx: &Context<Self>, d: &BillsData) -> impl IntoElement {
        let currency = d.currency.as_str();
        let p = d.provenance;
        let muted = |text: SharedString| div().text_color(theme::text_muted(cx)).child(text);

        let priced = match p.priced {
            0 => tr!("bills.no_priced"),
            n => tr!("bills.list_caption", n = n),
        };

        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(
                div()
                    .w_full()
                    .h_flex()
                    .items_stretch()
                    .gap_4()
                    .child(theme::stat_card(
                        "stat-bills-1",
                        cx,
                        tr!("bills.billed"),
                        fmt::amount(p.billed, currency),
                        muted(tr!("bills.billed_caption")),
                    ))
                    .child(theme::stat_card(
                        "stat-bills-2",
                        cx,
                        tr!("bills.list_price"),
                        fmt::amount(p.list, currency),
                        muted(priced),
                    ))
                    .child(theme::stat_card(
                        "stat-bills-3",
                        cx,
                        tr!("bills.effective"),
                        fmt::amount(p.effective, currency),
                        muted(tr!("bills.effective_caption")),
                    ))
                    .child(theme::stat_card(
                        "stat-bills-4",
                        cx,
                        tr!("bills.lines"),
                        fmt::quantity(p.lines as f64),
                        muted(tr!("bills.lines_caption")),
                    )),
            )
            .children(self.render_discount(cx, d))
            .when(p.unpriced > 0, |el| {
                el.child(theme::caption(cx, tr!("bills.unpriced", n = p.unpriced)))
            })
            .when(p.unconverted > 0, |el| {
                el.child(
                    div()
                        .text_sm()
                        .text_color(theme::warning_text(cx))
                        .child(tr!(
                            "bills.unconverted",
                            n = p.unconverted,
                            currency = d.currency
                        )),
                )
            })
    }

    /// How the effective cost sits against list, on the lines that reported
    /// a list price. Only when both sides have one: a percentage against a
    /// partial list is not a discount.
    fn render_discount(&self, cx: &Context<Self>, d: &BillsData) -> Option<AnyElement> {
        let p = d.provenance;
        if p.priced == 0 || p.list.abs() < fmt::DUST_THRESHOLD {
            return None;
        }
        Some(
            div()
                .text_sm()
                .text_color(theme::text_muted(cx))
                .child(tr!(
                    "bills.discount",
                    pct = format!("{:.1}%", p.effective / p.list * 100.0)
                ))
                .into_any_element(),
        )
    }

    fn render_breakdown(&self, cx: &mut Context<Self>, d: &BillsData) -> impl IntoElement {
        let currency = d.currency.as_str();
        let mut rows = Vec::with_capacity(d.groups.len());
        for (index, group) in d.groups.iter().enumerate() {
            let filter = group.filter.clone();
            let key_filter = filter.clone();
            let active = self.filter == filter;
            let share = group.share;
            let provider = group.provider.clone();

            rows.push(
                div()
                    // The label is not unique — two accounts can share a
                    // name — so the position is part of the id.
                    .id(SharedString::from(format!("bill-group-{index}")))
                    .role(Role::Button)
                    .aria_label(tr!(
                        "bills.group_a11y",
                        name = group.label.clone(),
                        amount = fmt::amount(group.amount, currency)
                    ))
                    .tab_index(0)
                    .cursor_pointer()
                    .h_flex()
                    .items_center()
                    .gap_3()
                    .py_2()
                    .px_2()
                    .rounded(cx.theme().radius)
                    .when(active, |el| el.bg(theme::accent(cx).opacity(0.10)))
                    .when(!active, |el| el.hover(|s| s.bg(theme::sidebar_bg(cx))))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_filter(filter.clone(), cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.toggle_filter(key_filter.clone(), cx);
                        }
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .v_flex()
                            .child(
                                div()
                                    .text_sm()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_color(if active {
                                        theme::accent(cx)
                                    } else {
                                        theme::text_primary(cx)
                                    })
                                    .child(group.label.clone()),
                            )
                            .when_some(provider, |el, provider| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::text_muted(cx))
                                        .child(provider),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted(cx))
                            .child(tr!("bills.group_lines", n = group.lines)),
                    )
                    .child(
                        div().w(rems(7.0)).px_2().child(
                            div()
                                .w_full()
                                .h_2()
                                .rounded_full()
                                .bg(theme::sidebar_bg(cx))
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(share as f32))
                                        .rounded_full()
                                        .bg(theme::accent(cx)),
                                ),
                        ),
                    )
                    .child(
                        div()
                            .w_24()
                            .text_right()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_primary(cx))
                            .child(fmt::amount(group.amount, currency)),
                    ),
            );
        }

        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_3()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .child(theme::section_title(cx, tr!("bills.breakdown")))
                    .child(div().h_flex().items_center().gap_1().children(
                        BillDim::ALL.iter().map(|dim| {
                            let active = *dim == self.dim;
                            let dim = *dim;
                            let button = Button::new(SharedString::from(format!(
                                "bills-dim-{}",
                                dim.as_str()
                            )))
                            .label(dim_label(dim))
                            .small()
                            .rounded_full()
                            .custom(theme::range_pill(cx, active))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.set_dim(dim, cx);
                                },
                            ));
                            if active {
                                button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                            } else {
                                button
                            }
                        }),
                    )),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .pb_2()
                    .child(
                        theme::header_cell(cx, dim_label(self.dim))
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(
                        theme::header_cell(cx, tr!("bills.col_lines"))
                            .w_20()
                            .text_right(),
                    )
                    .child(
                        theme::header_cell(cx, tr!("bills.col_share"))
                            .w(rems(7.0))
                            .px_2(),
                    )
                    .child(
                        theme::header_cell(cx, tr!("bills.col_amount"))
                            .w_24()
                            .text_right(),
                    ),
            )
            .children(rows)
            .when(d.groups.is_empty(), |el| {
                el.child(theme::caption(cx, tr!("bills.no_groups")))
            })
    }

    /// The line list: the filter chip, the table, and the pager.
    fn render_lines(&self, cx: &mut Context<Self>, d: &BillsData) -> impl IntoElement {
        let filter_chip = self.filter_label(d).map(|label| {
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(theme::pill_outline(cx, label))
                .child(
                    Button::new("bills-clear-filter")
                        .label(tr!("bills.clear_filter"))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.filter = BillFilter::default();
                            this.page = 0;
                            this.load(cx);
                        })),
                )
                .into_any_element()
        });

        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_3()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(theme::section_title(cx, tr!("bills.charges")))
                            .child(theme::caption(
                                cx,
                                tr!(
                                    "bills.charges_caption",
                                    n = d.total,
                                    page = d.page + 1,
                                    pages = d.pages
                                ),
                            )),
                    )
                    .children(filter_chip),
            )
            .child(if d.lines.is_empty() {
                theme::caption(cx, tr!("bills.no_rows")).into_any_element()
            } else {
                div()
                    .w_full()
                    .h(rems(28.0))
                    .overflow_hidden()
                    .child(
                        div().size_full().child(
                            DataTable::new(&self.table)
                                .small()
                                .stripe(true)
                                .scrollbar_visible(true, true),
                        ),
                    )
                    .into_any_element()
            })
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("bills-prev")
                            .label(tr!("bills.prev"))
                            .small()
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .disabled(d.page == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let page = this.page.saturating_sub(1);
                                this.set_page(page, cx);
                            })),
                    )
                    .child(div().text_xs().text_color(theme::text_muted(cx)).child(tr!(
                        "bills.page_of",
                        page = d.page + 1,
                        pages = d.pages
                    )))
                    .child(
                        Button::new("bills-next")
                            .label(tr!("bills.next"))
                            .small()
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .disabled(d.page + 1 >= d.pages)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_page(this.page + 1, cx);
                            })),
                    ),
            )
    }

    /// What the line list is narrowed to, named by the dimension it came
    /// from; `None` for every charge.
    fn filter_label(&self, d: &BillsData) -> Option<SharedString> {
        if let Some(account_id) = &d.filter.account_id {
            let name = d
                .account_names
                .get(account_id)
                .cloned()
                .unwrap_or_else(|| account_id.clone());
            return Some(tr!("bills.filter_account", name = name));
        }
        match d.filter.credential.as_ref() {
            Some(BillKey::Unnamed) => return Some(tr!("bills.no_credential")),
            Some(BillKey::Named(value)) => {
                return Some(tr!("bills.filter_credential", name = value.as_str()))
            }
            None => {}
        }
        match d.filter.principal.as_ref() {
            Some(BillKey::Unnamed) => Some(tr!("bills.no_principal")),
            Some(BillKey::Named(value)) => {
                Some(tr!("bills.filter_principal", name = value.as_str()))
            }
            None => None,
        }
    }

    fn render_empty(&self, cx: &Context<Self>) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_primary(cx))
                    .child(tr!("bills.empty")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text_muted(cx))
                    .child(tr!("bills.empty_hint")),
            )
    }

    fn render_loading(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap_6()
            .child(div().h_flex().gap_4().children((0..4).map(|_| {
                theme::card(cx)
                    .flex_1()
                    .min_w_0()
                    .p_5()
                    .v_flex()
                    .gap_2()
                    .child(Skeleton::new().w_20().h_3())
                    .child(Skeleton::new().w_24().h_6())
                    .child(Skeleton::new().w_32().h_3())
            })))
            .child(
                theme::card(cx)
                    .w_full()
                    .p_5()
                    .v_flex()
                    .gap_3()
                    .child(Skeleton::new().w_32().h_4())
                    .children((0..5).map(|_| Skeleton::new().w_full().h_4())),
            )
    }

    fn render_error_banner(&self, cx: &Context<Self>, error: &str) -> impl IntoElement {
        div()
            .w_full()
            .p_3()
            .rounded_md()
            .bg(theme::danger_bg(cx))
            .text_sm()
            .text_color(theme::danger(cx))
            .child(error.to_string())
    }

    fn render_error(&self, cx: &mut Context<Self>, error: &str) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text_primary(cx))
                    .child(error.to_string()),
            )
            .child(
                div().h_flex().child(
                    Button::new("bills-retry")
                        .label(tr!("common.retry"))
                        .custom(theme::outline_variant(cx))
                        .card_outline(cx)
                        .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                ),
            )
    }
}

impl Render for BillsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = if let Some(d) = &self.data {
            let content: AnyElement = if d.groups.is_empty() {
                self.render_empty(cx).into_any_element()
            } else {
                div()
                    .v_flex()
                    .gap_6()
                    .child(self.render_provenance(cx, d))
                    .child(self.render_breakdown(cx, d))
                    .child(self.render_lines(cx, d))
                    .into_any_element()
            };
            div()
                .v_flex()
                .gap_6()
                .when_some(self.error.clone(), |el, error| {
                    el.child(self.render_error_banner(cx, &error))
                })
                .child(content)
                .into_any_element()
        } else if let Some(error) = self.error.clone() {
            self.render_error(cx, &error).into_any_element()
        } else if self.loading {
            self.render_loading(cx).into_any_element()
        } else {
            self.render_empty(cx).into_any_element()
        };

        div()
            .id("bills")
            .size_full()
            .v_flex()
            .gap_6()
            .p_8()
            .bg(theme::app_bg(cx))
            .overflow_y_scroll()
            .child(self.render_header(cx))
            .child(body)
    }
}

/// The breakdown tab's label, and the header cell of its name column.
fn dim_label(dim: BillDim) -> SharedString {
    match dim {
        BillDim::Account => tr!("bills.tab_account"),
        BillDim::Credential => tr!("bills.tab_credential"),
        BillDim::Principal => tr!("bills.tab_principal"),
    }
}

/// A charge category's name, translated for display; the stored value stays
/// English, as everywhere it is an identity.
fn category_label(category: ChargeCategory) -> String {
    match category {
        ChargeCategory::Usage => tr!("bills.category_usage"),
        ChargeCategory::Purchase => tr!("bills.category_purchase"),
        ChargeCategory::Credit => tr!("bills.category_credit"),
        ChargeCategory::Tax => tr!("bills.category_tax"),
        ChargeCategory::Adjustment => tr!("bills.category_adjustment"),
    }
    .into()
}
