mod find_string;
mod message;
mod named_combobox;
mod request_counter;
mod toggle_switch;

//#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Deref, RangeInclusive};
use std::time::{Duration, Instant, SystemTime};
use std::{
    collections::{HashMap, HashSet},
    ops::DerefMut,
    path::PathBuf,
};

use eframe::egui::{Button, CollapsingHeader, RichText};
use eframe::epaint::{Pos2, Vec2};
use eframe::{
    egui::{FontSelection, Layout, TextFormat, Ui},
    emath::{Align, Align2},
    epaint::{Color32, Stroke, text::LayoutJob},
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use itertools::Itertools as _;
use mint_lib::error::ResultExt as _;
use mint_lib::mod_info::{ModioTags, RequiredStatus};
use mint_lib::update::GitHubRelease;
use strum::{EnumIter, IntoEnumIterator};
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    task::JoinHandle,
};
use tracing::{debug, trace};

use crate::Dirs;
use crate::gui::find_string::searchable_text;
use crate::mod_lints::{LintId, LintReport, SplitAssetPair};
use crate::providers::ProviderError;
use crate::state::SortingConfig;
use crate::{
    MintError,
    integrate::uninstall,
    is_drg_pak,
    providers::{
        ApprovalStatus, FetchProgress, ModInfo, ModSpecification, ModStore, ProviderFactory,
    },
    state::{ModConfig, ModData_v0_2_0 as ModData, ModOrGroup, ModProfile_v0_2_0 as ModProfile, State},
};
use message::MessageHandle;
use request_counter::{RequestCounter, RequestID};

use self::toggle_switch::toggle_switch;

pub fn gui(dirs: Dirs, args: Option<Vec<String>>) -> Result<(), MintError> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 500.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        &format!("mint {}", mint_lib::built_info::version()),
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc, dirs, args)?))),
    )
    .with_generic(|e| format!("{e}"))?;
    Ok(())
}

pub mod colors {
    use eframe::epaint::Color32;

    pub const DARK_RED: Color32 = Color32::DARK_RED;
    pub const DARKER_RED: Color32 = Color32::from_rgb(110, 0, 0);

    pub const DARK_GREEN: Color32 = Color32::DARK_GREEN;
    pub const DARKER_GREEN: Color32 = Color32::from_rgb(0, 80, 0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GuiTheme {
    Light,
    Dark,
}

impl GuiTheme {
    fn from_egui_theme(theme: egui::ThemePreference) -> Option<Self> {
        match theme {
            egui::ThemePreference::Dark => Some(GuiTheme::Dark),
            egui::ThemePreference::Light => Some(GuiTheme::Light),
            egui::ThemePreference::System => None,
        }
    }
    fn into_egui_theme(theme: Option<Self>) -> egui::ThemePreference {
        match theme {
            Some(GuiTheme::Dark) => egui::ThemePreference::Dark,
            Some(GuiTheme::Light) => egui::ThemePreference::Light,
            None => egui::ThemePreference::System,
        }
    }
}

#[derive(PartialEq, Debug, EnumIter, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum SortBy {
    Enabled,
    Name,
    Priority,
    Provider,
    RequiredStatus,
    ApprovalCategory,
}

impl SortBy {
    fn as_str(&self) -> &'static str {
        match self {
            SortBy::Enabled => "Enabled",
            SortBy::Name => "Name",
            SortBy::Priority => "Priority",
            SortBy::Provider => "Provider",
            SortBy::RequiredStatus => "Is Required",
            SortBy::ApprovalCategory => "Approval",
        }
    }
}

const MODIO_LOGO_PNG: &[u8] = include_bytes!("../../assets/modio-cog-blue.png");

pub struct App {
    args: Option<Vec<String>>,
    tx: Sender<message::Message>,
    rx: Receiver<message::Message>,
    state: State,
    resolve_mod: String,
    resolve_mod_rid: Option<MessageHandle<()>>,
    integrate_rid: Option<MessageHandle<HashMap<ModSpecification, SpecFetchProgress>>>,
    update_rid: Option<MessageHandle<()>>,
    check_updates_rid: Option<MessageHandle<()>>,
    has_run_init: bool,
    request_counter: RequestCounter,
    window_provider_parameters: Option<WindowProviderParameters>,
    search_string: String,
    scroll_to_match: bool,
    focus_search: bool,
    settings_window: Option<WindowSettings>,
    modio_texture_handle: Option<egui::TextureHandle>,
    last_action: Option<LastAction>,
    available_update: Option<GitHubRelease>,
    show_update_time: Option<SystemTime>,
    open_profiles: HashSet<String>,
    lint_rid: Option<MessageHandle<()>>,
    lint_report_window: Option<WindowLintReport>,
    lint_report: Option<LintReport>,
    lints_toggle_window: Option<WindowLintsToggle>,
    lint_options: LintOptions,
    cache: CommonMarkCache,
    needs_restart: bool,
    self_update_rid: Option<MessageHandle<SelfUpdateProgress>>,
    original_exe_path: Option<PathBuf>,
    problematic_mod_id: Option<u32>,
    pending_deletion: Option<PendingDeletion>,
    // Folder management
    create_folder_popup: Option<String>, // Some(buffer) when popup is open
    rename_folder_popup: Option<(String, String)>, // Some((old_name, buffer))
    expand_folder: Option<String>, // Folder to expand on next frame
}

#[derive(Default)]
struct LintOptions {
    archive_with_multiple_paks: bool,
    archive_with_only_non_pak_files: bool,
    asset_register_bin: bool,
    conflicting: bool,
    empty_archive: bool,
    outdated_pak_version: bool,
    shader_files: bool,
    non_asset_files: bool,
    split_asset_pairs: bool,
    unmodified_game_assets: bool,
}

struct LastAction {
    timestamp: Instant,
    status: LastActionStatus,
}
impl LastAction {
    fn success(msg: String) -> Self {
        Self {
            timestamp: Instant::now(),
            status: LastActionStatus::Success(msg),
        }
    }
    fn failure(msg: String) -> Self {
        Self {
            timestamp: Instant::now(),
            status: LastActionStatus::Failure(msg),
        }
    }
    fn timeago(&self) -> String {
        let duration = Instant::now().duration_since(self.timestamp);
        let seconds = duration.as_secs();
        if seconds < 60 {
            format!("{seconds}s ago")
        } else if seconds < 3600 {
            format!("{}m ago", seconds / 60)
        } else {
            ">1h ago".into()
        }
    }
}

enum LastActionStatus {
    Success(String),
    Failure(String),
}

impl App {
    fn new(
        _cc: &eframe::CreationContext,
        dirs: Dirs,
        args: Option<Vec<String>>,
    ) -> Result<Self, MintError> {
        let (tx, rx) = mpsc::channel(10);
        let state = State::init(dirs)?;

        Ok(Self {
            args,
            tx,
            rx,
            request_counter: Default::default(),
            state,
            resolve_mod: Default::default(),
            resolve_mod_rid: None,
            integrate_rid: None,
            update_rid: None,
            check_updates_rid: None,
            has_run_init: false,
            window_provider_parameters: None,
            search_string: Default::default(),
            scroll_to_match: false,
            focus_search: false,
            settings_window: None,
            modio_texture_handle: None,
            last_action: None,
            available_update: None,
            show_update_time: None,
            open_profiles: Default::default(),
            lint_rid: None,
            lint_report_window: None,
            lint_report: None,
            lints_toggle_window: None,
            lint_options: LintOptions::default(),
            cache: Default::default(),
            needs_restart: false,
            self_update_rid: None,
            original_exe_path: None,
            problematic_mod_id: None,
            pending_deletion: None,
            create_folder_popup: None,
            rename_folder_popup: None,
            expand_folder: None,
        })
    }

