//! Port of `packages/coding-agent/src/cli/startup-ui.ts`.
//!
//! Standalone startup TUIs (selectors, input, first-time setup) shown before the interactive
//! mode starts.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use pi_tui::terminal::ProcessTerminal;
use pi_tui::tui::{ComponentRef, Tui, TuiOptions, TuiStopOptions};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::config::{APP_NAME, CONFIG_DIR_NAME, ENV_AGENT_DIR, PACKAGE_NAME, get_agent_dir, get_settings_path};
use crate::core::experimental::are_experimental_features_enabled;
use crate::core::keybindings::KeybindingsManager;
use crate::core::package_manager::{DefaultPackageManager, MissingSourceAction, PackageManager, PackageManagerOptions, ResolvedResource};
use crate::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use crate::modes::interactive::components::extension_input::{ExtensionInputComponent, ExtensionInputOptions};
use crate::modes::interactive::components::extension_selector::{
    ExtensionSelectorComponent, ExtensionSelectorOptions,
};
use crate::modes::interactive::components::first_time_setup::{
    FirstTimeSetupComponent, FirstTimeSetupOptions, FirstTimeSetupResult,
};
use crate::modes::interactive::theme::system_theme::SYSTEM_THEME_NAME;
use crate::modes::interactive::theme::theme::{
    Theme, get_terminal_theme, init_theme, load_theme_from_path, mark_terminal_colors_pending,
    resolve_theme_setting, set_registered_themes, set_terminal_colors, set_theme,
};
use crate::modes::interactive::theme::theme_controller::request_terminal_colors;

const OFFICIAL_PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";
const OFFICIAL_APP_NAME: &str = "pi";
const OFFICIAL_CONFIG_DIR_NAME: &str = ".pi";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DistributionMetadata {
    pub package_name: String,
    pub app_name: String,
    pub config_dir_name: String,
}

fn is_official_distribution(metadata: &DistributionMetadata) -> bool {
    metadata.package_name == OFFICIAL_PACKAGE_NAME
        && metadata.app_name == OFFICIAL_APP_NAME
        && metadata.config_dir_name == OFFICIAL_CONFIG_DIR_NAME
}

fn load_themes(resources: &[ResolvedResource]) -> Vec<Arc<Theme>> {
    let mut themes: Vec<Arc<Theme>> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for resource in resources {
        if !resource.enabled {
            continue;
        }
        // Startup prompts should not fail because a theme is broken. The normal
        // resource loader reports theme diagnostics later in startup.
        let path = resource.path.clone();
        let Ok(Ok(loaded_theme)) = std::panic::catch_unwind(move || load_theme_from_path(&path, None)) else {
            continue;
        };
        if let Some(name) = loaded_theme.name()
            && !seen.insert(name)
        {
            continue;
        }
        themes.push(loaded_theme);
    }
    themes
}

async fn load_startup_themes(settings_manager: &SettingsManager) -> Vec<Arc<Theme>> {
    let global_settings_manager = SettingsManager::in_memory(
        Some(&settings_manager.get_global_settings()),
        Some(&SettingsManagerCreateOptions { project_trusted: Some(false) }),
    );
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let package_manager = DefaultPackageManager::new(&PackageManagerOptions {
        cwd,
        agent_dir: get_agent_dir(),
        settings_manager: global_settings_manager,
        builtin_extensions: None,
    });
    let skip: Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, anyhow::Result<MissingSourceAction>> + Send + Sync> =
        Arc::new(|_| Box::pin(async { Ok(MissingSourceAction::Skip) }));
    match package_manager.resolve(Some(skip)).await {
        Ok(resolved) => load_themes(&resolved.themes),
        Err(_) => Vec::new(),
    }
}

pub async fn create_startup_tui(settings_manager: Arc<SettingsManager>) -> anyhow::Result<Arc<Tui>> {
    pi_tui::terminal_image::set_capability_overrides(settings_manager.get_terminal_capability_overrides());
    set_registered_themes(load_startup_themes(&settings_manager).await);
    // The system theme starts in grayscale until the terminal reports its colors.
    mark_terminal_colors_pending();
    let theme_name = resolve_theme_setting(settings_manager.get_theme_setting().as_deref(), &get_terminal_theme())
        .unwrap_or_else(|| SYSTEM_THEME_NAME.to_string());
    init_theme(Some(&theme_name), None);
    pi_tui::keybindings::set_keybindings(KeybindingsManager::create(None).tui_manager());
    let ui = Tui::main_screen_with(
        Arc::new(ProcessTerminal::new()),
        TuiOptions {
            show_hardware_cursor: Some(settings_manager.get_show_hardware_cursor()),
            log_directory: Some(get_agent_dir().into()),
            event_loop: None,
        },
    );
    ui.set_clear_on_shrink(settings_manager.get_clear_on_shrink());
    Ok(ui)
}

