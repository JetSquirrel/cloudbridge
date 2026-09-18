//! Query View — ad-hoc read-only SQL against the local ledger, with a
//! template picker on the left and the result table on the right.

use std::rc::Rc;

use gpui_kit::component::{
    button::*,
    input::{Editor, EditorState, InputEvent, TabSize},
    table::{Column, DataTable, TableDelegate, TableState},
    ActiveTheme, Disableable as _, Sizable as _, StyledExt,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{data, theme};

actions!(query, [RunQuery]);

/// Key context of the page root, so ⌘Enter runs while the editor is not
/// focused. While it is, the editor's own (deeper) Input context binds the
/// same keystroke to "insert newline" and wins the dispatch — that path is
/// handled by the `PressEnter` subscription in `new`.
const QUERY_KEY_CONTEXT: &str = "Query";

/// What the editor is prefilled with: how to run, what to query, and that
/// the ledger is read-only from here.
const WELCOME_SQL: &str = "-- ⌘Enter runs the query. Everything here is read-only.
-- The main view is v_charge_normalized: one row per charge,
-- amounts in the reporting currency.

SELECT billing_period,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
GROUP BY billing_period, reporting_currency
ORDER BY billing_period DESC
LIMIT 12";

/// Row-number column header; it leads every result table.
const INDEX_COLUMN_HEADER: &str = "#";
/// How many leading columns sit in front of the data columns.
const LEADING_COLUMNS: usize = 1;

/// Compact cell padding shared by header and body cells.
fn cell_paddings() -> Edges<Pixels> {
    Edges {
        top: px(2.),
        bottom: px(2.),
        left: px(10.),
        right: px(10.),
    }
}

/// The result pane's state machine.
enum QueryStatus {
    /// Nothing has run yet.
    Empty,
    /// A run is in flight.
    Running,
    /// The last run produced rows.
    Rows(Rc<data::QueryResultData>),
    /// The last run failed; the text is rendered verbatim.
    Failed(String),
}

/// The result table's data source: the leading `#` row-number column plus
/// the query's data columns, shared with the view rather than copied.
struct QueryTableDelegate {
    columns: Vec<Column>,
    result: Option<Rc<data::QueryResultData>>,
}

impl QueryTableDelegate {
    fn new() -> Self {
        Self {
            columns: Vec::new(),
            result: None,
        }
    }

    fn set_result(&mut self, result: Rc<data::QueryResultData>) {
        let mut index_column = Column::new(INDEX_COLUMN_HEADER, INDEX_COLUMN_HEADER)
            .width(48.)
            .text_right()
            .resizable(false);
        index_column.paddings = Some(cell_paddings());

        let mut columns = Vec::with_capacity(result.columns.len() + LEADING_COLUMNS);
        columns.push(index_column);
        columns.extend(result.columns.iter().enumerate().map(|(ix, name)| {
            let mut column = Column::new(format!("c{ix}"), name.clone());
            if result.numeric.get(ix).copied().unwrap_or(false) {
                column = column.text_right();
            }
            column.paddings = Some(cell_paddings());
            column
        }));

        self.columns = columns;
        self.result = Some(result);
    }

    /// One cell's display text: the string the query layer produced, with
    /// SQL NULL carried as `None`.
    fn cell(&self, row_ix: usize, col_ix: usize) -> Option<&str> {
        self.result
            .as_ref()?
            .rows
            .get(row_ix)?
            .get(col_ix.saturating_sub(LEADING_COLUMNS))?
            .as_deref()
    }
}

impl TableDelegate for QueryTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.result
            .as_ref()
            .map(|result| result.rows.len())
            .unwrap_or(0)
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
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .font_family(cx.theme().mono_font_family.clone())
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
        let base = div()
            .size_full()
            .when_some(column.paddings, |el, paddings| el.paddings(paddings))
            .h_flex()
            .overflow_hidden();

        if col_ix == 0 {
            // Row number: the visible position (1-based), muted.
            return base
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(theme::text_muted(cx))
                        .text_align(column.align)
                        .child((row_ix + 1).to_string()),
                )
                .into_any_element();
        }

        let (text, is_null) = match self.cell(row_ix, col_ix) {
            Some(text) => (text.to_string(), false),
            None => ("NULL".to_string(), true),
        };

        base.child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_ellipsis()
                .font_family(cx.theme().mono_font_family.clone())
                .text_align(column.align)
                .text_color(if is_null {
                    theme::text_muted(cx)
                } else {
                    theme::text_primary(cx)
                })
                .child(text),
        )
        .into_any_element()
    }
}

