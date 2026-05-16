// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gpui::{Context, Render, TestAppContext, Window};
use tab_atelier::FontConfig;

struct TestModel {
    font_config: FontConfig,
}

impl Render for TestModel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        gpui::div()
    }
}

#[gpui::test]
fn test_font_config_in_gpui_context(cx: &mut TestAppContext) {
    let window = cx.add_window(|_window: &mut Window, _cx: &mut Context<TestModel>| TestModel {
        font_config: FontConfig::default(),
    });

    window
        .update(cx, |model, _window, _cx| {
            assert_eq!(model.font_config.family, "monospace");
            assert!((model.font_config.size - 16.0).abs() < f32::EPSILON);
            assert_eq!(model.font_config.weight, 400);
            assert!((model.font_config.scroll_sensitivity - 1.0).abs() < f32::EPSILON);
        })
        .unwrap();
}

struct UrlModel {
    urls: Vec<(usize, usize, String, bool)>,
}

impl Render for UrlModel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        gpui::div()
    }
}

#[gpui::test]
fn test_detect_urls_in_gpui_context(cx: &mut TestAppContext) {
    use tab_atelier::detect_urls;

    let window = cx.add_window(|_window: &mut Window, _cx: &mut Context<UrlModel>| {
        let text = "Visit https://example.com or open /home/user/file.rs:10";
        UrlModel { urls: detect_urls(text) }
    });

    window
        .update(cx, |model, _window, _cx| {
            assert_eq!(model.urls.len(), 2);
            assert_eq!(model.urls[0].2, "https://example.com");
            assert!(!model.urls[0].3); // not a file
            assert!(model.urls[1].2.contains("/home/user/file.rs:10"));
            assert!(model.urls[1].3); // is a file
        })
        .unwrap();
}

struct KeycodeModel {
    keycode: Option<u8>,
    label: String,
}

impl Render for KeycodeModel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        gpui::div()
    }
}

#[gpui::test]
fn test_hotkey_functions_in_gpui_context(cx: &mut TestAppContext) {
    use tab_atelier::{gpui_key_to_keycode, keycode_label};

    let window = cx.add_window(|_window: &mut Window, _cx: &mut Context<KeycodeModel>| {
        let keycode = gpui_key_to_keycode("`");
        let label = keycode.map_or_else(|| "unknown".into(), keycode_label);
        KeycodeModel { keycode, label }
    });

    window
        .update(cx, |model, _window, _cx| {
            assert_eq!(model.keycode, Some(49));
            assert!(model.label.contains('`'));
        })
        .unwrap();
}

struct PrefsModel {
    prefs: tab_atelier::Preferences,
}

impl Render for PrefsModel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        gpui::div()
    }
}

#[gpui::test]
fn test_load_preferences_missing_dir(cx: &mut TestAppContext) {
    use tab_atelier::load_preferences;
    use std::path::Path;

    let window = cx.add_window(|_window: &mut Window, _cx: &mut Context<PrefsModel>| PrefsModel {
        prefs: load_preferences(Path::new("/nonexistent/path")),
    });

    window
        .update(cx, |model, _window, _cx| {
            // Should return defaults
            assert!(model.prefs.hotkeys.is_empty());
            assert!(model.prefs.theme.is_none());
            assert!(model.prefs.lang.is_none());
        })
        .unwrap();
}
