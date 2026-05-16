// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Fr,
}

impl Lang {
    pub const fn label(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Fr => "Français",
        }
    }

    pub const ALL: &[Self] = &[Self::En, Self::Fr];
}

#[allow(dead_code)]
pub struct Strings {
    pub terminal: &'static str,
    pub terminal_n: &'static str,
    pub tab_atelier: &'static str,
    pub title_suffix: &'static str,

    // Tab bar
    pub new_tab: &'static str,

    // Context menu
    pub rename: &'static str,
    pub close: &'static str,
    pub copy: &'static str,
    pub copy_all: &'static str,
    pub paste: &'static str,
    pub reset_input_color: &'static str,
    pub disable_colors: &'static str,
    pub enable_colors: &'static str,
    pub screenshot_tab: &'static str,
    pub screenshot_app: &'static str,
    pub close_all: &'static str,
    pub remote_control: &'static str,
    pub fullscreen_mode: &'static str,
    pub windowed_mode: &'static str,
    pub preferences: &'static str,

    // Stats
    pub cpu: &'static str,
    pub power: &'static str,
    pub energy: &'static str,
    pub uptime: &'static str,

    // Rename dialog
    pub rename_tab: &'static str,

    // Exit confirm
    pub exit_close_or_reopen: &'static str,
    pub reopen_clean: &'static str,
    pub reopen_with_history: &'static str,
    pub close_tab: &'static str,

    // Close confirm
    pub cancel: &'static str,

    // QR modal
    pub scan_to_connect: &'static str,

    // Screenshots
    pub taking_screenshot: &'static str,
    pub rendering_screenshot: &'static str,
    pub saved: &'static str,
    pub screenshot_failed: &'static str,

    // Preferences
    pub theme: &'static str,
    pub opacity: &'static str,
    pub toggle_hotkeys: &'static str,
    pub add_key: &'static str,
    pub choose_a_key: &'static str,
    pub press_a_key: &'static str,
    pub key_already_registered: &'static str,
    pub remove: &'static str,
    pub language: &'static str,
    pub browser: &'static str,
    pub browser_placeholder: &'static str,
    pub code_editor: &'static str,
    pub code_editor_placeholder: &'static str,
    pub save: &'static str,
}

pub static EN: Strings = Strings {
    terminal: "Terminal",
    terminal_n: "Terminal",
    tab_atelier: "Tab Atelier",
    title_suffix: " — Tab Atelier",

    new_tab: "+",

    rename: "Rename",
    close: "Close",
    copy: "Copy",
    copy_all: "Copy All",
    paste: "Paste",
    reset_input_color: "Reset input & color",
    disable_colors: "Disable colors",
    enable_colors: "Enable colors",
    screenshot_tab: "Screenshot tab",
    screenshot_app: "Screenshot app",
    close_all: "Close All",
    remote_control: "Remote control",
    fullscreen_mode: "Fullscreen mode",
    windowed_mode: "Windowed mode",
    preferences: "Preferences",

    cpu: "CPU",
    power: "Power",
    energy: "Energy",
    uptime: "Active time",

    rename_tab: "Rename tab:",

    exit_close_or_reopen: "Close this tab or reopen a new shell?",
    reopen_clean: "Reopen (clean)",
    reopen_with_history: "Reopen (with history)",
    close_tab: "Close Tab",

    cancel: "Cancel",

    scan_to_connect: "Scan to connect from your phone",

    taking_screenshot: "Taking screenshot...",
    rendering_screenshot: "Rendering screenshot...",
    saved: "Saved",
    screenshot_failed: "Screenshot failed",

    theme: "Theme",
    opacity: "Opacity",
    toggle_hotkeys: "Toggle hotkeys",
    add_key: "Add key",
    choose_a_key: "Choose a key",
    press_a_key: "Press any key...",
    key_already_registered: "Already registered",
    remove: "Remove",
    language: "Language",
    browser: "Browser",
    browser_placeholder: "xdg-open (system default)",
    code_editor: "Code editor",
    code_editor_placeholder: "xdg-open (system default)",
    save: "Save",
};

pub static FR: Strings = Strings {
    terminal: "Terminal",
    terminal_n: "Terminal",
    tab_atelier: "Tab Atelier",
    title_suffix: " — Tab Atelier",

    new_tab: "+",

    rename: "Renommer",
    close: "Fermer",
    copy: "Copier",
    copy_all: "Tout copier",
    paste: "Coller",
    reset_input_color: "Réinitialiser saisie et couleur",
    disable_colors: "Désactiver les couleurs",
    enable_colors: "Activer les couleurs",
    screenshot_tab: "Capture de l'onglet",
    screenshot_app: "Capture de l'application",
    close_all: "Tout fermer",
    remote_control: "Télécommande",
    fullscreen_mode: "Mode plein écran",
    windowed_mode: "Mode fenêtré",
    preferences: "Préférences",

    cpu: "CPU",
    power: "Puissance",
    energy: "Énergie",
    uptime: "Temps actif",

    rename_tab: "Renommer l'onglet :",

    exit_close_or_reopen: "Fermer cet onglet ou rouvrir un nouveau shell ?",
    reopen_clean: "Rouvrir (vide)",
    reopen_with_history: "Rouvrir (avec historique)",
    close_tab: "Fermer l'onglet",

    cancel: "Annuler",

    scan_to_connect: "Scannez pour vous connecter depuis votre téléphone",

    taking_screenshot: "Capture en cours...",
    rendering_screenshot: "Rendu de la capture...",
    saved: "Enregistré",
    screenshot_failed: "Échec de la capture",

    theme: "Thème",
    opacity: "Opacité",
    toggle_hotkeys: "Raccourcis d'affichage",
    add_key: "Ajouter une touche",
    choose_a_key: "Choisir une touche",
    press_a_key: "Appuyez sur une touche...",
    key_already_registered: "Déjà enregistrée",
    remove: "Supprimer",
    language: "Langue",
    browser: "Navigateur",
    browser_placeholder: "xdg-open (par défaut)",
    code_editor: "Éditeur de code",
    code_editor_placeholder: "xdg-open (par défaut)",
    save: "Enregistrer",
};

pub fn detect_lang() -> Lang {
    for var in ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            let lower = val.to_lowercase();
            if lower.starts_with("fr") {
                return Lang::Fr;
            }
        }
    }
    Lang::En
}

pub fn strings(lang: Lang) -> &'static Strings {
    match lang {
        Lang::En => &EN,
        Lang::Fr => &FR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_langs_have_labels() {
        for lang in Lang::ALL {
            assert!(!lang.label().is_empty());
        }
    }

    #[test]
    fn strings_returns_correct_lang() {
        assert_eq!(strings(Lang::En).close, "Close");
        assert_eq!(strings(Lang::Fr).close, "Fermer");
    }

    #[test]
    fn detect_lang_defaults_to_en() {
        // In test env, LANG might be anything, but the function shouldn't panic
        let _ = detect_lang();
    }
}
