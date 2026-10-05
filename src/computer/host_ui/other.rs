//! Platforms without a host UI yet.

use std::time::Duration;

use super::Strings;

pub(super) fn run_with_main_loop(body: impl FnOnce()) -> ! {
    body();
    unreachable!("the body exits the process")
}

pub(super) fn preferred_language() -> Option<String> {
    std::env::var("LANG").ok()
}

pub(super) fn show_status(_strings: &Strings, _summary: &str) {}

pub(super) fn hide_status() {}

pub(super) fn mark_point(_x: f64, _y: f64) {}

pub(super) fn confirm(
    _strings: &Strings,
    _title: &str,
    _message: &str,
    _timeout: Duration,
) -> bool {
    false
}
