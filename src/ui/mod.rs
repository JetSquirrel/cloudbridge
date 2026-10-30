//! UI Module

pub mod account_detail;
pub mod accounts;
pub mod alerts;
pub mod attribution;
pub mod chart;
pub mod data;
mod data_health;
mod dialogs;
pub mod fmt;
pub mod insights;
pub mod models;
pub mod overview;
pub mod palette;
pub mod query;
pub mod rules;
pub mod settings;
pub mod theme;

use std::rc::Rc;

use gpui_kit::{Action, App, KeyBinding, KeyBindingContextPredicate};

/// A key binding whose keystrokes may use `secondary-`, which is ⌘ on macOS
/// and Ctrl elsewhere. `KeyBinding::new` parses without knowing the host OS
/// and panics on that alias, so this goes through `KeyBinding::load`, which
/// resolves it against `cx.platform_info()`.
pub fn platform_key_binding<A: Action>(
    keystrokes: &str,
    action: A,
    context: Option<&str>,
    cx: &App,
) -> KeyBinding {
    let context =
        context.map(|context| Rc::new(KeyBindingContextPredicate::parse(context).unwrap()));
    KeyBinding::load(keystrokes, Box::new(action), context, false, None, cx).unwrap()
}