    fn ui_profile(&mut self, ui: &mut Ui, profile: &str) {
        let sorting_config = self.get_sorting_config();

        let mod_data = self.state.mod_data.deref_mut().deref_mut();
        let active_profile_name = mod_data.active_profile.clone();
        
        // Get mutable reference to profiles map
        let profiles = &mut mod_data.profiles;
        
        // Get folder names from the active profile
        let folder_names: Vec<String> = profiles
            .get(&active_profile_name)
            .map(|p| p.groups.keys().cloned().collect())
            .unwrap_or_default();

        struct Ctx {
            needs_save: bool,
            scroll_to_match: bool,
            btn_remove: Option<usize>,
            pending_delete: Option<(String, usize)>, // (mod_name, row_index)
            pending_folder_delete: Option<String>, // folder_name
            pending_folder_mod_delete: Option<(String, usize)>, // (folder_name, mod_index) - delete mod inside folder
            add_deps: Option<Vec<ModSpecification>>,
            // Folder operations
            move_mod_to_folder: Option<(usize, String)>, // (mod_index, folder_name)
            move_mod_from_folder: Option<(String, usize)>, // (folder_name, mod_index_in_folder) -> to root
            move_mod_between_folders: Option<(String, usize, String)>, // (from_folder, mod_index, to_folder)
            rename_folder: Option<String>, // folder name to rename
        }
        let mut ctx = Ctx {
            needs_save: false,
            scroll_to_match: self.scroll_to_match,
            btn_remove: None,
            pending_delete: None,
            pending_folder_delete: None,
            pending_folder_mod_delete: None,
            add_deps: None,
            move_mod_to_folder: None,
            move_mod_from_folder: None,
            move_mod_between_folders: None,
            rename_folder: None,
        };

        let ui_profile = |ui: &mut Ui, profile: &mut ModProfile| {
            let enabled_specs = profile
                .mods
                .iter()
                .enumerate()
                .flat_map(|(i, m)| -> Box<dyn Iterator<Item = _>> {
                    match m {
                        ModOrGroup::Individual(mc) => {
                            Box::new(mc.enabled.then_some((Some(i), mc.spec.clone())).into_iter())
                        }
                        ModOrGroup::Group {
                            group_name,
                            enabled,
                        } => Box::new(
                            enabled
                                .then(|| profile.groups.get(group_name))
                                .flatten()
                                .into_iter()
                                .flat_map(|g| {
                                    g.mods
                                        .iter()
                                        .filter_map(|m| m.enabled.then_some((None, m.spec.clone())))
                                }),
                        ),
                    }
                })
                .collect::<Vec<_>>();

            let ui_mod_tags = |ctx: &mut Ctx, ui: &mut Ui, info: &ModInfo| {
                if let Some(ModioTags {
                    qol,
                    gameplay,
                    audio,
                    visual,
                    framework,
                    required_status,
                    approval_status,
                    versions: _,
                }) = info.modio_tags.as_ref()
                {
                    let mut mk_searchable_modio_tag =
                        |tag_str: &str,
                         ui: &mut Ui,
                         color: Option<egui::Color32>,
                         hover_str: Option<&str>| {
                            let search = searchable_text(tag_str, &self.search_string, {
                                TextFormat {
                                    color: if color.is_some() {
                                        Color32::BLACK
                                    } else {
                                        Color32::GRAY
                                    },

                                    ..Default::default()
                                }
                            });

                            let button = if let Some(color) = color {
                                egui::Button::new(search.job)
                                    .small()
                                    .fill(color)
                                    .stroke(egui::Stroke::NONE)
                            } else {
                                egui::Button::new(search.job)
                                    .small()
                                    .stroke(egui::Stroke::NONE)
                            };

                            let res = if let Some(hover_str) = hover_str {
                                ui.add_enabled(false, button)
                                    .on_disabled_hover_text(hover_str)
                            } else {
                                ui.add_enabled(false, button)
                            };

                            if search.is_match && self.scroll_to_match {
                                res.scroll_to_me(None);
                                ctx.scroll_to_match = false;
                            }
                        };

                    match approval_status {
                        ApprovalStatus::Verified => {
                            mk_searchable_modio_tag(
                                "Verified",
                                ui,
                                Some(egui::Color32::LIGHT_GREEN),
                                Some("Does not contain any gameplay affecting features or changes"),
                            );
                        }
                        ApprovalStatus::Approved => {
                            mk_searchable_modio_tag(
                                "Approved",
                                ui,
                                Some(egui::Color32::LIGHT_BLUE),
                                Some("Contains gameplay affecting features or changes"),
                            );
                        }
                        ApprovalStatus::Sandbox => {
                            mk_searchable_modio_tag(
                                "Sandbox",
                                ui,
                                Some(egui::Color32::LIGHT_YELLOW),
                                Some(
                                    "Contains significant, possibly progression breaking, changes to gameplay",
                                ),
                            );
                        }
                    }

                    match required_status {
                        RequiredStatus::RequiredByAll => {
                            mk_searchable_modio_tag(
                                "RequiredByAll",
                                ui,
                                Some(egui::Color32::LIGHT_RED),
                                Some(
                                    "All lobby members must use this mod for it to work correctly!",
                                ),
                            );
                        }
                        RequiredStatus::Optional => {
                            mk_searchable_modio_tag(
                                "Optional",
                                ui,
                                None,
                                Some("Clients are not required to install this mod to function"),
                            );
                        }
                    }

                    if *qol {
                        mk_searchable_modio_tag("QoL", ui, None, None);
                    }
                    if *gameplay {
                        mk_searchable_modio_tag("Gameplay", ui, None, None);
                    }
                    if *audio {
                        mk_searchable_modio_tag("Audio", ui, None, None);
                    }
                    if *visual {
                        mk_searchable_modio_tag("Visual", ui, None, None);
                    }
                    if *framework {
                        mk_searchable_modio_tag("Framework", ui, None, None);
                    }
                }
            };

            let mut ui_mod = |ctx: &mut Ctx,
                              ui: &mut Ui,
                              in_folder: Option<&str>,
                              row_index: usize,
                              mc: &mut ModConfig,
                              override_priority: Option<i32>| {
                if !mc.enabled {
                    let vis = ui.visuals_mut();
                    vis.override_text_color = Some(vis.text_color());
                    vis.hyperlink_color = vis.text_color();
                }

                if ui
                    .add(toggle_switch(&mut mc.enabled))
                    .on_hover_text_at_pointer("Enabled?")
                    .changed()
                {
                    ctx.needs_save = true;
                }

                // Move to folder dropdown (only for mods at root level)
                if in_folder.is_none() && !folder_names.is_empty() {
                    egui::ComboBox::from_id_salt(format!("move-to-folder-{}", row_index))
                        .selected_text("📁")
                        .width(40.0)
                        .show_ui(ui, |ui| {
                            for folder_name in &folder_names {
                                if ui.selectable_label(false, folder_name).clicked() {
                                    ctx.move_mod_to_folder = Some((row_index, folder_name.clone()));
                                }
                            }
                        })
                        .response
                        .on_hover_text("Move to folder");
                }

                /*
                if ui
                    .add(egui::Checkbox::without_text(&mut mc.required))
                    .changed()
                {
                    needs_save = true;
                }
                */

                let info = self.state.store.get_mod_info(&mc.spec);

                if let Some(ref info) = info
                    && let Some(modio_id) = info.modio_id
                    && self.problematic_mod_id.is_some_and(|id| id == modio_id)
                {
                    let icon = egui::Button::new(RichText::new("❌").color(Color32::WHITE))
                        .fill(Color32::RED);
                    ui.add_enabled(false, icon);
                }

                if mc.enabled
                    && let Some(req) = &self.integrate_rid
                {
                    match req.state.get(&mc.spec) {
                        Some(SpecFetchProgress::Progress { progress, size }) => {
                            ui.add(
                                egui::ProgressBar::new(*progress as f32 / *size as f32)
                                    .show_percentage()
                                    .desired_width(100.0),
                            );
                        }
                        Some(SpecFetchProgress::Complete) => {
                            ui.add(egui::ProgressBar::new(1.0).desired_width(100.0));
                        }
                        None => {
                            ui.spinner();
                        }
                    }
                }

                if let Some(info) = &info {
                    egui::ComboBox::from_id_salt(row_index)
                        .selected_text(
                            self.state
                                .store
                                .get_version_name(&mc.spec)
                                .unwrap_or_default(),
                        )
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut mc.spec.url,
                                info.spec.url.to_string(),
                                self.state
                                    .store
                                    .get_version_name(&info.spec)
                                    .unwrap_or_default(),
                            );
                            for version in info.versions.iter().rev() {
                                ui.selectable_value(
                                    &mut mc.spec.url,
                                    version.url.to_string(),
                                    self.state
                                        .store
                                        .get_version_name(version)
                                        .unwrap_or_default(),
                                );
                            }
                        });

                    ui.scope(|ui| {
                        ui.style_mut().spacing.interact_size.x = 30.;
                        let dark = ui.visuals().dark_mode;
                        
                        // Use override priority if set, otherwise use mod's own priority
                        let display_priority = override_priority.unwrap_or(mc.priority);
                        let is_overridden = override_priority.is_some();
                        
                        match display_priority.cmp(&0) {
                            std::cmp::Ordering::Less => {
                                ui.visuals_mut().override_text_color = Some(if dark {
                                    Color32::LIGHT_RED
                                } else {
                                    Color32::DARK_RED
                                });
                            }
                            std::cmp::Ordering::Greater => {
                                ui.visuals_mut().override_text_color = Some(if dark {
                                    Color32::LIGHT_GREEN
                                } else {
                                    Color32::DARK_GREEN
                                });
                            }
                            _ => {}
                        }
                        
                        if is_overridden {
                            // Show folder's priority (read-only)
                            ui.add_enabled(false,
                                egui::DragValue::new(&mut display_priority.clone())
                                    .custom_formatter(|n, _| {
                                        if n == 0. {
                                            "-".to_string()
                                        } else {
                                            format!("{n}")
                                        }
                                    })
                            )
                            .on_hover_text_at_pointer(
                                "Load Priority (set by folder)\nFolder priority override is enabled.",
                            );
                        } else {
                            if ui.add(
                                egui::DragValue::new(&mut mc.priority)
                                    .custom_formatter(|n, _| {
                                        if n == 0. {
                                            "-".to_string()
                                        } else {
                                            format!("{n}")
                                        }
                                    })
                                    .speed(0.05)
                                    .range(RangeInclusive::new(-999, 999)),
                            )
                            .on_hover_text_at_pointer(
                                "Load Priority\nIn case of asset conflict, mods with higher priority take precedent.\nCan have duplicate values.",
                            ).changed() {
                                ctx.needs_save = true;
                            }
                        }
                    });

                    if ui
                        .button("📋")
                        .on_hover_text_at_pointer("copy URL")
                        .clicked()
                    {
                        ui.ctx().copy_text(mc.spec.url.to_string());
                    }

                    if mc.enabled {
                        let is_duplicate = enabled_specs.iter().any(|(i, spec)| {
                            Some(row_index) != *i && info.spec.satisfies_dependency(spec)
                        });
                        if is_duplicate
                            && ui
                                .button(
                                    egui::RichText::new("\u{26A0}")
                                        .color(ui.visuals().warn_fg_color),
                                )
                                .on_hover_text_at_pointer("remove duplicate")
                                .clicked()
                        {
                            ctx.pending_delete = Some((info.name.clone(), row_index));
                        }

                        let missing_deps = info
                            .suggested_dependencies
                            .iter()
                            .filter(|d| {
                                !enabled_specs.iter().any(|(_, s)| s.satisfies_dependency(d))
                            })
                            .collect::<Vec<_>>();

                        if !missing_deps.is_empty() {
                            let mut msg = "Add missing dependencies:".to_string();
                            for dep in &missing_deps {
                                msg.push('\n');
                                msg.push_str(&dep.url);
                            }
                            if ui
                                .button(
                                    egui::RichText::new("\u{26A0}")
                                        .color(ui.visuals().warn_fg_color),
                                )
                                .on_hover_text(msg)
                                .clicked()
                            {
                                ctx.add_deps = Some(missing_deps.into_iter().cloned().collect());
                            }
                        }
                    }

                    match info.provider {
                        "modio" => {
                            let texture: &egui::TextureHandle =
                                self.modio_texture_handle.get_or_insert_with(|| {
                                    let image = image::load_from_memory(MODIO_LOGO_PNG).unwrap();
                                    let size = [image.width() as _, image.height() as _];
                                    let image_buffer = image.to_rgba8();
                                    let pixels = image_buffer.as_flat_samples();
                                    let image = egui::ColorImage::from_rgba_unmultiplied(
                                        size,
                                        pixels.as_slice(),
                                    );

                                    ui.ctx()
                                        .load_texture("modio-logo", image, Default::default())
                                });
                            let mut img =
                                egui::Image::new(texture).fit_to_exact_size([16.0, 16.0].into());
                            if !mc.enabled {
                                img = img.tint(Color32::LIGHT_RED);
                            }
                            ui.add(img);
                        }
                        "http" => {
                            ui.label("🌐");
                        }
                        "file" => {
                            ui.label("📁");
                        }
                        _ => unimplemented!("unimplemented provider kind"),
                    }

                    let search = searchable_text(&info.name, &self.search_string, {
                        TextFormat {
                            color: ui.visuals().hyperlink_color,
                            ..Default::default()
                        }
                    });

                    let res = ui.hyperlink_to(search.job, &mc.spec.url);
                    if search.is_match && self.scroll_to_match {
                        res.scroll_to_me(None);
                        ctx.scroll_to_match = false;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui_mod_tags(ctx, ui, info);
                    });
                } else {
                    if ui
                        .button("📋")
                        .on_hover_text_at_pointer("Copy URL")
                        .clicked()
                    {
                        ui.ctx().copy_text(mc.spec.url.to_string());
                    }

                    let search = searchable_text(&mc.spec.url, &self.search_string, {
                        TextFormat {
                            color: ui.visuals().hyperlink_color,
                            ..Default::default()
                        }
                    });

                    let res = ui.hyperlink_to(search.job, &mc.spec.url);
                    if search.is_match && self.scroll_to_match {
                        res.scroll_to_me(None);
                        ctx.scroll_to_match = false;
                    }
                }
            };

            let mut ui_item =
                |ctx: &mut Ctx, ui: &mut Ui, mc: &mut ModOrGroup, row_index: usize| {
                    ui.scope(|ui| {
                        ui.visuals_mut().widgets.hovered.weak_bg_fill = colors::DARK_RED;
                        ui.visuals_mut().widgets.active.weak_bg_fill = colors::DARKER_RED;
                        if ui
                            .add(Button::new(" 🗑 "))
                            .on_hover_text_at_pointer(match mc {
                                ModOrGroup::Individual(_) => "Delete mod",
                                ModOrGroup::Group { .. } => "Delete folder (mods move to root)",
                            })
                            .clicked()
                        {
                            match mc {
                                ModOrGroup::Individual(mod_config) => {
                                    ctx.pending_delete = Some((mod_config.spec.url.clone(), row_index));
                                }
                                ModOrGroup::Group { group_name, .. } => {
                                    ctx.pending_folder_delete = Some(group_name.clone());
                                }
                            }
                        };
                    });

                    match mc {
                        ModOrGroup::Individual(mc) => {
                            ui_mod(ctx, ui, None, row_index, mc, None);
                        }
                        ModOrGroup::Group {
                            group_name,
                            enabled,
                        } => {
                            if ui
                                .add(toggle_switch(enabled))
                                .on_hover_text_at_pointer("Enabled?")
                                .changed()
                            {
                                ctx.needs_save = true;
                            }
                            
                            // Rename button for folder
                            if ui.button("✏").on_hover_text("Rename folder").clicked() {
                                ctx.rename_folder = Some(group_name.clone());
                            }
                            
                            let group_name_clone = group_name.clone();
                            let folder_id = ui.make_persistent_id(format!("folder-{}", group_name));
                            
                            // Check if this folder should be opened (e.g., after moving a mod into it)
                            let should_open = self.expand_folder.as_ref() == Some(group_name);
                            
                            // Use open() to force-open when a mod was just moved in
                            let mut header = egui::CollapsingHeader::new(group_name.as_str())
                                .id_salt(folder_id)
                                .default_open(false);
                            
                            if should_open {
                                header = header.open(Some(true));
                            }
                            
                            header.show(ui, |ui| {
                                    if let Some(group) = profile.groups.get_mut(&group_name_clone) {
                                        // Folder priority override controls
                                        ui.horizontal(|ui| {
                                            let has_override = group.priority_override.is_some();
                                            let mut override_enabled = has_override;
                                            
                                            if ui.checkbox(&mut override_enabled, "Priority override:")
                                                .on_hover_text("When enabled, all mods in this folder use the folder's priority")
                                                .changed()
                                            {
                                                if override_enabled {
                                                    group.priority_override = Some(0);
                                                } else {
                                                    group.priority_override = None;
                                                }
                                                ctx.needs_save = true;
                                            }
                                            
                                            if let Some(ref mut priority) = group.priority_override {
                                                if ui.add(egui::DragValue::new(priority)).changed() {
                                                    ctx.needs_save = true;
                                                }
                                            }
                                        });
                                        
                                        ui.separator();
                                        
                                        let override_priority = group.priority_override;
                                        let mut move_out_index: Option<usize> = None;
                                        let mut move_to_other_folder: Option<(usize, String)> = None;
                                        let mut delete_mod_index: Option<usize> = None;
                                        
                                        for (index, m) in group.mods.iter_mut().enumerate() {
                                            ui.horizontal(|ui| {
                                                // Delete button (red styling)
                                                ui.scope(|ui| {
                                                    ui.visuals_mut().widgets.hovered.weak_bg_fill = colors::DARK_RED;
                                                    ui.visuals_mut().widgets.active.weak_bg_fill = colors::DARKER_RED;
                                                    if ui.button(" 🗑 ").on_hover_text("Delete mod").clicked() {
                                                        delete_mod_index = Some(index);
                                                    }
                                                });
                                                
                                                // Move dropdown - shows root + other folders
                                                egui::ComboBox::from_id_salt(format!("move-in-folder-{}-{}", group_name_clone, index))
                                                    .selected_text("📁")
                                                    .width(40.0)
                                                    .show_ui(ui, |ui| {
                                                        // Option to move to root
                                                        if ui.selectable_label(false, "📤 (root)").clicked() {
                                                            move_out_index = Some(index);
                                                        }
                                                        ui.separator();
                                                        // Options for other folders
                                                        for other_folder in &folder_names {
                                                            if other_folder != &group_name_clone {
                                                                if ui.selectable_label(false, format!("📁 {}", other_folder)).clicked() {
                                                                    move_to_other_folder = Some((index, other_folder.clone()));
                                                                }
                                                            }
                                                        }
                                                    })
                                                    .response
                                                    .on_hover_text("Move to...");
                                                
                                                ui_mod(ctx, ui, Some(&group_name_clone), index, m, override_priority);
                                            });
                                        }
                                        if let Some(idx) = move_out_index {
                                            ctx.move_mod_from_folder = Some((group_name_clone.clone(), idx));
                                        }
                                        if let Some((idx, target_folder)) = move_to_other_folder {
                                            ctx.move_mod_between_folders = Some((group_name_clone.clone(), idx, target_folder));
                                        }
                                        if let Some(idx) = delete_mod_index {
                                            // Get mod name for confirmation
                                            if let Some(m) = group.mods.get(idx) {
                                                ctx.pending_folder_mod_delete = Some((group_name_clone.clone(), idx));
                                            }
                                        }
                                    }
                                });
                        }
                    }
                };

            if let Some(sorting_config) = sorting_config {
                let comp = sort_mods(sorting_config);
                profile
                    .mods
                    .iter_mut()
                    .map(|m| {
                        // fetch ModInfo up front because doing it in the comparator is slow
                        let ModOrGroup::Individual(mc) = m else {
                            unimplemented!("Item is not Individual \n{:?}", m);
                        };
                        let info = self.state.store.get_mod_info(&mc.spec);
                        (m, info)
                    })
                    .enumerate()
                    .sorted_by(|a, b| comp((a.1.0, a.1.1.as_ref()), (b.1.0, b.1.1.as_ref())))
                    .enumerate()
                    .for_each(|(visual_index, (store_index, item))| {
                        let mut frame = egui::Frame::NONE;
                        if visual_index % 2 == 1 {
                            frame.fill = ui.visuals().faint_bg_color
                        }
                        frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui_item(&mut ctx, ui, item.0, store_index);
                            });
                        });
                    });
            } else {
                let res = egui_dnd::dnd(ui, ui.id())
                    .with_mouse_config(egui_dnd::DragDropConfig::mouse())
                    .show(
                        profile.mods.iter_mut().enumerate(),
                        |ui, (_index, item), handle, state| {
                            let mut frame = egui::Frame::NONE;
                            if state.dragged {
                                frame.fill = ui.visuals().extreme_bg_color
                            } else if state.index % 2 == 1 {
                                frame.fill = ui.visuals().faint_bg_color
                            }
                            frame.show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    handle.ui(ui, |ui| {
                                        ui.label("   ☰  ");
                                    });

                                    ui_item(&mut ctx, ui, item, state.index);
                                });
                            });
                        },
                    );

                if res.final_update().is_some() {
                    res.update_vec(&mut profile.mods);
                    ctx.needs_save = true;
                }
            }
            if let Some(remove) = ctx.btn_remove {
                profile.mods.remove(remove);
                ctx.needs_save = true;
            }
        };

        egui::ScrollArea::vertical().show(ui, |ui| {
            if let Some(profile) = profiles.get_mut(profile) {
                ui_profile(ui, profile);
            } else {
                ui.label("no such profile");
            }
        });

        // Transfer pending deletion to App for confirmation dialog
        if let Some((mod_name, row_index)) = ctx.pending_delete {
            self.pending_deletion = Some(PendingDeletion::Mod {
                mod_name,
                row_index,
            });
        }

        // Handle folder rename request
        if let Some(folder_name) = ctx.rename_folder {
            self.rename_folder_popup = Some((folder_name.clone(), folder_name));
        }

        // Handle move mod to folder
        let mut did_move_to_folder = false;
        if let Some((mod_index, folder_name)) = ctx.move_mod_to_folder {
            let active_profile = self.state.mod_data.active_profile.clone();
            if let Some(profile) = self.state.mod_data.profiles.get_mut(&active_profile) {
                // First verify the target folder exists
                let folder_exists = profile.groups.contains_key(&folder_name);
                if folder_exists {
                    if let Some(ModOrGroup::Individual(mod_config)) = profile.mods.get(mod_index).cloned() {
                        // Remove from root
                        profile.mods.remove(mod_index);
                        // Add to folder (we know it exists)
                        if let Some(group) = profile.groups.get_mut(&folder_name) {
                            group.mods.push(mod_config);
                        }
                        // Expand the folder so user can see where mod went
                        self.expand_folder = Some(folder_name);
                        did_move_to_folder = true;
                        ctx.needs_save = true;
                    }
                }
            }
        }
        
        // Clear expand_folder after it's been used (it was set last frame, used this frame)
        if self.expand_folder.is_some() && !did_move_to_folder {
            self.expand_folder = None;
        }

        // Handle move mod out of folder
        if let Some((folder_name, mod_index)) = ctx.move_mod_from_folder {
            let active_profile = self.state.mod_data.active_profile.clone();
            if let Some(profile) = self.state.mod_data.profiles.get_mut(&active_profile) {
                if let Some(group) = profile.groups.get_mut(&folder_name) {
                    if mod_index < group.mods.len() {
                        let mod_config = group.mods.remove(mod_index);
                        // Add to root of profile
                        profile.mods.push(ModOrGroup::Individual(mod_config));
                        ctx.needs_save = true;
                    }
                }
            }
        }

        // Handle move mod between folders
        if let Some((from_folder, mod_index, to_folder)) = ctx.move_mod_between_folders {
            let active_profile = self.state.mod_data.active_profile.clone();
            if let Some(profile) = self.state.mod_data.profiles.get_mut(&active_profile) {
                // Verify both folders exist
                let from_exists = profile.groups.contains_key(&from_folder);
                let to_exists = profile.groups.contains_key(&to_folder);
                
                if from_exists && to_exists {
                    // Remove from source folder
                    let mod_config = profile.groups.get_mut(&from_folder)
                        .and_then(|g| if mod_index < g.mods.len() { Some(g.mods.remove(mod_index)) } else { None });
                    
                    // Add to target folder
                    if let Some(mod_config) = mod_config {
                        if let Some(to_group) = profile.groups.get_mut(&to_folder) {
                            to_group.mods.push(mod_config);
                        }
                        // Expand target folder
                        self.expand_folder = Some(to_folder);
                        ctx.needs_save = true;
                    }
                }
            }
        }

        // Handle folder deletion request
        if let Some(folder_name) = ctx.pending_folder_delete {
            self.pending_deletion = Some(PendingDeletion::Folder { folder_name });
        }

        // Handle mod deletion inside folder
        if let Some((folder_name, mod_index)) = ctx.pending_folder_mod_delete {
            let active_profile = self.state.mod_data.active_profile.clone();
            if let Some(profile) = self.state.mod_data.profiles.get(&active_profile) {
                if let Some(group) = profile.groups.get(&folder_name) {
                    if let Some(m) = group.mods.get(mod_index) {
                        self.pending_deletion = Some(PendingDeletion::FolderMod { 
                            folder_name, 
                            mod_index,
                            mod_name: m.spec.url.clone(),
                        });
                    }
                }
            }
        }

        if let Some(add_deps) = ctx.add_deps {
            message::ResolveMods::send(self, ui.ctx(), add_deps, true);
            self.problematic_mod_id = None;
        }

        self.scroll_to_match = ctx.scroll_to_match;

        if ctx.needs_save {
            self.state.mod_data.save().unwrap();
        }
    }

    fn parse_mods(&self) -> Vec<ModSpecification> {
        self.resolve_mod
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| ModSpecification::new(l.to_string()))
            .collect()
    }

    fn build_mod_string(mods: &Vec<ModConfig>) -> String {
        let mut string = String::new();
        for m in mods {
            if m.enabled {
                string.push_str(&m.spec.url);
                string.push('\n');
            }
        }
        string
    }

    fn create_backup(dirs: &Dirs, backup_base_path: &str) -> Result<String, String> {
        use std::fs;
        use chrono::Local;

        // Create timestamp for backup folder name
        let timestamp = Local::now().format("%Y-%m-%d-%H-%M-%S").to_string();
        let backup_folder_name = format!("backup_{}", timestamp);
        let backup_path = PathBuf::from(backup_base_path).join(&backup_folder_name);

        // Create backup directory
        fs::create_dir_all(&backup_path)
            .map_err(|e| format!("Failed to create backup directory: {}", e))?;

        // Copy config directory contents
        let config_dir = &dirs.config_dir;
        if config_dir.exists() {
            Self::copy_dir_contents(config_dir, &backup_path.join("config"))
                .map_err(|e| format!("Failed to backup config: {}", e))?;
        }

        // Copy data directory contents
        let data_dir = &dirs.data_dir;
        if data_dir.exists() {
            Self::copy_dir_contents(data_dir, &backup_path.join("data"))
                .map_err(|e| format!("Failed to backup data: {}", e))?;
        }

        Ok(backup_path.to_string_lossy().to_string())
    }

    fn copy_dir_contents(src: &PathBuf, dst: &PathBuf) -> std::io::Result<()> {
        use std::fs;

        fs::create_dir_all(dst)?;

        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let path = entry.path();
            let dest_path = dst.join(entry.file_name());

            if path.is_dir() {
                Self::copy_dir_contents(&path, &dest_path)?;
            } else {
                fs::copy(&path, &dest_path)?;
            }
        }

        Ok(())
    }

    fn show_update_window(&mut self, ctx: &egui::Context) {
        if let (Some(update), Some(update_time)) =
            (self.available_update.as_ref(), self.show_update_time)
        {
            let now = SystemTime::now();
            let wait_time = Duration::from_secs(10);
            egui::Area::new("available-update-overlay".into())
                .movable(false)
                .fixed_pos(Pos2::ZERO)
                .order(egui::Order::Background)
                .show(ctx, |ui| {
                    egui::Frame::NONE
                        .fill(Color32::from_rgba_unmultiplied(0, 0, 0, 127))
                        .show(ui, |ui| {
                            ui.allocate_space(ui.available_size());
                        })
                });
            if let Some(MessageHandle { state, .. }) = &self.self_update_rid {
                egui::Window::new("Update progress")
                    .collapsible(false)
                    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                    .resizable(false)
                    .show(ctx, |ui| {
                        ui.with_layout(egui::Layout::top_down_justified(Align::Center), |ui| {
                            match state {
                                SelfUpdateProgress::Pending => {
                                    ui.add(egui::ProgressBar::new(0.0).show_percentage());
                                }
                                SelfUpdateProgress::Progress { progress, size } => {
                                    ui.add(
                                        egui::ProgressBar::new(*progress as f32 / *size as f32)
                                            .show_percentage(),
                                    );
                                }
                                SelfUpdateProgress::Complete => {
                                    ui.add(egui::ProgressBar::new(1.0).show_percentage());
                                    ui.label(
                                        egui::RichText::new("Update successful.")
                                            .color(Color32::LIGHT_GREEN),
                                    );

                                    if ui.button("Restart").clicked() {
                                        self.needs_restart = true;
                                    }
                                }
                            };
                        });
                    });
            } else {
                egui::Window::new(format!("Update available: {}", update.tag_name))
                    .collapsible(false)
                    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                    .resizable(false)
                    .show(ctx, |ui| {
                        CommonMarkViewer::new().max_image_width(Some(512)).show(
                            ui,
                            &mut self.cache,
                            &update.body,
                        );
                        ui.with_layout(egui::Layout::right_to_left(Align::TOP), |ui| {
                            if ui
                                .add(egui::Button::new("Install update"))
                                .on_hover_text("Download and install the update.")
                                .clicked()
                            {
                                self.self_update_rid = Some(message::SelfUpdate::send(
                                    &mut self.request_counter,
                                    self.tx.clone(),
                                    ctx.clone(),
                                ));
                            }

                            let elapsed = now.duration_since(update_time).unwrap_or_default();
                            if elapsed > wait_time {
                                if ui.button("Close").clicked() {
                                    self.show_update_time = None;
                                }
                            } else {
                                ui.spinner();
                            }
                        });
                    });
            }
        }
    }

    fn show_provider_parameters(&mut self, ctx: &egui::Context) {
        let Some(window) = &mut self.window_provider_parameters else {
            return;
        };

        while let Ok((rid, res)) = window.rx.try_recv() {
            if window.check_rid.as_ref().is_some_and(|r| rid == r.0) {
                match res {
                    Ok(()) => {
                        let window = self.window_provider_parameters.take().unwrap();
                        self.state
                            .config
                            .provider_parameters
                            .insert(window.factory.id.to_string(), window.parameters);
                        self.state.config.save().unwrap();
                        return;
                    }
                    Err(e) => {
                        window.check_error = Some(e.to_string());
                    }
                }
                window.check_rid = None;
            }
        }

        let mut open = true;
        let mut check = false;
        egui::Window::new(format!("Configure {} provider", window.factory.id))
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_enabled_ui(window.check_rid.is_none(), |ui| {
                    egui::Grid::new("grid").num_columns(2).show(ui, |ui| {
                        for p in window.factory.parameters {
                            if let Some(link) = p.link {
                                ui.hyperlink_to(p.name, link).on_hover_text(p.description);
                            } else {
                                ui.label(p.name).on_hover_text(p.description);
                            }
                            let res = ui.add(
                                egui::TextEdit::singleline(
                                    window.parameters.entry(p.id.to_string()).or_default(),
                                )
                                .password(true)
                                .desired_width(200.0),
                            );
                            if is_committed(&res) {
                                check = true;
                            }
                            ui.end_row();
                        }
                    });

                    ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                        if ui.button("Save").clicked() {
                            check = true;
                        }
                        if window.check_rid.is_some() {
                            ui.spinner();
                        }
                        if let Some(error) = &window.check_error {
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        }
                    });
                });
            });
        if !open {
            self.window_provider_parameters = None;
        } else if check {
            window.check_error = None;
            let tx = window.tx.clone();
            let ctx = ctx.clone();
            let rid = self.request_counter.next();
            let store = self.state.store.clone();
            let params = window.parameters.clone();
            let factory = window.factory;
            let handle = tokio::task::spawn(async move {
                let res = store.add_provider_checked(factory, &params).await;
                tx.send((rid, res)).await.unwrap();
                ctx.request_repaint();
            });
            window.check_rid = Some((rid, handle));
        }
    }

    fn show_profile_windows(&mut self, ctx: &egui::Context) {
        let mut to_remove = vec![];
        for profile in &self.open_profiles.clone() {
            let mut open = true;
            egui::Window::new(format!("Profile \"{profile}\""))
                .open(&mut open)
                .show(ctx, |ui| {
                    self.ui_profile(ui, profile);
                });
            if !open {
                to_remove.push(profile.clone());
            }
        }
        for r in to_remove {
            self.open_profiles.remove(&r);
        }
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        if let Some(window) = &mut self.settings_window {
            let mut open = true;
            let mut try_save = false;
            egui::Window::new("Settings")
                .open(&mut open)
                .resizable(false)
                .show(ctx, |ui| {
                    egui::Grid::new("grid").num_columns(2).striped(true).show(ui, |ui| {
                        let mut job = LayoutJob::default();
                        job.append(
                            "DRG pak",
                            0.0,
                            TextFormat {
                                color: ui.visuals().text_color(),
                                underline: Stroke::new(1.0, ui.visuals().text_color()),
                                ..Default::default()
                            },
                        );
                        ui.label(job).on_hover_cursor(egui::CursorIcon::Help).on_hover_text("Path to FSD-WindowsNoEditor.pak (FSD-WinGDK.pak for Microsoft Store version)\nLocated inside the \"Deep Rock Galactic\" installation directory under FSD/Content/Paks.");
                        ui.horizontal(|ui| {
                            let res = ui.add(
                                egui::TextEdit::singleline(
                                    &mut window.drg_pak_path
                                )
                                .desired_width(200.0),
                            );
                            if res.changed() {
                                window.drg_pak_path_err = None;
                            }
                            if is_committed(&res) {
                                try_save = true;
                            }
                            if ui.button("browse").clicked()
                                && let Some(fsd_pak) = rfd::FileDialog::new()
                                    .add_filter("DRG Pak", &["pak"])
                                    .pick_file()
                                {
                                    window.drg_pak_path = fsd_pak.to_string_lossy().to_string();
                                    window.drg_pak_path_err = None;
                                }
                        });
                        ui.end_row();

                        let config_dir = &self.state.dirs.config_dir;
                        ui.label("Config directory:");
                        if ui.link(config_dir.display().to_string()).clicked() {
                            opener::open(config_dir).ok();
                        }
                        ui.end_row();

                        let cache_dir = &self.state.dirs.cache_dir;
                        ui.label("Cache directory:");
                        if ui.link(cache_dir.display().to_string()).clicked() {
                            opener::open(cache_dir).ok();
                        }
                        ui.end_row();

                        let data_dir = &self.state.dirs.data_dir;
                        ui.label("Data directory:");
                        if ui.link(data_dir.display().to_string()).clicked() {
                            opener::open(data_dir).ok();
                        }
                        ui.end_row();

                        ui.label("GUI theme:");
                        ui.horizontal(|ui| {
                            ui.horizontal(|ui| {
                                let config = &mut self.state.config;

                                let old_theme = GuiTheme::into_egui_theme(config.gui_theme);
                                let mut theme = old_theme;
                                theme.radio_buttons(ui);
                                if theme != old_theme {
                                    ui.memory_mut(|m| m.options.theme_preference = theme);
                                    config.gui_theme = GuiTheme::from_egui_theme(theme);
                                    config.save().unwrap();
                                }
                            });
                        });
                        ui.end_row();

                        ui.label("Confirm mod deletion:");
                        if ui.checkbox(&mut self.state.config.confirm_mod_deletion, "")
                            .on_hover_text("Show confirmation dialog before deleting mods")
                            .changed()
                        {
                            self.state.config.save().unwrap();
                        }
                        ui.end_row();

                        ui.label("Confirm profile deletion:");
                        if ui.checkbox(&mut self.state.config.confirm_profile_deletion, "")
                            .on_hover_text("Show confirmation dialog before deleting profiles")
                            .changed()
                        {
                            self.state.config.save().unwrap();
                        }
                        ui.end_row();

                        ui.label("Backup path:");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut window.backup_path)
                                    .desired_width(200.0),
                            );
                            if ui.button("browse").clicked() {
                                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                    window.backup_path = folder.to_string_lossy().to_string();
                                }
                            }
                            if ui.button("Save path").clicked() {
                                self.state.config.backup_path = Some(PathBuf::from(&window.backup_path));
                                self.state.config.save().unwrap();
                            }
                        });
                        ui.end_row();

                        ui.label("");
                        ui.horizontal(|ui| {
                            if ui.button("Create Backup Now").clicked() {
                                let backup_result = Self::create_backup(
                                    &self.state.dirs,
                                    &window.backup_path,
                                );
                                window.backup_status = Some(match backup_result {
                                    Ok(path) => (true, format!("Backup created: {}", path)),
                                    Err(e) => (false, format!("Backup failed: {}", e)),
                                });
                            }
                            if let Some((success, msg)) = &window.backup_status {
                                if *success {
                                    ui.colored_label(Color32::LIGHT_GREEN, msg);
                                } else {
                                    ui.colored_label(ui.visuals().error_fg_color, msg);
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Mod providers:");
                        ui.end_row();

                        for provider_factory in ModStore::get_provider_factories() {
                            ui.label(provider_factory.id);
                            if ui.add_enabled(!provider_factory.parameters.is_empty(), egui::Button::new("⚙"))
                                    .on_hover_text(format!("Open \"{}\" settings", provider_factory.id))
                                    .clicked() {
                                self.window_provider_parameters = Some(
                                    WindowProviderParameters::new(provider_factory, &self.state),
                                );
                            }
                            ui.end_row();
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(Align::TOP), |ui| {
                        if ui.add_enabled(window.drg_pak_path_err.is_none(), egui::Button::new("save")).clicked() {
                            try_save = true;
                        }
                        if let Some(error) = &window.drg_pak_path_err {
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        }
                    });

                });
            if try_save {
                if let Err(e) = is_drg_pak(&window.drg_pak_path) {
                    window.drg_pak_path_err = Some(e.to_string());
                } else {
                    self.state.config.drg_pak_path = Some(PathBuf::from(
                        self.settings_window.take().unwrap().drg_pak_path,
                    ));
                    self.state.config.save().unwrap();
                }
            } else if !open {
                self.settings_window = None;
            }
        }
    }

    fn show_delete_confirmation(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_deletion else {
            return;
        };

        // Check if confirmation is enabled for this type
        let confirmation_enabled = match pending {
            PendingDeletion::Mod { .. } => self.state.config.confirm_mod_deletion,
            PendingDeletion::Profile { .. } => self.state.config.confirm_profile_deletion,
            PendingDeletion::Folder { .. } => self.state.config.confirm_mod_deletion,
            PendingDeletion::FolderMod { .. } => self.state.config.confirm_mod_deletion,
        };

        // If confirmation is disabled, perform deletion immediately
        if !confirmation_enabled {
            self.perform_pending_deletion();
            return;
        }

        // Extract info based on deletion type
        let (item_type, item_name) = match pending {
            PendingDeletion::Mod { mod_name, .. } => ("mod", mod_name.clone()),
            PendingDeletion::Profile { profile_name } => ("profile", profile_name.clone()),
            PendingDeletion::Folder { folder_name } => ("folder", folder_name.clone()),
            PendingDeletion::FolderMod { mod_name, .. } => ("mod", mod_name.clone()),
        };

        let mut confirmed = false;
        let mut cancelled = false;

        egui::Window::new("Confirm Deletion")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label(format!("Are you sure you want to delete this {item_type}?"));
                    ui.add_space(8.0);

                    // Show the item name in a highlighted box
                    egui::Frame::NONE
                        .fill(ui.visuals().extreme_bg_color)
                        .inner_margin(8.0)
                        .corner_radius(4.0)
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(&item_name).strong());
                        });

                    ui.add_space(16.0);

                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                        ui.add_space(16.0);
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new("Delete").color(egui::Color32::WHITE),
                            ).fill(egui::Color32::DARK_RED))
                            .clicked()
                        {
                            confirmed = true;
                        }
                    });
                    ui.add_space(8.0);
                });
            });

        if cancelled {
            self.pending_deletion = None;
        } else if confirmed {
            self.perform_pending_deletion();
        }
    }

    fn perform_pending_deletion(&mut self) {
        match &self.pending_deletion {
            Some(PendingDeletion::Mod { row_index, .. }) => {
                let row_index = *row_index;
                let active_profile = self.state.mod_data.active_profile.clone();
                if let Some(profile) = self.state.mod_data.profiles.get_mut(&active_profile) {
                    profile.mods.remove(row_index);
                    self.state.mod_data.save().unwrap();
                }
            }
            Some(PendingDeletion::Profile { profile_name }) => {
                let profile_name = profile_name.clone();
                self.state.mod_data.profiles.remove(&profile_name);
                // Select a different profile if we deleted the active one
                if self.state.mod_data.active_profile == profile_name {
                    if let Some(first_profile) = self.state.mod_data.profiles.keys().next() {
                        self.state.mod_data.active_profile = first_profile.clone();
                    }
                }
                
                self.state.mod_data.save().unwrap();
            }
            Some(PendingDeletion::Folder { folder_name }) => {
                let folder_name = folder_name.clone();
                let active_profile = self.state.mod_data.active_profile.clone();
                
                if let Some(profile) = self.state.mod_data.profiles.get_mut(&active_profile) {
                    // Move all mods from folder back to root
                    if let Some(group) = profile.groups.remove(&folder_name) {
                        for mod_config in group.mods {
                            profile.mods.push(ModOrGroup::Individual(mod_config));
                        }
                    }
                    // Remove the group reference from profile's mods list
                    profile.mods.retain(|item| {
                        !matches!(item, ModOrGroup::Group { group_name, .. } if group_name == &folder_name)
                    });
                }
                
                self.state.mod_data.save().unwrap();
            }
            Some(PendingDeletion::FolderMod { folder_name, mod_index, .. }) => {
                let folder_name = folder_name.clone();
                let mod_index = *mod_index;
                let active_profile = self.state.mod_data.active_profile.clone();
                
                if let Some(profile) = self.state.mod_data.profiles.get_mut(&active_profile) {
                    if let Some(group) = profile.groups.get_mut(&folder_name) {
                        if mod_index < group.mods.len() {
                            group.mods.remove(mod_index);
                        }
                    }
                }
                
                self.state.mod_data.save().unwrap();
            }
            None => {}
        }
        self.pending_deletion = None;
    }

    fn show_create_folder_popup(&mut self, ctx: &egui::Context) {
        if self.create_folder_popup.is_none() {
            return;
        }

        let mut should_close = false;
        let mut should_create = false;

        // Get active profile for checking existing folders
        let active_profile = self.state.mod_data.active_profile.clone();

        egui::Window::new("Create Folder")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label("Enter folder name:");
                    ui.add_space(8.0);

                    let buffer = self.create_folder_popup.as_mut().unwrap();
                    let response = ui.text_edit_singleline(buffer);
                    
                    // Auto-focus the text field
                    if response.gained_focus() || buffer.is_empty() {
                        response.request_focus();
                    }

                    // Check if name already exists in active profile
                    let name_exists = self.state.mod_data.profiles
                        .get(&active_profile)
                        .map(|p| p.groups.contains_key(buffer.as_str()))
                        .unwrap_or(false);
                    let name_valid = !buffer.trim().is_empty() && !name_exists;

                    if name_exists && !buffer.is_empty() {
                        ui.colored_label(ui.visuals().error_fg_color, "Folder name already exists");
                    }

                    ui.add_space(16.0);

                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            should_close = true;
                        }
                        ui.add_space(16.0);
                        if ui.add_enabled(name_valid, egui::Button::new("Create")).clicked() 
                            || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && name_valid)
                        {
                            should_create = true;
                        }
                    });
                    ui.add_space(8.0);
                });
            });

        if should_close {
            self.create_folder_popup = None;
        } else if should_create {
            let folder_name = self.create_folder_popup.take().unwrap().trim().to_string();
            // Add group to active profile
            if let Some(profile) = self.state.mod_data.profiles.get_mut(&active_profile) {
                // Create the group in profile's groups map
                profile.groups.insert(folder_name.clone(), crate::state::ModGroup { 
                    mods: vec![],
                    priority_override: None,
                });
                // Add group reference to profile's mods list
                profile.mods.push(ModOrGroup::Group { group_name: folder_name, enabled: true });
            }
            self.state.mod_data.save().unwrap();
        }
    }

    fn show_rename_folder_popup(&mut self, ctx: &egui::Context) {
        if self.rename_folder_popup.is_none() {
            return;
        }

        let mut should_close = false;
        let mut should_rename = false;
        
        // Get active profile for checking existing folders
        let active_profile = self.state.mod_data.active_profile.clone();

        egui::Window::new("Rename Folder")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label("Enter new folder name:");
                    ui.add_space(8.0);

                    let (old_name, buffer) = self.rename_folder_popup.as_mut().unwrap();
                    let response = ui.text_edit_singleline(buffer);

                    // Check if name already exists (and is different from current)
                    let name_exists = buffer != old_name && self.state.mod_data.profiles
                        .get(&active_profile)
                        .map(|p| p.groups.contains_key(buffer.as_str()))
                        .unwrap_or(false);
                    let name_valid = !buffer.trim().is_empty() && !name_exists;

                    if name_exists {
                        ui.colored_label(ui.visuals().error_fg_color, "Folder name already exists");
                    }

                    ui.add_space(16.0);

                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            should_close = true;
                        }
                        ui.add_space(16.0);
                        if ui.add_enabled(name_valid, egui::Button::new("Rename")).clicked()
                            || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && name_valid)
                        {
                            should_rename = true;
                        }
                    });
                    ui.add_space(8.0);
                });
            });

        if should_close {
            self.rename_folder_popup = None;
        } else if should_rename {
            let (old_name, new_name) = self.rename_folder_popup.take().unwrap();
            let new_name = new_name.trim().to_string();
            
            // Rename in active profile only
            if let Some(profile) = self.state.mod_data.profiles.get_mut(&active_profile) {
                // Move the group data to new key
                if let Some(group) = profile.groups.remove(&old_name) {
                    profile.groups.insert(new_name.clone(), group);
                }
                
                // Update references in mods list
                for item in &mut profile.mods {
                    if let ModOrGroup::Group { group_name, .. } = item {
                        if group_name == &old_name {
                            *group_name = new_name.clone();
                        }
                    }
                }
            }
            
            self.state.mod_data.save().unwrap();
        }
    }

    fn show_lints_toggle(&mut self, ctx: &egui::Context) {
        if let Some(_lints_toggle) = &self.lints_toggle_window {
            let mut open = true;

            egui::Window::new("Toggle lints")
                .open(&mut open)
                .resizable(false)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        egui::Grid::new("lints-toggle-grid").show(ui, |ui| {
                            ui.heading("Lint");
                            ui.heading("Enabled?");
                            ui.end_row();

                            ui.label("Archive with multiple paks");
                            ui.add(toggle_switch(
                                &mut self.lint_options.archive_with_multiple_paks,
                            ));
                            ui.end_row();

                            ui.label("Archive with only non-pak files");
                            ui.add(toggle_switch(
                                &mut self.lint_options.archive_with_only_non_pak_files,
                            ));
                            ui.end_row();

                            ui.label("Mods containing AssetRegister.bin");
                            ui.add(toggle_switch(&mut self.lint_options.asset_register_bin));
                            ui.end_row();

                            ui.label("Mods containing conflicting files");
                            ui.add(toggle_switch(&mut self.lint_options.conflicting));
                            ui.end_row();

                            ui.label("Mods containing empty archives");
                            ui.add(toggle_switch(&mut self.lint_options.empty_archive));
                            ui.end_row();

                            ui.label("Mods containing oudated pak version");
                            ui.add(toggle_switch(&mut self.lint_options.outdated_pak_version));
                            ui.end_row();

                            ui.label("Mods containing shader files");
                            ui.add(toggle_switch(&mut self.lint_options.shader_files));
                            ui.end_row();

                            ui.label("Mods containing non-asset files");
                            ui.add(toggle_switch(&mut self.lint_options.non_asset_files));
                            ui.end_row();

                            ui.label("Mods containing split {uexp, uasset} pairs");
                            ui.add(toggle_switch(&mut self.lint_options.split_asset_pairs));
                            ui.end_row();

                            ui.label("Mods containing unmodified game assets");
                            ui.add_enabled(
                                self.state.config.drg_pak_path.is_some(),
                                toggle_switch(&mut self.lint_options.unmodified_game_assets),
                            )
                            .on_disabled_hover_text(
                                "This lint requires DRG pak path to be specified",
                            );
                            ui.end_row();
                        });
                    });

                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.lints_toggle_window = None;
                        }

                        if ui
                            .add_enabled(
                                self.check_updates_rid.is_none()
                                    && self.integrate_rid.is_none()
                                    && self.lint_rid.is_none(),
                                egui::Button::new("Generate report"),
                            )
                            .clicked()
                        {
                            let lint_options = BTreeMap::from([
                                (
                                    LintId::ARCHIVE_WITH_MULTIPLE_PAKS,
                                    self.lint_options.archive_with_multiple_paks,
                                ),
                                (
                                    LintId::ARCHIVE_WITH_ONLY_NON_PAK_FILES,
                                    self.lint_options.archive_with_only_non_pak_files,
                                ),
                                (
                                    LintId::ASSET_REGISTRY_BIN,
                                    self.lint_options.asset_register_bin,
                                ),
                                (LintId::CONFLICTING, self.lint_options.conflicting),
                                (LintId::EMPTY_ARCHIVE, self.lint_options.empty_archive),
                                (
                                    LintId::OUTDATED_PAK_VERSION,
                                    self.lint_options.outdated_pak_version,
                                ),
                                (LintId::SHADER_FILES, self.lint_options.shader_files),
                                (LintId::NON_ASSET_FILES, self.lint_options.non_asset_files),
                                (
                                    LintId::SPLIT_ASSET_PAIRS,
                                    self.lint_options.split_asset_pairs,
                                ),
                                (
                                    LintId::UNMODIFIED_GAME_ASSETS,
                                    self.lint_options.unmodified_game_assets,
                                ),
                            ]);

                            trace!(?lint_options);

                            let mut mods = Vec::new();
                            self.state.mod_data.for_each_enabled_mod(
                                &self.state.mod_data.active_profile,
                                |mc| {
                                    mods.push(mc.spec.clone());
                                },
                            );

                            self.lint_report = None;
                            self.lint_rid = Some(message::LintMods::send(
                                &mut self.request_counter,
                                self.state.store.clone(),
                                mods,
                                BTreeSet::from_iter(
                                    lint_options
                                        .into_iter()
                                        .filter_map(|(lint, enabled)| enabled.then_some(lint)),
                                ),
                                self.state.config.drg_pak_path.clone(),
                                self.tx.clone(),
                                ctx.clone(),
                            ));
                            self.problematic_mod_id = None;
                            self.lint_report_window = Some(WindowLintReport);
                        }
                    });
                });

            if !open {
                self.lints_toggle_window = None;
            }
        }
    }

    fn show_lint_report(&mut self, ctx: &egui::Context) {
        if self.lint_report_window.is_some() {
            let mut open = true;

            egui::Window::new("Lint results")
                .open(&mut open)
                .resizable(true)
                .show(ctx, |ui| {
                    if let Some(report) = &self.lint_report {
                        let scroll_height =
                            (ui.available_height() - 30.0).clamp(0.0, f32::INFINITY);
                        egui::ScrollArea::vertical()
                            .max_height(scroll_height)
                            .show(ui, |ui| {
                                const AMBER: Color32 = Color32::from_rgb(255, 191, 0);

                                if let Some(conflicting_mods) = &report.conflicting_mods
                                    && !conflicting_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new("⚠ Mods(s) with conflicting asset modifications detected")
                                                .color(AMBER),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            conflicting_mods.iter().for_each(|(path, mods)| {
                                                CollapsingHeader::new(
                                                    RichText::new(format!(
                                                        "⚠ Conflicting modification of asset `{path}`"
                                                    ))
                                                    .color(AMBER),
                                                )
                                                .show(
                                                    ui,
                                                    |ui| {
                                                        mods.iter().for_each(|mod_spec| {
                                                            ui.label(&mod_spec.url);
                                                        });
                                                    },
                                                );
                                            });
                                        });
                                    }

                                if let Some(asset_register_bin_mods) = &report.asset_register_bin_mods
                                    && !asset_register_bin_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new("ℹ Mod(s) with `AssetRegistry.bin` included detected")
                                                .color(Color32::LIGHT_BLUE),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            asset_register_bin_mods.iter().for_each(
                                                |(r#mod, paths)| {
                                                    CollapsingHeader::new(
                                                        RichText::new(format!(
                                                        "ℹ {} includes one or more `AssetRegistry.bin`",
                                                        r#mod.url
                                                    ))
                                                        .color(Color32::LIGHT_BLUE),
                                                    )
                                                    .show(ui, |ui| {
                                                        paths.iter().for_each(|path| {
                                                            ui.label(path);
                                                        });
                                                    });
                                                },
                                            );
                                        });
                                    }

                                if let Some(shader_file_mods) = &report.shader_file_mods
                                    && !shader_file_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new(
                                                "⚠ Mods(s) with shader files included detected",
                                            )
                                            .color(AMBER),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            shader_file_mods.iter().for_each(
                                                |(r#mod, shader_files)| {
                                                    CollapsingHeader::new(
                                                        RichText::new(format!(
                                                            "⚠ {} includes one or more shader files",
                                                            r#mod.url
                                                        ))
                                                        .color(AMBER),
                                                    )
                                                    .show(ui, |ui| {
                                                        shader_files.iter().for_each(|shader_file| {
                                                            ui.label(shader_file);
                                                        });
                                                    });
                                                },
                                            );
                                        });
                                    }

                                if let Some(outdated_pak_version_mods) = &report.outdated_pak_version_mods
                                    && !outdated_pak_version_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new(
                                                "⚠ Mod(s) with outdated pak version detected",
                                            )
                                            .color(AMBER),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            outdated_pak_version_mods.iter().for_each(
                                                |(r#mod, version)| {
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "⚠ {} includes outdated pak version {}",
                                                            r#mod.url, version
                                                        ))
                                                        .color(AMBER),
                                                    );
                                                },
                                            );
                                        });
                                    }

                                if let Some(empty_archive_mods) = &report.empty_archive_mods
                                    && !empty_archive_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new(
                                                "⚠ Mod(s) with empty archives detected",
                                            )
                                            .color(AMBER),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            empty_archive_mods.iter().for_each(|r#mod| {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "⚠ {} contains an empty archive",
                                                        r#mod.url
                                                    ))
                                                    .color(AMBER),
                                                );
                                            });
                                        });
                                    }

                                if let Some(archive_with_only_non_pak_files_mods) = &report.archive_with_only_non_pak_files_mods
                                    && !archive_with_only_non_pak_files_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new(
                                                "⚠ Mod(s) with only non-`.pak` files detected",
                                            )
                                            .color(AMBER),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            archive_with_only_non_pak_files_mods.iter().for_each(|r#mod| {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "⚠ {} contains only non-`.pak` files, perhaps the author forgot to pack it?",
                                                        r#mod.url
                                                    ))
                                                    .color(AMBER),
                                                );
                                            });
                                        });
                                    }

                                if let Some(archive_with_multiple_paks_mods) = &report.archive_with_multiple_paks_mods
                                    && !archive_with_multiple_paks_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new(
                                                "⚠ Mod(s) with multiple `.pak`s detected",
                                            )
                                            .color(AMBER),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            archive_with_multiple_paks_mods.iter().for_each(|r#mod| {
                                                ui.label(RichText::new(format!(
                                                    "⚠ {} contains multiple `.pak`s, only the first encountered `.pak` will be loaded",
                                                    r#mod.url
                                                ))
                                                .color(AMBER));
                                            });
                                        });
                                    }

                                if let Some(non_asset_file_mods) = &report.non_asset_file_mods
                                    && !non_asset_file_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new(
                                                "⚠ Mod(s) with non-asset files detected",
                                            )
                                            .color(AMBER),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            non_asset_file_mods.iter().for_each(|(r#mod, files)| {
                                                CollapsingHeader::new(
                                                    RichText::new(format!(
                                                        "⚠ {} includes non-asset files",
                                                        r#mod.url
                                                    ))
                                                    .color(AMBER),
                                                )
                                                .show(ui, |ui| {
                                                    files.iter().for_each(|file| {
                                                        ui.label(file);
                                                    });
                                                });
                                            });
                                        });
                                    }

                                if let Some(split_asset_pairs_mods) = &report.split_asset_pairs_mods
                                    && !split_asset_pairs_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new(
                                                "⚠ Mod(s) with split {uexp, uasset} pairs detected",
                                            )
                                            .color(AMBER),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            split_asset_pairs_mods.iter().for_each(|(r#mod, files)| {
                                                CollapsingHeader::new(
                                                    RichText::new(format!(
                                                        "⚠ {} includes split {{uexp, uasset}} pairs",
                                                        r#mod.url
                                                    ))
                                                    .color(AMBER),
                                                )
                                                .show(ui, |ui| {
                                                    files.iter().for_each(|(file, kind)| {
                                                        match kind {
                                                            SplitAssetPair::MissingUasset => {
                                                                ui.label(format!("`{file}` missing matching .uasset file"));
                                                            },
                                                            SplitAssetPair::MissingUexp => {
                                                                ui.label(format!("`{file}` missing matching .uexp file"));
                                                            }
                                                        }
                                                    });
                                                });
                                            });
                                        });
                                    }

                                if let Some(unmodified_game_assets_mods) = &report.unmodified_game_assets_mods
                                    && !unmodified_game_assets_mods.is_empty() {
                                        CollapsingHeader::new(
                                            RichText::new(
                                                "⚠ Mod(s) with unmodified game assets detected",
                                            )
                                            .color(AMBER),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            unmodified_game_assets_mods.iter().for_each(|(r#mod, files)| {
                                                CollapsingHeader::new(
                                                    RichText::new(format!(
                                                        "⚠ {} includes unmodified game assets",
                                                        r#mod.url
                                                    ))
                                                    .color(AMBER),
                                                )
                                                .show(ui, |ui| {
                                                    files.iter().for_each(|file| {
                                                        ui.label(file);
                                                    });
                                                });
                                            });
                                        });
                                    }
                            });
                    } else {
                        ui.spinner();
                        ui.label("Lint report generating...");
                    }
                });

            if !open {
                self.lint_report_window = None;
                self.lint_rid = None;
            }
        }
    }

    fn get_sorting_config(&self) -> Option<SortingConfig> {
        self.state.config.sorting_config.clone()
    }

    fn update_sorting_config(&mut self, sort_category: Option<SortBy>, is_ascending: bool) {
        self.state.config.sorting_config = sort_category.map(|sort_category| SortingConfig {
            sort_category,
            is_ascending,
        });
        self.state.config.save().unwrap();
    }
}

