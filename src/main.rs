//! The desktop entry point.
//!
//! Everything the application is lives in the library, so that the browser
//! demo can build the same code. This only starts it.

fn main() {
    cloudbridge::desktop::run();
}