/// Query View
pub struct QueryView {
    editor: Entity<EditorState>,
    table: Entity<TableState<QueryTableDelegate>>,
    /// The result pane's state.
    status: QueryStatus,
    /// A run is in flight; the Run button is disabled and a second run is
    /// a no-op.
    running: bool,
    /// Bumped on every run so an overlapping older run discards its result
    /// instead of clobbering fresher state.
    run_generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl QueryView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .tab_size(TabSize {
                    tab_size: 2,
                    hard_tabs: false,
                })
                .default_value(WELCOME_SQL)
        });
        let table = cx.new(|cx| TableState::new(QueryTableDelegate::new(), window, cx));

        // Registered once: bind_keys appends, and the view may be rebuilt on
        // every navigation.
        static BIND_KEYS: std::sync::Once = std::sync::Once::new();
        BIND_KEYS.call_once(|| {
            cx.bind_keys([KeyBinding::new(
                "cmd-enter",
                RunQuery,
                Some(QUERY_KEY_CONTEXT),
            )]);
        });

        // ⌘Enter reaches the focused editor as `secondary-enter`, which the
        // Input key context (deeper than the page's) binds to "insert
        // newline". The page's own keybinding therefore never fires while the
        // editor is focused; the PressEnter event is the only signal left.
        // The newline is already inserted by then, so undo it before running.
        let subscription = cx.subscribe_in(&editor, window, |this, editor, event, window, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: true,
                    ..
                }
            ) {
                Self::remove_secondary_enter_newline(editor, window, cx);
                this.run(cx);
            }
        });

        Self {
            editor,
            table,
            status: QueryStatus::Empty,
            running: false,
            run_generation: 0,
            _subscriptions: vec![subscription],
        }
    }

    /// Called by the app shell when this page is navigated to. The query
    /// page has nothing to refresh: the editor's text and the last result
    /// stay as the user left them.
    pub fn reload(&mut self, _cx: &mut Context<Self>) {}

    /// The editor inserts `"\n" + indent` before emitting `PressEnter`; the
    /// cursor then sits right after the insertion. Delete exactly that
    /// insertion — walk back over the indent, then require a newline, and do
    /// nothing if the text before the cursor does not match that shape.
    fn remove_secondary_enter_newline(
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let range = editor.update(cx, |editor, _| {
            let cursor = editor.cursor();
            let text = editor.text();
            let mut start = cursor;
            let mut chars = text.chars_at(cursor);
            let mut prev = chars.prev();
            // The inserted indent is plain spaces/tabs (ASCII), so byte
            // arithmetic on `start` stays on char boundaries.
            while matches!(prev, Some(' ' | '\t')) {
                start -= 1;
                prev = chars.prev();
            }
            if prev != Some('\n') {
                return None;
            }
            Some(start - 1..cursor)
        });
        if let Some(range) = range {
            editor.update(cx, |editor, cx| {
                editor.set_selected_range(range, cx);
                editor.replace("", window, cx);
            });
        }
    }

    fn focus_editor(&self, window: &mut Window, cx: &mut App) {
        self.editor.read(cx).focus_handle(cx).focus(window, cx);
    }

    /// Put a template's SQL into the editor and focus it.
    fn fill_from_template(
        &mut self,
        template: &data::QueryTemplate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.set_value(template.sql, window, cx);
        });
        self.focus_editor(window, cx);
    }

    /// Run the editor's SQL off the UI thread: the query is blocking
    /// DuckDB, so it runs on a worker thread like the other pages' loads
    /// do. Only the newest generation may write state; an older result is
    /// discarded.
    fn run(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let sql = self.editor.read(cx).value().to_string();
        if sql.trim().is_empty() {
            return;
        }

        self.running = true;
        self.status = QueryStatus::Running;
        self.run_generation += 1;
        let generation = self.run_generation;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(move || data::run_adhoc_query(sql)).await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    if generation != this.run_generation {
                        return;
                    }
                    this.running = false;
                    match outcome {
                        Ok(result) => {
                            let result = Rc::new(result);
                            this.table.update(cx, |table, cx| {
                                table.delegate_mut().set_result(result.clone());
                                table.refresh(cx);
                            });
                            this.status = QueryStatus::Rows(result);
                        }
                        Err(e) => {
                            this.status = QueryStatus::Failed(e);
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    // ==================== Rendering ====================

    fn render_templates(&self, cx: &Context<Self>) -> impl IntoElement {
        // Group templates by category, in order of first appearance.
        let mut groups: Vec<(&'static str, Vec<&'static data::QueryTemplate>)> = Vec::new();
        for template in data::query_templates() {
            match groups
                .iter_mut()
                .find(|(category, _)| *category == template.category)
            {
                Some((_, templates)) => templates.push(template),
                None => groups.push((template.category, vec![template])),
            }
        }

        let hover_bg = theme::sidebar_bg(cx);

        // h_flex on the body centers children vertically, so both columns
        // need an explicit full height: without it the templates render at
        // content height (no scroll, clipped past the window) and the
        // result pane's flex_1 collapses to zero.
        div()
            .id("query-templates")
            .w_64()
            .h_full()
            .flex_shrink_0()
            .v_flex()
            .gap_4()
            .overflow_y_scroll()
            .children(groups.into_iter().map(|(category, templates)| {
                div()
                    .v_flex()
                    .gap_2()
                    .child(theme::section_title(cx, category))
                    .children(templates.into_iter().map(|template| {
                        theme::card(cx)
                            .id(SharedString::from(format!("template-{}", template.title)))
                            .p_3()
                            .v_flex()
                            .gap_1()
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover_bg))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text_primary(cx))
                                    .child(template.title),
                            )
                            .child(theme::caption(cx, template.description))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.fill_from_template(template, window, cx);
                            }))
                    }))
            }))
    }

    fn render_status_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let line: Option<(String, Hsla)> = match &self.status {
            QueryStatus::Running => Some(("Running…".to_string(), theme::text_muted(cx))),
            QueryStatus::Rows(result) if result.truncated => Some((
                format!(
                    "Showing first {} rows — result truncated · {} ms",
                    result.rows.len(),
                    result.elapsed_ms
                ),
                theme::warning_text(cx),
            )),
            QueryStatus::Rows(result) => Some((
                format!("{} rows · {} ms", result.rows.len(), result.elapsed_ms),
                theme::text_muted(cx),
            )),
            _ => None,
        };

        // The row keeps its slot whether or not it has text, so the result
        // pane does not jump when a run starts.
        div()
            .flex_shrink_0()
            .h_5()
            .text_sm()
            .when_some(line, |el, (text, color)| {
                el.child(div().text_color(color).child(text))
            })
    }

    fn render_results(&self, cx: &Context<Self>) -> AnyElement {
        match &self.status {
            QueryStatus::Empty => theme::card(cx)
                .w_full()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(theme::caption(cx, "Run a query or pick a template"))
                .into_any_element(),
            QueryStatus::Running => theme::card(cx)
                .w_full()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(theme::caption(cx, "Running…"))
                .into_any_element(),
            QueryStatus::Rows(_) => theme::card(cx)
                .w_full()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(
                    div().size_full().child(
                        DataTable::new(&self.table)
                            .small()
                            .stripe(true)
                            .scrollbar_visible(true, true),
                    ),
                )
                .into_any_element(),
            QueryStatus::Failed(error) => div()
                .w_full()
                .flex_shrink_0()
                .p_4()
                .rounded_md()
                .bg(theme::danger_bg(cx))
                .child(
                    div()
                        .text_sm()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(theme::danger(cx))
                        .child(error.clone()),
                )
                .into_any_element(),
        }
    }
}

impl Render for QueryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex_shrink_0()
            .h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::page_title(cx, "Query"))
                    .child(theme::caption(cx, "Read-only SQL against the local ledger")),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted(cx))
                            .child("⌘Enter"),
                    )
                    .child(
                        Button::new("run-query")
                            .label("Run")
                            .custom(theme::accent_variant(cx))
                            .loading(self.running)
                            .disabled(self.running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run(cx);
                            })),
                    ),
            );

        let body = div()
            .flex_1()
            .min_h_0()
            .h_flex()
            .gap_6()
            .child(self.render_templates(cx))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .v_flex()
                    .gap_4()
                    .child(
                        theme::card(cx)
                            .flex_shrink_0()
                            .p_4()
                            .child(Editor::new(&self.editor).h(px(180.)).bordered(false)),
                    )
                    .child(self.render_status_row(cx))
                    .child(self.render_results(cx)),
            );

        div()
            .size_full()
            .key_context(QUERY_KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| {
                this.run(cx);
            }))
            .v_flex()
            .gap_6()
            .p_8()
            .bg(theme::app_bg(cx))
            .child(header)
            .child(body)
    }
}