type ModListEntry<'a> = (&'a ModOrGroup, Option<&'a ModInfo>);
fn sort_mods(config: SortingConfig) -> impl Fn(ModListEntry, ModListEntry) -> Ordering {
    move |(a, info_a), (b, info_b)| {
        if matches!(a, ModOrGroup::Group { .. }) || matches!(b, ModOrGroup::Group { .. }) {
            unimplemented!("Groups in sorting not implemented");
        }

        let ModOrGroup::Individual(mc_a) = a else {
            debug!("Item is not Individual \n{:?}", a);
            return Ordering::Equal;
        };
        let ModOrGroup::Individual(mc_b) = b else {
            debug!("Item is not Individual \n{:?}", b);
            return Ordering::Equal;
        };

        fn map_cmp<V, M, F>(a: &V, b: &V, map: F) -> Ordering
        where
            M: Ord,
            F: Fn(&V) -> M,
        {
            map(a).cmp(&map(b))
        }

        let name_order = map_cmp(&(mc_a, info_a), &(mc_b, info_b), |(mc, info)| {
            (info.map(|i| i.name.to_lowercase()), &mc.spec.url)
        });
        let provider_order = map_cmp(&info_a, &info_b, |info| info.map(|i| i.provider));
        let approval_order = map_cmp(&info_a, &info_b, |info| {
            info.and_then(|i| i.modio_tags.as_ref())
                .map(|t| t.approval_status)
        });
        let required_order = map_cmp(&info_a, &info_b, |info| {
            info.and_then(|i| i.modio_tags.as_ref())
                .map(|t| std::cmp::Reverse(t.required_status))
        });
        let mut order = match config.sort_category {
            SortBy::Enabled => mc_b.enabled.cmp(&mc_a.enabled),
            SortBy::Name => name_order,
            SortBy::Priority => mc_a.priority.cmp(&mc_b.priority),
            SortBy::Provider => provider_order,
            SortBy::RequiredStatus => required_order,
            SortBy::ApprovalCategory => approval_order,
        };

        if config.is_ascending {
            order = order.reverse();
        }
        if config.sort_category != SortBy::Name {
            order = order.then(name_order);
        }
        order
    }
}

