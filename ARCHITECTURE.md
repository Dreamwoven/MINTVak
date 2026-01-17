# mint Fork Architecture

This document describes architectural changes made to the mint mod manager fork for Deep Rock Galactic.

## Overview

This fork adds several features on top of upstream mint:
- **Folder Organization** - Group mods into collapsible folders within profiles
- **Priority Override System** - Folders can override load priority for all contained mods
- **Manual Backup System** - Create timestamped backups of mod configurations
- **Deletion Confirmations** - Configurable confirmation dialogs for destructive actions

## Data Model

### Version History

The `ModData` structure uses obake versioning:

| Version | Changes |
|---------|---------|
| 0.0.0 | Original - flat mod list per profile |
| 0.1.0 | Added `ModOrGroup` enum, global `groups` map |
| 0.2.0 | **Current** - Moved `groups` into `ModProfile` (per-profile folders) |

### Current Structure (0.2.0)

```
ModData
  +-- active_profile: String
  +-- profiles: BTreeMap<String, ModProfile>
        +-- mods: Vec<ModOrGroup>
        |     +-- Individual(ModConfig)
        |     +-- Group { group_name, enabled }
        +-- groups: BTreeMap<String, ModGroup>  // Per-profile!
              +-- mods: Vec<ModConfig>
              +-- priority_override: Option<i32>
```

**Key Design Decision**: Folders are stored per-profile, not globally. This prevents:
- Cross-profile folder contamination
- Orphaned folders when profiles are deleted
- Confusion about which profile owns which folder

### Migration Path

```
0.0.0 -> 0.1.0: Wrap mods in ModOrGroup::Individual, create empty global groups
0.1.0 -> 0.2.0: Copy referenced groups from global map into each profile
```

## Folder System

### UI Components

Located in `src/gui/mod.rs`:

| Component | Purpose |
|-----------|---------|
| `create_folder_popup` | Modal for new folder name input |
| `rename_folder_popup` | Modal for renaming existing folder |
| `expand_folder: Option<String>` | Auto-expand folder after move operation |

### Operations

**Create Folder**:
1. Validate name doesn't exist in active profile
2. Insert into `profile.groups`
3. Add `ModOrGroup::Group` reference to `profile.mods`

**Delete Folder**:
1. Move all mods back to root (`profile.mods`)
2. Remove from `profile.groups`
3. Remove `ModOrGroup::Group` reference from `profile.mods`

**Move Mod to Folder**:
1. Remove `ModOrGroup::Individual` from `profile.mods`
2. Add `ModConfig` to `profile.groups[folder].mods`
3. Set `expand_folder` to show destination

**Move Mod Between Folders**:
1. Remove from source folder's mods
2. Add to destination folder's mods
3. Set `expand_folder` to show destination

### Priority Override

When `ModGroup.priority_override = Some(priority)`:
- All mods in folder use `priority` for load order
- Individual mod priority controls are grayed out
- Moving mod out preserves its original `ModConfig.priority`

Integration code in `get_enabled_mods_with_priority()`:
```rust
let effective_priority = group.priority_override.unwrap_or(mc.priority);
```

## File Reference

| File | Lines | Purpose |
|------|-------|---------|
| `src/gui/mod.rs` | ~2900 | Main GUI, folder UI, deletion dialogs |
| `src/state/mod.rs` | ~850 | Data structures, versioning, migrations |
| `src/gui/message.rs` | ~350 | Async message handling |
| `src/gui/named_combobox.rs` | ~280 | Profile selector widget |

## Debugging Lessons

### Borrow Checker in Nested UI Closures

**Problem**: Iterating `profile.mods` while also accessing `profile.groups` causes borrow conflicts.

**Solution**: Pre-collect data before closures:
```rust
let folder_names: Vec<String> = profile.groups.keys().cloned().collect();
```

Or restructure to access through profile reference passed to closure.

### CollapsingHeader Programmatic Control

**Problem**: Needed to auto-expand folders after moving mods.

**Failed Approach**: `CollapsingState::load_with_default_open().set_open(true)` with mismatched IDs.

**Working Solution**: Use `CollapsingHeader::open(Some(true))` method:
```rust
let mut header = CollapsingHeader::new(name).id_salt(folder_id).default_open(false);
if should_open {
    header = header.open(Some(true));
}
```

### Global vs Per-Profile State

**Problem**: Global `groups` map caused folders to persist after profile deletion and allowed cross-profile contamination.

**Solution**: Move `groups` into `ModProfile`. Each profile owns its folders completely.

### UTF-8 Encoding in Documentation

**Problem**: Linux container uses Latin-1, corrupting special characters in markdown.

**Solution**: Use ASCII alternatives (-> instead of arrows, -- instead of em-dashes) or edit via Python with explicit UTF-8 encoding.

## Configuration

Settings stored in config (separate from mod data):

| Setting | Default | Purpose |
|---------|---------|---------|
| `confirm_mod_deletion` | true | Show dialog before deleting mods/folders |
| `confirm_profile_deletion` | true | Show dialog before deleting profiles |
| `backup_path` | `Documents/mint_backups/` | Manual backup location |

## Build

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

Output: `target/x86_64-pc-windows-gnu/release/mint.exe` (~76 MB)
