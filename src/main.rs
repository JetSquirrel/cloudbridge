//! The desktop entry point.
//!
//! Everything the application is lives in the library, so that the browser
//! demo can build the same code. This only starts it.
//!
//! A release build on Windows is a GUI-subsystem program, so launching it
//! does not open a console window beside the app. Debug builds keep the
//! console for their log output.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    cloudbridge::desktop::run();
}