struct WindowProviderParameters {
    tx: Sender<(RequestID, Result<(), ProviderError>)>,
    rx: Receiver<(RequestID, Result<(), ProviderError>)>,
    check_rid: Option<(RequestID, JoinHandle<()>)>,
    check_error: Option<String>,
    factory: &'static ProviderFactory,
    parameters: HashMap<String, String>,
}

impl WindowProviderParameters {
    fn new(factory: &'static ProviderFactory, state: &State) -> Self {
        let (tx, rx) = mpsc::channel(10);
        Self {
            tx,
            rx,
            check_rid: None,
            check_error: None,
            parameters: state
                .config
                .provider_parameters
                .get(factory.id)
                .cloned()
                .unwrap_or_default(),
            factory,
        }
    }
}

struct WindowSettings {
    drg_pak_path: String,
    drg_pak_path_err: Option<String>,
    backup_path: String,
    backup_status: Option<(bool, String)>, // (success, message)
}

impl WindowSettings {
    fn new(state: &State) -> Self {
        let path = state
            .config
            .drg_pak_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let backup_path = state
            .config
            .backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| Self::default_backup_path());
        Self {
            drg_pak_path: path,
            drg_pak_path_err: None,
            backup_path,
            backup_status: None,
        }
    }

    fn default_backup_path() -> String {
        directories::UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(|d| d.to_path_buf()))
            .map(|d| d.join("mint_backups"))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

