use crate::log_viewer;
use crashes;
use fs::Fs;
use gpui::{App, Global, UpdateGlobal as _};
use settings::SettingsStore;
use std::sync::Arc;

#[allow(dead_code)]
pub struct CrashHandler(pub Arc<crashes::Client>);

impl Global for CrashHandler {}

pub fn init(cx: &mut App) {
    cx.on_action(quit);
    log_viewer::init(cx);
}

fn quit(_: &zed_actions::Quit, cx: &mut App) {
    cx.quit();
}

pub fn watch_settings_files(fs: Arc<dyn Fs>, cx: &mut App) {
    SettingsStore::update_global(cx, |store, cx| {
        store.watch_settings_files(fs, cx, |_, _, _| {});
    });
}