pub fn start_startup_tui(ui: Arc<Tui>, settings_manager: Arc<SettingsManager>) {
    drop(start_startup_tui_tracked(ui, settings_manager));
}

/// [`start_startup_tui`], returning the task that applies the terminal colors (it ends once the
/// colors arrive or the query times out).
pub fn start_startup_tui_tracked(ui: Arc<Tui>, settings_manager: Arc<SettingsManager>) -> tokio::task::JoinHandle<()> {
    ui.start();
    let theme_setting = settings_manager.get_theme_setting();
    query_startup_terminal_colors(
        ui,
        Arc::new(move || {
            let name = resolve_theme_setting(theme_setting.as_deref(), &get_terminal_theme())
                .unwrap_or_else(|| SYSTEM_THEME_NAME.to_string());
            set_theme(&name, None);
        }),
    )
}

/// Query the terminal's colors without waiting for them. When they arrive, including after the timeout,
/// record them for the system theme and "" (terminal default) tokens, run `on_colors`, and re-render.
fn query_startup_terminal_colors(ui: Arc<Tui>, on_colors: Arc<dyn Fn() + Send + Sync>) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(&ui);
    let apply = Arc::new(move |colors| {
        set_terminal_colors(colors);
        on_colors();
        if let Some(ui) = weak.upgrade() {
            ui.invalidate();
            ui.request_render(false);
        }
    });
    crate::utils::uncaught::spawn(async move {
        let _ = request_terminal_colors(ui, apply).await;
    })
}

async fn clear_startup_tui(ui: &Tui) {
    ui.clear();
    ui.request_render(false);
    tokio::time::sleep(Duration::from_millis(25)).await;
}

/// First-time setup runs when all of these hold:
/// - this is the official Pi distribution (not a fork/rebrand)
/// - experimental features are enabled (PI_EXPERIMENTAL=1)
/// - the default agent directory is used (no custom agent dir override)
/// - setup was not completed before (settings.json does not exist)
pub fn should_run_first_time_setup(settings_path: Option<&str>) -> bool {
    if !is_official_distribution(&DistributionMetadata {
        package_name: PACKAGE_NAME.to_string(),
        app_name: APP_NAME.to_string(),
        config_dir_name: CONFIG_DIR_NAME.to_string(),
    }) {
        return false;
    }
    if !are_experimental_features_enabled() {
        return false;
    }
    if std::env::var(ENV_AGENT_DIR).is_ok_and(|v| !v.is_empty()) {
        return false;
    }
    let settings_path = settings_path.map(str::to_string).unwrap_or_else(get_settings_path);
    !Path::new(&settings_path).exists()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShowStartupSelectorOptions {
    pub label: String,
    pub value: Value,
}

/// A one-shot result slot shared by component callbacks.
fn one_shot<T: Send + 'static>() -> (Arc<dyn Fn(T) + Send + Sync>, oneshot::Receiver<T>) {
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    (
        Arc::new(move |value| {
            if let Some(tx) = tx.lock().take() {
                let _ = tx.send(value);
            }
        }),
        rx,
    )
}

pub async fn show_startup_selector(
    settings_manager: Arc<SettingsManager>,
    title: &str,
    options: &[ShowStartupSelectorOptions],
) -> anyhow::Result<Option<Value>> {
    let ui = create_startup_tui(settings_manager.clone()).await?;
    let (finish, result) = one_shot::<Option<Value>>();
    let entries = options.to_vec();
    let on_select = {
        let finish = finish.clone();
        Arc::new(move |label: String| {
            finish(entries.iter().find(|entry| entry.label == label).map(|entry| entry.value.clone()))
        })
    };
    let on_cancel = {
        let finish = finish.clone();
        Arc::new(move || finish(None))
    };
    let labels: Vec<String> = options.iter().map(|option| option.label.clone()).collect();
    let selector: ComponentRef = Arc::new(ExtensionSelectorComponent::new(
        title,
        &labels,
        on_select,
        on_cancel,
        Some(&ExtensionSelectorOptions {
            tui: Some(ui.clone()),
            timeout: None,
            on_toggle_tools_expanded: None,
            description: None,
        }),
    ));
    ui.add_child(selector.clone());
    ui.set_focus(Some(selector));
    start_startup_tui(ui.clone(), settings_manager);
    let value = result.await.unwrap_or(None);
    clear_startup_tui(&ui).await;
    ui.stop(TuiStopOptions::default());
    Ok(value)
}