struct WindowLintReport;

struct WindowLintsToggle;

/// Holds information about a pending deletion confirmation
enum PendingDeletion {
    Mod { mod_name: String, row_index: usize },
    Profile { profile_name: String },
    Folder { folder_name: String },
    FolderMod { folder_name: String, mod_index: usize, mod_name: String },
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.needs_restart
            && let Some(original_exe_path) = &self.original_exe_path
        {
            debug!("needs restart");
            self.needs_restart = false;

            debug!("restarting...");
            let _child = std::process::Command::new(original_exe_path)
                .spawn()
                .unwrap();
            debug!("created child process");

            std::process::exit(0);
        }

        // do some init things that depend on ctx so cannot be done earlier
        if !self.has_run_init {
            self.has_run_init = true;

            let theme = GuiTheme::into_egui_theme(self.state.config.gui_theme);
            ctx.memory_mut(|m| m.options.theme_preference = theme);

            message::CheckUpdates::send(self, ctx);
        }

        // message handling
        while let Ok(msg) = self.rx.try_recv() {
            msg.handle(self);
        }

        // begin draw

        self.show_update_window(ctx);
        self.show_provider_parameters(ctx);
        self.show_profile_windows(ctx);
        self.show_settings(ctx);
        self.show_lints_toggle(ctx);
        self.show_lint_report(ctx);
        self.show_delete_confirmation(ctx);
        self.show_create_folder_popup(ctx);
        self.show_rename_folder_popup(ctx);

        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.with_layout(egui::Layout::right_to_left(Align::TOP), |ui| {
                ui.add_enabled_ui(
                    self.integrate_rid.is_none()
                        && self.update_rid.is_none()
                        && self.lint_rid.is_none()
                        && self.self_update_rid.is_none()
                        && self.state.config.drg_pak_path.is_some(),
                    |ui| {
                        if let Some(args) = &self.args
                            && ui
                                .button("Launch game")
                                .on_hover_ui(|ui| {
                                    for arg in args {
                                        ui.label(arg);
                                    }
                                })
                                .clicked()
                        {
                            let args = args.clone();
                            std::thread::spawn(move || {
                                let mut iter = args.iter();
                                std::process::Command::new(iter.next().unwrap())
                                    .args(iter)
                                    .spawn()
                                    .unwrap()
                                    .wait()
                                    .unwrap();
                            });
                        }

                        ui.add_enabled_ui(self.state.config.drg_pak_path.is_some(), |ui| {
                            let mut button = ui.button("Install mods");
                            if self.state.config.drg_pak_path.is_none() {
                                button = button.on_disabled_hover_text(
                                    "DRG install not found. Configure it in the settings menu.",
                                );
                            }

                            if button.clicked() {
                                let mut mods = Vec::new();
                                let active_profile = self.state.mod_data.active_profile.clone();
                                
                                // Get mods with effective priority (respecting folder overrides)
                                let mut mods_with_priority = self.state
                                    .mod_data
                                    .get_enabled_mods_with_priority(&active_profile);

                                // Sort by effective priority (descending)
                                mods_with_priority.sort_by_key(|(_, priority)| -priority);

                                for (config, _) in mods_with_priority {
                                    mods.push(config.spec.clone());
                                }

                                self.last_action = None;
                                self.integrate_rid = Some(message::Integrate::send(
                                    &mut self.request_counter,
                                    self.state.store.clone(),
                                    mods,
                                    self.state.config.drg_pak_path.as_ref().unwrap().clone(),
                                    self.state.config.deref().into(),
                                    self.tx.clone(),
                                    ctx.clone(),
                                ));
                                self.problematic_mod_id = None;
                            }
                        });

                        ui.add_enabled_ui(self.state.config.drg_pak_path.is_some(), |ui| {
                            let mut button = ui.button("Uninstall mods");
                            if self.state.config.drg_pak_path.is_none() {
                                button = button.on_disabled_hover_text(
                                    "DRG install not found. Configure it in the settings menu.",
                                );
                            }
                            if button.clicked() {
                                self.last_action = None;
                                if let Some(pak_path) = &self.state.config.drg_pak_path {
                                    let mut mods = HashSet::default();
                                    let active_profile = self.state.mod_data.active_profile.clone();
                                    self.state.mod_data.for_each_enabled_mod(
                                        &active_profile,
                                        |mc| {
                                            if let Some(modio_id) = self
                                                .state
                                                .store
                                                .get_mod_info(&mc.spec)
                                                .and_then(|i| i.modio_id)
                                            {
                                                mods.insert(modio_id);
                                            }
                                        },
                                    );

                                    debug!("uninstalling mods: pak_path = {}", pak_path.display());
                                    self.last_action = Some(match uninstall(pak_path, mods) {
                                        Ok(()) => LastAction::success(
                                            "Successfully uninstalled mods".to_string(),
                                        ),
                                        Err(e) => LastAction::failure(format!(
                                            "Failed to uninstall mods: {e}"
                                        )),
                                    })
                                }
                            }
                        });

                        if ui
                            .button("Update cache")
                            .on_hover_text(
                                "Checks for updates for all mods and updates local cache",
                            )
                            .clicked()
                        {
                            message::UpdateCache::send(self);
                            self.problematic_mod_id = None;
                        }
                    },
                );
                if self.integrate_rid.is_some() {
                    if ui.button("Cancel").clicked() {
                        self.integrate_rid.take().unwrap().handle.abort();
                    }
                    ui.spinner();
                }
                if self.update_rid.is_some() {
                    if ui.button("Cancel").clicked() {
                        self.update_rid.take().unwrap().handle.abort();
                    }
                    ui.spinner();
                }
                if ui
                    .button("Lint mods")
                    .on_hover_text("Lint mods in the current profile")
                    .clicked()
                {
                    self.lints_toggle_window = Some(WindowLintsToggle);
                }
                if ui.button("⚙").on_hover_text("Open settings").clicked() {
                    self.settings_window = Some(WindowSettings::new(&self.state));
                }
                if let Some(available_update) = &self.available_update
                    && ui
                        .button(egui::RichText::new("\u{26A0}").color(ui.visuals().warn_fg_color))
                        .on_hover_text(format!(
                            "Update available: {}\n{}",
                            available_update.tag_name, available_update.html_url
                        ))
                        .clicked()
                {
                    ui.ctx()
                        .open_url(egui::OpenUrl::new_tab(&available_update.html_url));
                }
                ui.with_layout(egui::Layout::left_to_right(Align::TOP), |ui| {
                    if let Some(last_action) = &self.last_action {
                        let msg = match &last_action.status {
                            LastActionStatus::Success(msg) => {
                                ui.label(
                                    egui::RichText::new("STATUS")
                                        .color(Color32::BLACK)
                                        .background_color(Color32::LIGHT_GREEN),
                                );
                                msg
                            }
                            LastActionStatus::Failure(msg) => {
                                ui.label(
                                    egui::RichText::new("STATUS")
                                        .color(Color32::BLACK)
                                        .background_color(Color32::LIGHT_RED),
                                );
                                msg
                            }
                        };
                        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1)); // throttle timeago updates
                        ui.label(format!("({}): {}", last_action.timeago(), msg));
                    }
                });
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.integrate_rid.is_some() || self.update_rid.is_some() || self.lint_rid.is_some()
            {
                ui.disable();
            }
            // profile selection

