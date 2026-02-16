use std::{fs, path::Path};

use serde_json::{Value, json};

use crate::{
    config,
    errors::{AppError, Result},
};

pub fn install_hooks(binary_path: &Path) -> Result<()> {
    let settings_path = config::claude_settings_path()?;
    let Some(parent) = settings_path.parent() else {
        return Err(AppError::HomeDirUnavailable);
    };
    fs::create_dir_all(parent)
        .map_err(|_| AppError::CreateConfigDir(parent.display().to_string()))?;

    let mut root: Value = if settings_path.exists() {
        let text = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&text).map_err(|_| AppError::PluginSettingsParse)?
    } else {
        json!({})
    };

    root["hooks"] = build_hooks_json(binary_path);
    let serialized = serde_json::to_string_pretty(&root)?;
    fs::write(&settings_path, serialized)
        .map_err(|_| AppError::WriteConfig(settings_path.display().to_string()))?;
    Ok(())
}

pub fn hooks_installed() -> Result<bool> {
    let settings_path = config::claude_settings_path()?;
    if !settings_path.exists() {
        return Ok(false);
    }

    let text = fs::read_to_string(&settings_path)?;
    let parsed: Value = serde_json::from_str(&text).map_err(|_| AppError::PluginSettingsParse)?;
    Ok(parsed.get("hooks").is_some())
}

pub fn write_plugin_artifacts(binary_path: &Path) -> Result<()> {
    let data_dir = config::data_dir()?.join("plugin");
    fs::create_dir_all(&data_dir)
        .map_err(|_| AppError::CreateConfigDir(data_dir.display().to_string()))?;

    let plugin_json = json!({
        "name": "codelatch",
        "description": "Remote supervision for Claude Code via Telegram",
        "version": "0.1.0",
        "author": { "name": "codelatch" }
    });
    let hooks_json = json!({
      "description": "Codelatch remote supervision hooks",
      "hooks": build_hooks_json(binary_path)
    });

    fs::write(
        data_dir.join("plugin.json"),
        serde_json::to_string_pretty(&plugin_json)?,
    )
    .map_err(|_| AppError::WriteConfig(data_dir.join("plugin.json").display().to_string()))?;

    fs::write(
        data_dir.join("hooks.json"),
        serde_json::to_string_pretty(&hooks_json)?,
    )
    .map_err(|_| AppError::WriteConfig(data_dir.join("hooks.json").display().to_string()))?;
    Ok(())
}

fn build_hooks_json(binary_path: &Path) -> Value {
    let bin = binary_path.display().to_string();
    json!({
      "Notification": [
        {
          "matcher": "elicitation_dialog",
          "hooks": [
            { "type": "command", "command": format!("{bin} hook Notification"), "async": true }
          ]
        },
        {
          "matcher": "permission_prompt",
          "hooks": [
            { "type": "command", "command": format!("{bin} hook Notification"), "async": true }
          ]
        }
      ],
      "PostToolUseFailure": [
        {
          "matcher": "",
          "hooks": [
            { "type": "command", "command": format!("{bin} hook PostToolUseFailure"), "async": true }
          ]
        }
      ],
      "Stop": [
        {
          "hooks": [
            { "type": "command", "command": format!("{bin} hook Stop"), "async": true }
          ]
        }
      ],
      "SessionStart": [
        {
          "hooks": [
            { "type": "command", "command": format!("{bin} hook SessionStart"), "async": true }
          ]
        }
      ],
      "SessionEnd": [
        {
          "hooks": [
            { "type": "command", "command": format!("{bin} hook SessionEnd"), "async": true }
          ]
        }
      ]
    })
}
