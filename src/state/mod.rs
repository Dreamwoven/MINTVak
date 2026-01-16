pub mod config;

use std::{
    collections::{BTreeMap, HashMap},
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::Arc,
};

use fs_err as fs;
use serde::{Deserialize, Serialize};
use snafu::prelude::*;

use self::config::ConfigWrapper;
use crate::{
    Dirs,
    gui::GuiTheme,
    providers::{ModSpecification, ModStore},
};
use crate::{gui::SortBy, providers::ProviderError};
use mint_lib::{DRGInstallation, mod_info::MetaConfig};

/// Mod configuration, holds ModSpecification as well as other metadata
#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct ModConfig {
    pub spec: ModSpecification,
    pub required: bool,

    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub priority: i32,
}

fn default_true() -> bool {
    true
}

fn is_zero(value: &i32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModGroup {
    pub mods: Vec<ModConfig>,
    /// When Some, all mods in this group use this priority instead of their individual priority
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_override: Option<i32>,
}

#[obake::versioned]
#[obake(version("0.0.0"))]
#[obake(version("0.1.0"))]
#[obake(version("0.2.0"))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModProfile {
    #[obake(cfg("0.0.0"))]
    pub mods: Vec<ModConfig>,

    /// A profile can contain ordered individual mods mixed with mod groups.
    #[obake(cfg("0.1.0"))]
    #[obake(cfg("0.2.0"))]
    pub mods: Vec<ModOrGroup>,
    
    /// Per-profile folder storage (added in 0.2.0)
    #[obake(cfg("0.2.0"))]
    #[serde(default)]
    pub groups: BTreeMap<String, ModGroup>,
}

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModOrGroup {
    Group { group_name: String, enabled: bool },
    Individual(ModConfig),
}

impl From<ModProfile!["0.0.0"]> for ModProfile!["0.1.0"] {
    fn from(_legacy: ModProfile!["0.0.0"]) -> Self {
        // The migration requires `ModData` to handle instead.
        unimplemented!("migration requires handling from `ModData`")
    }
}

impl From<ModProfile!["0.1.0"]> for ModProfile!["0.2.0"] {
    fn from(legacy: ModProfile!["0.1.0"]) -> Self {
        Self {
            mods: legacy.mods,
            groups: BTreeMap::new(), // Will be populated during ModData migration
        }
    }
}

#[obake::versioned]
#[obake(version("0.0.0"))]
#[obake(version("0.1.0"))]
#[obake(version("0.2.0"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModData {
    pub active_profile: String,
    #[obake(cfg("0.0.0"))]
    pub profiles: BTreeMap<String, ModProfile!["0.0.0"]>,
    #[obake(cfg("0.1.0"))]
    pub profiles: BTreeMap<String, ModProfile!["0.1.0"]>,
    #[obake(cfg("0.2.0"))]
    pub profiles: BTreeMap<String, ModProfile!["0.2.0"]>,
    /// Global groups storage (legacy, removed in 0.2.0)
    #[obake(cfg("0.1.0"))]
    pub groups: BTreeMap<String, ModGroup>,
}

impl ModData!["0.2.0"] {
    pub fn for_each_mod_predicate<
        F: FnMut(&ModConfig),
        G: FnMut(bool /* mod group enabled? */) -> bool,
        P: FnMut(&ModConfig) -> bool,
    >(
        &self,
        profile: &str,
        mut f: F,
        mut g: G,
        mut p: P,
    ) {
        let prof = self.profiles.get(profile).unwrap();
        for ref mod_or_group in &prof.mods {
            match mod_or_group {
                ModOrGroup::Group {
                    group_name,
                    enabled,
                } => {
                    if g(*enabled) {
                        if let Some(group) = prof.groups.get(group_name) {
                            for mc in &group.mods {
                                if p(mc) {
                                    f(mc);
                                }
                            }
                        }
                    }
                }
                ModOrGroup::Individual(mc) => {
                    if p(mc) {
                        f(mc);
                    }
                }
            }
        }
    }