            let buttons = |ui: &mut Ui, mod_data: &mut ModData| {
                if ui
                    .button("📋")
                    .on_hover_text_at_pointer("Copy profile mods")
                    .clicked()
                {
                    let mut mods = Vec::new();
                    let active_profile = mod_data.active_profile.clone();
                    mod_data.for_each_enabled_mod(&active_profile, |mc| {
                        mods.push(mc.clone());
                    });
                    let mods = Self::build_mod_string(&mods);
                    ui.ctx().copy_text(mods);
                }

                // TODO find better icon, flesh out multiple-view usage, fix GUI locking
                /*
                if ui
                    .button("pop out")
                    .on_hover_text_at_pointer("pop out")
                    .clicked()
                {
                    self.open_profiles.insert(mod_data.active_profile.clone());
                }
                */
            };

            let (modified, pending_profile_delete) = named_combobox::ui(
                ui,
                "profile",
                self.state.mod_data.deref_mut().deref_mut(),
                Some(buttons),
            );
            if modified {
                self.state.mod_data.save().unwrap();
            }
            if let Some(profile_name) = pending_profile_delete {
                self.pending_deletion = Some(PendingDeletion::Profile { profile_name });
            }

            ui.separator();

            ui.with_layout(egui::Layout::right_to_left(Align::TOP), |ui| {
                if self.resolve_mod_rid.is_some() {
                    ui.spinner();
                }
                ui.with_layout(ui.layout().with_main_justify(true), |ui| {
                    // define multiline layouter to be able to show multiple lines in a single line widget
                    let font_id = FontSelection::default().resolve(ui.style());
                    let text_color = ui.visuals().widgets.inactive.text_color();
                    let mut multiline_layouter = move |ui: &Ui, text: &str, wrap_width: f32| {
                        let layout_job = LayoutJob::simple(
                            text.to_string(),
                            font_id.clone(),
                            text_color,
                            wrap_width,
                        );
                        ui.fonts(|f| f.layout_job(layout_job))
                    };

                    let resolve = ui.add_enabled(
                        self.resolve_mod_rid.is_none(),
                        egui::TextEdit::singleline(&mut self.resolve_mod)
                            .layouter(&mut multiline_layouter)
                            .hint_text("Add mod..."),
                    );
                    if is_committed(&resolve) {
                        message::ResolveMods::send(self, ctx, self.parse_mods(), false);
                        self.problematic_mod_id = None;
                    }
                });
            });

