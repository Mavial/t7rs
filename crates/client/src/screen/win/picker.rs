use std::collections::BTreeMap;

use windows::Win32::Foundation::HWND;

use super::super::{AppWindows, Handle, PickerSources, Source, sources};
use super::app_info::{Apartment, AppInfo};
use super::capture::window_pid;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum AppKey {
    Identity(String),
    Window(Handle),
    League,
}

pub(crate) fn picker_sources() -> PickerSources {
    let _apartment = Apartment::enter();
    let mut result = PickerSources::default();
    let mut groups = BTreeMap::new();
    for item in sources() {
        if !item.source.is_window() {
            result.screens.push(item);
            continue;
        }
        let handle = item.source.handle();
        let hwnd = HWND(handle.0 as _);
        if is_own_window(hwnd) {
            continue;
        }
        let mut info = AppInfo::for_window(hwnd);
        let league = matches!(item.source, Source::League(_));
        let key =
            if league { AppKey::League } else { info.key.take().map_or(AppKey::Window(handle), AppKey::Identity) };
        let group = groups.entry(key).or_insert_with(|| AppWindows {
            name: if league { item.name.clone() } else { info.name().unwrap_or_else(|| item.name.clone()) },
            icon: info.icon(hwnd),
            windows: Vec::new(),
        });
        group.windows.push(item);
    }
    result.apps = groups.into_values().collect();
    result.apps.sort_by_cached_key(|app| app.name.to_lowercase());
    for app in &mut result.apps {
        app.windows.sort_by_cached_key(|item| (item.name.to_lowercase(), item.source.handle()));
    }
    result
}

fn is_own_window(window: HWND) -> bool {
    window_pid(window) == std::process::id()
}