    pub fn for_each_mod_predicate_mut<
        F: FnMut(&mut ModConfig),
        G: FnMut(bool /* mod group enabled? */) -> bool,
        P: FnMut(&mut ModConfig) -> bool,
    >(
        &mut self,
        profile: &str,
        mut f: F,
        mut g: G,
        mut p: P,
    ) {
        let prof = self.profiles.get_mut(profile).unwrap();
        // Need to iterate mods and groups separately due to borrow checker
        let group_refs: Vec<_> = prof.mods.iter().filter_map(|m| {
            if let ModOrGroup::Group { group_name, enabled } = m {
                Some((group_name.clone(), *enabled))
            } else {
                None
            }
        }).collect();
        
        // Process groups
        for (group_name, enabled) in group_refs {
            if g(enabled) {
                if let Some(group) = prof.groups.get_mut(&group_name) {
                    for mc in &mut group.mods {
                        if p(mc) {
                            f(mc);
                        }
                    }
                }
            }
        }
        
        // Process individual mods
        for mod_or_group in &mut prof.mods {
            if let ModOrGroup::Individual(mc) = mod_or_group {
                if p(mc) {
                    f(mc);
                }
            }
        }
    }

    pub fn for_each_mod<F: FnMut(&ModConfig)>(&self, profile: &str, f: F) {
        self.for_each_mod_predicate(profile, f, |_| true, |_| true)
    }

    pub fn for_each_enabled_mod<F: FnMut(&ModConfig)>(&self, profile: &str, f: F) {
        self.for_each_mod_predicate(profile, f, std::convert::identity, |mc| mc.enabled)
    }

    /// Returns enabled mods with their effective priority (considering folder overrides)
    /// Returns Vec of (ModConfig clone, effective_priority)
    pub fn get_enabled_mods_with_priority(&self, profile: &str) -> Vec<(ModConfig, i32)> {
        let mut result = Vec::new();
        let prof = self.profiles.get(profile).unwrap();
        for mod_or_group in &prof.mods {
            match mod_or_group {
                ModOrGroup::Group { group_name, enabled } => {
                    if *enabled {
                        if let Some(group) = prof.groups.get(group_name) {
                            let override_priority = group.priority_override;
                            for mc in &group.mods {
                                if mc.enabled {
                                    let effective_priority = override_priority.unwrap_or(mc.priority);
                                    result.push((mc.clone(), effective_priority));
                                }
                            }
                        }
                    }
                }
                ModOrGroup::Individual(mc) => {
                    if mc.enabled {
                        result.push((mc.clone(), mc.priority));
                    }
                }
            }
        }
        result
    }

    pub fn for_each_mod_mut<F: FnMut(&mut ModConfig)>(&mut self, profile: &str, f: F) {
        self.for_each_mod_predicate_mut(profile, f, |_| true, |_| true)
    }

    pub fn any_mod<F: FnMut(&ModConfig, Option<bool> /* mod group enabled? */) -> bool>(
        &self,
        profile: &str,
        mut f: F,
    ) -> bool {
        let prof = self.profiles.get(profile).unwrap();
        prof.mods.iter().any(|m| {
            let f = &mut f;
            match m {
                ModOrGroup::Individual(mc) => f(mc, None),
                ModOrGroup::Group {
                    group_name,
                    enabled,
                } => prof
                    .groups
                    .get(group_name)
                    .map(|g| g.mods.iter().any(|mc| f(mc, Some(*enabled))))
                    .unwrap_or(false),
            }
        })
    }