            let profile = self.state.mod_data.active_profile.clone();

            ui.horizontal(|ui| {
                ui.label("Sort by: ");

                let (mut sort_category, mut is_ascending) = self
                    .get_sorting_config()
                    .map(|c| (Some(c.sort_category), c.is_ascending))
                    .unwrap_or_default();

                let mut clicked = ui.radio_value(&mut sort_category, None, "Manual").clicked();
                for category in SortBy::iter() {
                    let mut radio_label = category.as_str().to_owned();
                    if sort_category == Some(category) {
                        radio_label.push_str(if is_ascending { " ⏶" } else { " ⏷" });
                    }
                    let resp = ui.radio_value(&mut sort_category, Some(category), radio_label);
                    if resp.clicked() {
                        clicked = true;
                        if resp.changed() {
                            is_ascending = true;
                        } else {
                            is_ascending = !is_ascending;
                        }
                    };
                }
                if clicked {
                    self.update_sorting_config(sort_category, is_ascending);
                }

                ui.add_space(16.);

                // Create folder button
                if ui.button("📁+").on_hover_text("Create new folder").clicked() {
                    self.create_folder_popup = Some(String::new());
                }

                ui.add_space(8.);

                // TODO: actually implement mod groups.
                let search_string = &mut self.search_string;
                let lower = search_string.to_lowercase();
                let any_matches = self.state.mod_data.any_mod(&profile, |mc, _| {
                    self.state
                        .store
                        .get_mod_info(&mc.spec)
                        .map(|i| i.name.to_lowercase().contains(&lower))
                        .unwrap_or(false)
                });

                let mut text_edit = egui::TextEdit::singleline(search_string).hint_text("Search");
                if !any_matches {
                    text_edit = text_edit.text_color(ui.visuals().error_fg_color);
                }
                let res = ui
                    .scope_builder(
                        egui::UiBuilder::new().layout(egui::Layout::bottom_up(Align::RIGHT)),
                        |ui| ui.add(text_edit),
                    )
                    .inner;
                if res.changed() {
                    self.scroll_to_match = true;
                }
                if res.lost_focus()
                    && ui.input(|i| {
                        i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape)
                    })
                {
                    *search_string = String::new();
                    self.scroll_to_match = false;
                } else if self.focus_search {
                    res.request_focus();
                    self.focus_search = false;
                }
            });
            ui.add_space(4.);

            self.ui_profile(ui, &profile);

            // must access memory outside of input lock to prevent deadlock
            let is_anything_focused = ctx.memory(|m| m.focused().is_some());
            ctx.input(|i| {
                if !i.raw.dropped_files.is_empty()
                    && self.integrate_rid.is_none()
                    && self.update_rid.is_none()
                {
                    let mut mods = String::new();
                    for f in i
                        .raw
                        .dropped_files
                        .iter()
                        .filter_map(|f| f.path.as_ref().map(|p| p.to_string_lossy()))
                    {
                        mods.push_str(&f);
                        mods.push('\n');
                    }

                    self.resolve_mod = mods.trim().to_string();
                    message::ResolveMods::send(self, ctx, self.parse_mods(), false);
                    self.problematic_mod_id = None;
                }
                for e in &i.events {
                    match e {
                        egui::Event::Paste(s) => {
                            if self.integrate_rid.is_none()
                                && self.update_rid.is_none()
                                && self.lint_rid.is_none()
                                && !is_anything_focused
                            {
                                self.resolve_mod = s.trim().to_string();
                                message::ResolveMods::send(self, ctx, self.parse_mods(), false);
                            }
                        }
                        egui::Event::Text(text) => {
                            if !is_anything_focused {
                                self.search_string = text.to_string();
                                self.scroll_to_match = true;
                                self.focus_search = true;
                            }
                        }
                        _ => {}
                    }
                }
            });
        });
    }
}