/// Show the first-time setup dialog and persist the result
pub async fn show_first_time_setup(settings_manager: Arc<SettingsManager>) -> anyhow::Result<()> {
    let ui = create_startup_tui(settings_manager.clone()).await?;
    let (finish, result) = one_shot::<Option<FirstTimeSetupResult>>();

    ui.start();
    let preview_theme = Arc::new(Mutex::new(SYSTEM_THEME_NAME.to_string()));
    set_theme(SYSTEM_THEME_NAME, None);
    let component: ComponentRef = Arc::new(FirstTimeSetupComponent::new(&FirstTimeSetupOptions {
        on_theme_preview: {
            let preview_theme = preview_theme.clone();
            let weak = Arc::downgrade(&ui);
            Arc::new(move |theme_name: String| {
                *preview_theme.lock() = theme_name.clone();
                set_theme(&theme_name, None);
                if let Some(ui) = weak.upgrade() {
                    ui.request_render(false);
                }
            })
        },
        on_submit: {
            let finish = finish.clone();
            Arc::new(move |result| finish(Some(result)))
        },
        on_cancel: {
            let finish = finish.clone();
            Arc::new(move || finish(None))
        },
    }));
    ui.add_child(component.clone());
    ui.set_focus(Some(component));
    ui.request_render(false);
    // The terminal's colors regenerate the system theme; re-rendering rebuilds the dialog with it.
    {
        let preview_theme = preview_theme.clone();
        query_startup_terminal_colors(
            ui.clone(),
            Arc::new(move || {
                let name = preview_theme.lock().clone();
                set_theme(&name, None);
            }),
        );
    }

    if let Some(result) = result.await.unwrap_or(None) {
        settings_manager.set_theme(&result.theme);
        settings_manager.set_enable_analytics(result.share_analytics);
        settings_manager.flush().await?;
    }
    clear_startup_tui(&ui).await;
    ui.stop(TuiStopOptions::default());
    Ok(())
}

pub async fn show_startup_input(
    settings_manager: Arc<SettingsManager>,
    title: &str,
    placeholder: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let ui = create_startup_tui(settings_manager.clone()).await?;
    let (finish, result) = one_shot::<Option<String>>();
    let input = Arc::new(ExtensionInputComponent::new(
        title,
        placeholder,
        {
            let finish = finish.clone();
            Arc::new(move |value: String| finish(Some(value)))
        },
        {
            let finish = finish.clone();
            Arc::new(move || finish(None))
        },
        Some(&ExtensionInputOptions { tui: Some(ui.clone()), timeout: None, initial_value: None, description: None }),
    ));
    let component: ComponentRef = input.clone();
    ui.add_child(component.clone());
    ui.set_focus(Some(component));
    start_startup_tui(ui.clone(), settings_manager);
    let value = result.await.unwrap_or(None);
    input.dispose();
    clear_startup_tui(&ui).await;
    ui.stop(TuiStopOptions::default());
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_official_distribution() {
        assert!(is_official_distribution(&DistributionMetadata {
            package_name: OFFICIAL_PACKAGE_NAME.into(),
            app_name: "pi".into(),
            config_dir_name: ".pi".into(),
        }));
        assert!(!is_official_distribution(&DistributionMetadata {
            package_name: "@fork/pi".into(),
            app_name: "pi".into(),
            config_dir_name: ".pi".into(),
        }));
    }
}

#[cfg(test)]
mod first_time_setup_tests {
    use super::*;

    // first-time-setup.test.ts (env-dependent cases run in one test to avoid races).
    #[test]
    fn should_run_first_time_setup_conditions() {
        let dir = std::env::temp_dir().join(format!("pi-first-time-setup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings_path = dir.join("settings.json");
        let settings_path = settings_path.to_str().unwrap();
        let original_experimental = std::env::var("PI_EXPERIMENTAL").ok();
        let original_agent_dir = std::env::var(ENV_AGENT_DIR).ok();
        // SAFETY: test-only environment mutation, restored below.
        unsafe {
            std::env::set_var("PI_EXPERIMENTAL", "1");
            std::env::remove_var(ENV_AGENT_DIR);
        }
        assert!(should_run_first_time_setup(Some(settings_path)));
        unsafe { std::env::remove_var("PI_EXPERIMENTAL") };
        assert!(!should_run_first_time_setup(Some(settings_path)));
        unsafe {
            std::env::set_var("PI_EXPERIMENTAL", "1");
            std::env::set_var(ENV_AGENT_DIR, dir.to_str().unwrap());
        }
        assert!(!should_run_first_time_setup(Some(settings_path)));
        unsafe { std::env::remove_var(ENV_AGENT_DIR) };
        std::fs::write(settings_path, "{}").unwrap();
        assert!(!should_run_first_time_setup(Some(settings_path)));
        unsafe {
            match original_experimental {
                Some(v) => std::env::set_var("PI_EXPERIMENTAL", v),
                None => std::env::remove_var("PI_EXPERIMENTAL"),
            }
            if let Some(v) = original_agent_dir {
                std::env::set_var(ENV_AGENT_DIR, v);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