    pub fn any_mod_mut<
        F: FnMut(&mut ModConfig, Option<&mut bool> /* mod group enabled? */) -> bool,
    >(
        &mut self,
        profile: &str,
        mut f: F,
    ) -> bool {
        let prof = self.profiles.get_mut(profile).unwrap();
        // Collect group names first to avoid borrow issues
        let group_names: Vec<_> = prof.mods.iter().filter_map(|m| {
            if let ModOrGroup::Group { group_name, .. } = m {
                Some(group_name.clone())
            } else {
                None
            }
        }).collect();
        
        // Check individual mods
        for m in &mut prof.mods {
            if let ModOrGroup::Individual(mc) = m {
                if f(mc, None) {
                    return true;
                }
            }
        }
        
        // Check group mods (need to get enabled state from mods vec)
        for group_name in group_names {
            let enabled = prof.mods.iter_mut().find_map(|m| {
                if let ModOrGroup::Group { group_name: gn, enabled } = m {
                    if gn == &group_name { Some(enabled) } else { None }
                } else {
                    None
                }
            });
            
            if let Some(enabled) = enabled {
                if let Some(group) = prof.groups.get_mut(&group_name) {
                    for mc in &mut group.mods {
                        if f(mc, Some(enabled)) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

impl Default for ModData!["0.1.0"] {
    fn default() -> Self {
        Self {
            active_profile: "default".to_string(),
            profiles: [("default".to_string(), Default::default())]
                .into_iter()
                .collect(),
            groups: BTreeMap::default(),
        }
    }
}

impl From<ModData!["0.0.0"]> for ModData!["0.1.0"] {
    fn from(legacy: ModData!["0.0.0"]) -> Self {
        let mut new_profiles = Vec::new();
        for (name, profile) in legacy.profiles {
            let new_profile = ModProfile_v0_1_0 {
                mods: profile
                    .mods
                    .into_iter()
                    .map(ModOrGroup::Individual)
                    .collect(),
            };
            new_profiles.push((name, new_profile));
        }

        Self {
            active_profile: legacy.active_profile,
            profiles: new_profiles.into_iter().collect(),
            groups: BTreeMap::default(),
        }
    }
}

impl From<ModData!["0.1.0"]> for ModData!["0.2.0"] {
    fn from(legacy: ModData!["0.1.0"]) -> Self {
        // Migrate global groups into per-profile groups
        // Each profile gets a copy of the groups it references
        let mut new_profiles = BTreeMap::new();
        
        for (name, profile) in legacy.profiles {
            // Find which groups this profile references
            let mut profile_groups = BTreeMap::new();
            for item in &profile.mods {
                if let ModOrGroup::Group { group_name, .. } = item {
                    if let Some(group) = legacy.groups.get(group_name) {
                        profile_groups.insert(group_name.clone(), group.clone());
                    }
                }
            }
            
            let new_profile = ModProfile_v0_2_0 {
                mods: profile.mods,
                groups: profile_groups,
            };
            new_profiles.insert(name, new_profile);
        }

        Self {
            active_profile: legacy.active_profile,
            profiles: new_profiles,
        }
    }
}

impl Default for ModData!["0.2.0"] {
    fn default() -> Self {
        Self {
            active_profile: "default".to_string(),
            profiles: [("default".to_string(), Default::default())]
                .into_iter()
                .collect(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "version")]
pub enum VersionAnnotatedModData {
    #[serde(rename = "0.0.0")]
    V0_0_0(ModData!["0.0.0"]),
    #[serde(rename = "0.1.0")]
    V0_1_0(ModData!["0.1.0"]),
    #[serde(rename = "0.2.0")]
    V0_2_0(ModData!["0.2.0"]),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MaybeVersionedModData {
    Versioned(VersionAnnotatedModData),
    Legacy(ModData!["0.0.0"]),
}

impl Default for ModData!["0.0.0"] {
    fn default() -> Self {
        Self {
            active_profile: "default".to_string(),
            profiles: [("default".to_string(), Default::default())]
                .into_iter()
                .collect(),
        }
    }
}

impl Default for MaybeVersionedModData {
    fn default() -> Self {
        MaybeVersionedModData::Versioned(Default::default())
    }
}

impl Default for VersionAnnotatedModData {
    fn default() -> Self {
        VersionAnnotatedModData::V0_2_0(Default::default())
    }
}

impl Deref for VersionAnnotatedModData {
    type Target = ModData!["0.2.0"];

    fn deref(&self) -> &Self::Target {
        match self {
            VersionAnnotatedModData::V0_0_0(_) => unreachable!(),
            VersionAnnotatedModData::V0_1_0(_) => unreachable!(),
            VersionAnnotatedModData::V0_2_0(md) => md,
        }
    }
}

impl DerefMut for VersionAnnotatedModData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            VersionAnnotatedModData::V0_0_0(_) => unreachable!(),
            VersionAnnotatedModData::V0_1_0(_) => unreachable!(),
            VersionAnnotatedModData::V0_2_0(md) => md,
        }
    }
}

impl ModData!["0.2.0"] {
    pub fn get_active_profile(&self) -> &ModProfile!["0.2.0"] {
        &self.profiles[&self.active_profile]
    }

    pub fn get_active_profile_mut(&mut self) -> &mut ModProfile!["0.2.0"] {
        self.profiles.get_mut(&self.active_profile).unwrap()
    }

    pub fn remove_active_profile(&mut self) {
        self.profiles.remove(&self.active_profile);
        self.active_profile = self.profiles.keys().next().unwrap().to_string();
    }
}

#[obake::versioned]
#[obake(version("0.0.0"))]
#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub provider_parameters: HashMap<String, HashMap<String, String>>,
    pub drg_pak_path: Option<PathBuf>,
    pub gui_theme: Option<GuiTheme>,
    pub sorting_config: Option<SortingConfig>,
    #[serde(default = "default_true")]
    pub confirm_mod_deletion: bool,
    #[serde(default = "default_true")]
    pub confirm_profile_deletion: bool,
    #[serde(default)]
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SortingConfig {
    pub sort_category: SortBy,
    pub is_ascending: bool,
}

impl Default for SortingConfig {
    fn default() -> Self {
        Self {
            sort_category: SortBy::Enabled,
            is_ascending: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "version")]
pub enum VersionAnnotatedConfig {
    #[serde(rename = "0.0.0")]
    V0_0_0(Config!["0.0.0"]),
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MaybeVersionedConfig {
    Versioned(VersionAnnotatedConfig),
    Legacy(Config!["0.0.0"]),
}

impl Default for MaybeVersionedConfig {
    fn default() -> Self {
        MaybeVersionedConfig::Versioned(Default::default())
    }
}

impl Default for VersionAnnotatedConfig {
    fn default() -> Self {
        VersionAnnotatedConfig::V0_0_0(Default::default())
    }
}

impl Deref for VersionAnnotatedConfig {
    type Target = Config!["0.0.0"];

    fn deref(&self) -> &Self::Target {
        match self {
            VersionAnnotatedConfig::V0_0_0(cfg) => cfg,
            VersionAnnotatedConfig::Unsupported => unreachable!(),
        }
    }
}

impl DerefMut for VersionAnnotatedConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            VersionAnnotatedConfig::V0_0_0(cfg) => cfg,
            VersionAnnotatedConfig::Unsupported => unreachable!(),
        }
    }
}

impl Default for Config!["0.0.0"] {
    fn default() -> Self {
        Self {
            provider_parameters: Default::default(),
            drg_pak_path: DRGInstallation::find()
                .as_ref()
                .map(DRGInstallation::main_pak),
            gui_theme: None,
            sorting_config: None,
            confirm_mod_deletion: true,
            confirm_profile_deletion: true,
            backup_path: None,
        }
    }
}

impl From<&VersionAnnotatedConfig> for MetaConfig {
    fn from(_value: &VersionAnnotatedConfig) -> Self {
        MetaConfig {}
    }
}

#[derive(Debug, Snafu)]
pub enum StateError {
    #[snafu(display("failed to deserialize user config"))]
    CfgDeserializationFailed { source: serde_json::Error },
    #[snafu(display("unsupported config version"))]
    UnsupportedCfgVersion,
    #[snafu(display("failed to read config.json"))]
    CfgReadFailed { source: std::io::Error },
    #[snafu(display("failed to save config"))]
    CfgSaveFailed { source: std::io::Error },
    #[snafu(display("failed to serialize user config"))]
    CfgSerializationFailed { source: serde_json::Error },
    #[snafu(transparent)]
    IoError { source: std::io::Error },
    #[snafu(transparent)]
    PersistError { source: tempfile::PersistError },
    #[snafu(transparent)]
    ProviderError { source: ProviderError },
    #[snafu(display("failed to deserialize mod data"))]
    ModDataDeserializationFailed { source: serde_json::Error },
    #[snafu(display("failed to deserialize legacy profiles"))]
    LegacyProfilesDeserializationFailed { source: serde_json::Error },
}

pub struct State {
    pub dirs: Dirs,
    pub config: ConfigWrapper<VersionAnnotatedConfig>,
    pub mod_data: ConfigWrapper<VersionAnnotatedModData>,
    pub store: Arc<ModStore>,
}

impl State {
    pub fn init(dirs: Dirs) -> Result<Self, StateError> {
        let config_path = dirs.config_dir.join("config.json");

        let config = read_config_or_default(&config_path)?;
        let config = ConfigWrapper::<VersionAnnotatedConfig>::new(&config_path, config);
        config.save().unwrap();

        let legacy_mod_profiles_path = dirs.config_dir.join("profiles.json");
        let mod_data_path = dirs.config_dir.join("mod_data.json");
        let mod_data = read_mod_data_or_default(&mod_data_path, legacy_mod_profiles_path)?;
        let mod_data = ConfigWrapper::<VersionAnnotatedModData>::new(mod_data_path, mod_data);
        mod_data.save().unwrap();

        let store = ModStore::new(&dirs.cache_dir, &config.provider_parameters)?.into();

        Ok(Self {
            dirs,
            config,
            mod_data,
            store,
        })
    }
}

fn read_config_or_default(config_path: &PathBuf) -> Result<VersionAnnotatedConfig, StateError> {
    Ok(match fs::read(config_path) {
        Ok(buf) => {
            let config = serde_json::from_slice::<MaybeVersionedConfig>(&buf)
                .context(CfgDeserializationFailedSnafu)?;
            match config {
                MaybeVersionedConfig::Versioned(v) => match v {
                    VersionAnnotatedConfig::V0_0_0(v) => VersionAnnotatedConfig::V0_0_0(v),
                    VersionAnnotatedConfig::Unsupported => UnsupportedCfgVersionSnafu.fail()?,
                },
                MaybeVersionedConfig::Legacy(legacy) => {
                    VersionAnnotatedConfig::V0_0_0(Config_v0_0_0 {
                        provider_parameters: legacy.provider_parameters,
                        drg_pak_path: legacy.drg_pak_path,
                        ..Default::default()
                    })
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => VersionAnnotatedConfig::default(),
        Err(e) => Err(e)?,
    })
}

fn read_mod_data_or_default(
    mod_data_path: &PathBuf,
    legacy_mod_profiles_path: PathBuf,
) -> Result<VersionAnnotatedModData, StateError> {
    let mod_data = match fs::read(mod_data_path) {
        Ok(buf) => serde_json::from_slice::<MaybeVersionedModData>(&buf)
            .context(ModDataDeserializationFailedSnafu)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match fs::read(&legacy_mod_profiles_path) {
                Ok(buf) => {
                    let mod_data = serde_json::from_slice::<MaybeVersionedModData>(&buf)
                        .context(LegacyProfilesDeserializationFailedSnafu)?;
                    fs::remove_file(&legacy_mod_profiles_path)?;
                    mod_data
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    MaybeVersionedModData::default()
                }
                Err(e) => Err(e)?,
            }
        }
        Err(e) => Err(e)?,
    };

    let mod_data = match mod_data {
        MaybeVersionedModData::Legacy(legacy) => {
            // 0.0.0 -> 0.1.0 -> 0.2.0
            let v0_1_0: ModData_v0_1_0 = legacy.into();
            VersionAnnotatedModData::V0_2_0(v0_1_0.into())
        }
        MaybeVersionedModData::Versioned(v) => match v {
            VersionAnnotatedModData::V0_0_0(md) => {
                // 0.0.0 -> 0.1.0 -> 0.2.0
                let v0_1_0: ModData_v0_1_0 = md.into();
                VersionAnnotatedModData::V0_2_0(v0_1_0.into())
            }
            VersionAnnotatedModData::V0_1_0(md) => {
                // 0.1.0 -> 0.2.0
                VersionAnnotatedModData::V0_2_0(md.into())
            }
            VersionAnnotatedModData::V0_2_0(md) => VersionAnnotatedModData::V0_2_0(md),
        },
    };

    Ok(mod_data)
}

#[cfg(test)]
mod mod_data_tests {
    use super::{
        ModConfig, ModData_v0_1_0 as ModData, ModGroup, ModOrGroup, ModProfile_v0_1_0 as ModProfile,
    };
    use crate::providers::ModSpecification;

    #[test]
    fn test_for_each_mod() {
        let mod_1 = ModConfig {
            spec: ModSpecification::new("a".to_string()),
            required: false,
            enabled: false,
            priority: 50,
        };

        let mod_2 = ModConfig {
            spec: ModSpecification::new("b".to_string()),
            required: true,
            enabled: false,
            priority: 50,
        };

        let mod_3 = ModConfig {
            spec: ModSpecification::new("c".to_string()),
            required: false,
            enabled: true,
            priority: 50,
        };

        let mod_data = ModData {
            active_profile: "default".to_string(),
            profiles: [(
                "default".to_string(),
                ModProfile {
                    mods: vec![
                        ModOrGroup::Individual(mod_1),
                        ModOrGroup::Group {
                            group_name: "mg1".to_string(),
                            enabled: false,
                        },
                    ],
                },
            )]
            .into(),
            groups: [(
                "mg1".to_string(),
                ModGroup {
                    mods: vec![mod_2, mod_3],
                },
            )]
            .into(),
        };

        let mut counter = 0;
        mod_data.for_each_mod("default", |_| {
            counter += 1;
        });
        assert_eq!(counter, 3);
    }

    #[test]
    fn test_for_each_enabled_mod() {
        let mod_1 = ModConfig {
            spec: ModSpecification::new("a".to_string()),
            required: false,
            enabled: false,
            priority: 50,
        };

        let mod_2 = ModConfig {
            spec: ModSpecification::new("b".to_string()),
            required: true,
            enabled: false,
            priority: 50,
        };

        let mod_3 = ModConfig {
            spec: ModSpecification::new("c".to_string()),
            required: false,
            enabled: true,
            priority: 50,
        };

        let mod_data = ModData {
            active_profile: "default".to_string(),
            profiles: [(
                "default".to_string(),
                ModProfile {
                    mods: vec![
                        ModOrGroup::Individual(mod_1),
                        ModOrGroup::Group {
                            group_name: "mg1".to_string(),
                            enabled: true,
                        },
                    ],
                },
            )]
            .into(),
            groups: [(
                "mg1".to_string(),
                ModGroup {
                    mods: vec![mod_2, mod_3],
                },
            )]
            .into(),
        };

        let mut counter = 0;
        mod_data.for_each_enabled_mod("default", |_| {
            counter += 1;
        });
        assert_eq!(counter, 1);
    }

    #[test]
    fn test_any_mod() {
        let mod_1 = ModConfig {
            spec: ModSpecification::new("a".to_string()),
            required: false,
            enabled: false,
            priority: 50,
        };

        let mod_2 = ModConfig {
            spec: ModSpecification::new("b".to_string()),
            required: true,
            enabled: false,
            priority: 50,
        };

        let mod_3 = ModConfig {
            spec: ModSpecification::new("c".to_string()),
            required: false,
            enabled: true,
            priority: 50,
        };

        let mod_data = ModData {
            active_profile: "default".to_string(),
            profiles: [(
                "default".to_string(),
                ModProfile {
                    mods: vec![
                        ModOrGroup::Individual(mod_1),
                        ModOrGroup::Group {
                            group_name: "mg1".to_string(),
                            enabled: true,
                        },
                    ],
                },
            )]
            .into(),
            groups: [(
                "mg1".to_string(),
                ModGroup {
                    mods: vec![mod_2, mod_3],
                },
            )]
            .into(),
        };

        let any_required = mod_data.any_mod("default", |mc, _| mc.required);
        assert!(any_required);
    }
}