fn is_committed(res: &egui::Response) -> bool {
    res.lost_focus() && res.ctx.input(|i| i.key_pressed(egui::Key::Enter))
}

/// A custom popup which does not automatically close when clicked.
fn custom_popup_above_or_below_widget<R>(
    ui: &Ui,
    popup_id: egui::Id,
    widget_response: &egui::Response,
    above_or_below: egui::AboveOrBelow,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    if ui.memory(|mem| mem.is_popup_open(popup_id)) {
        let (pos, pivot) = match above_or_below {
            egui::AboveOrBelow::Above => (widget_response.rect.left_top(), Align2::LEFT_BOTTOM),
            egui::AboveOrBelow::Below => (widget_response.rect.left_bottom(), Align2::LEFT_TOP),
        };

        let inner = egui::Area::new(popup_id)
            .order(egui::Order::Foreground)
            .constrain(true)
            .fixed_pos(pos)
            .pivot(pivot)
            .show(ui.ctx(), |ui| {
                // Note: we use a separate clip-rect for this area, so the popup can be outside the parent.
                // See https://github.com/emilk/egui/issues/825
                let frame = egui::Frame::popup(ui.style());
                let frame_margin = frame.total_margin();
                frame
                    .show(ui, |ui| {
                        ui.with_layout(Layout::top_down_justified(Align::LEFT), |ui| {
                            ui.set_width(widget_response.rect.width() - frame_margin.sum().x);
                            add_contents(ui)
                        })
                        .inner
                    })
                    .inner
            })
            .inner;

        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            ui.memory_mut(|mem| mem.close_popup());
        }
        Some(inner)
    } else {
        None
    }
}

#[derive(Debug)]
pub enum SpecFetchProgress {
    Progress { progress: u64, size: u64 },
    Complete,
}

impl From<FetchProgress> for SpecFetchProgress {
    fn from(value: FetchProgress) -> Self {
        match value {
            FetchProgress::Progress { progress, size, .. } => Self::Progress { progress, size },
            FetchProgress::Complete { .. } => Self::Complete,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum SelfUpdateProgress {
    Pending,
    Progress { progress: u64, size: u64 },
    Complete,
}
