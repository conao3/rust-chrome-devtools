use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) profiles: Vec<Profile>,
}

#[derive(Clone, Debug)]
pub(crate) struct Profile {
    pub(crate) name: String,
    pub(crate) user_data_dir: String,
}

#[derive(Default)]
pub(crate) struct ProfileBuilder {
    name: Option<String>,
    user_data_dir: Option<String>,
}

pub(crate) const CONFIG_RELATIVE_PATH: &str = ".config/chrome-devtools/config.toml";

pub(crate) const CACHE_RELATIVE_PATH: &str = ".cache/chrome-devtools";

pub(crate) const DEFAULT_PROFILE_NAME: &str = "default";

pub(crate) const DEFAULT_PROFILE_USER_DATA_DIR: &str = "~/.config/chrome-devtools/profiles/default";

pub(crate) const PROFILE_USER_DATA_DIR_PREFIX: &str = "~/.config/chrome-devtools/profiles";

pub(crate) fn load_or_create_config() -> Result<Config, String> {
    let path = config_path()?;
    if !path.exists() {
        create_default_config(&path)?;
    }

    let content = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    parse_config(&content).map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

pub(crate) fn create_default_config(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(format!("config path has no parent: {}", path.display()));
    };

    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    fs::create_dir_all(expand_home(DEFAULT_PROFILE_USER_DATA_DIR)?)
        .map_err(|error| format!("failed to create default profile user data dir: {error}"))?;
    fs::write(path, default_config_content())
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

pub(crate) fn default_config_content() -> String {
    format!("[[profiles]]\nname = \"{DEFAULT_PROFILE_NAME}\"\n")
}

pub(crate) fn parse_config(content: &str) -> Result<Config, String> {
    let mut profiles = Vec::new();
    let mut current: Option<ProfileBuilder> = None;

    for (line_number, raw_line) in content.lines().enumerate() {
        let line_number = line_number + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line == "[[profiles]]" {
            push_profile(&mut profiles, current.take(), line_number)?;
            current = Some(ProfileBuilder::default());
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {line_number}: expected key = value"));
        };
        let Some(profile) = current.as_mut() else {
            return Err(format!(
                "line {line_number}: profile fields must be inside [[profiles]]"
            ));
        };

        let key = key.trim();
        let value = value.trim();
        match key {
            "name" => profile.name = Some(parse_toml_string(value, line_number)?),
            "port" => {
                eprintln!(
                    "warning: line {line_number}: 'port' is deprecated and ignored; the daemon now picks a free port automatically"
                );
            }
            "user_data_dir" => profile.user_data_dir = Some(parse_toml_string(value, line_number)?),
            unknown => {
                return Err(format!(
                    "line {line_number}: unknown profile key: {unknown}"
                ))
            }
        }
    }

    push_profile(&mut profiles, current.take(), content.lines().count() + 1)?;

    if profiles.is_empty() {
        return Err("config must define at least one [[profiles]] entry".to_string());
    }

    Ok(Config { profiles })
}

pub(crate) fn push_profile(
    profiles: &mut Vec<Profile>,
    builder: Option<ProfileBuilder>,
    line_number: usize,
) -> Result<(), String> {
    let Some(builder) = builder else {
        return Ok(());
    };

    let name = builder
        .name
        .ok_or_else(|| format!("line {line_number}: profile is missing name"))?;
    let user_data_dir = builder
        .user_data_dir
        .unwrap_or_else(|| default_user_data_dir_for_profile(&name));

    if profiles.iter().any(|profile| profile.name == name) {
        return Err(format!("duplicate profile name: {name}"));
    }

    profiles.push(Profile {
        name,
        user_data_dir,
    });
    Ok(())
}

pub(crate) fn default_user_data_dir_for_profile(name: &str) -> String {
    format!("{PROFILE_USER_DATA_DIR_PREFIX}/{name}")
}

pub(crate) fn parse_toml_string(value: &str, line_number: usize) -> Result<String, String> {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return Err(format!("line {line_number}: expected quoted string"));
    };

    let mut parsed = String::new();
    let mut chars = inner.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            parsed.push(character);
            continue;
        }

        let Some(escaped) = chars.next() else {
            return Err(format!("line {line_number}: dangling escape in string"));
        };
        match escaped {
            '\\' => parsed.push('\\'),
            '"' => parsed.push('"'),
            'n' => parsed.push('\n'),
            'r' => parsed.push('\r'),
            't' => parsed.push('\t'),
            other => return Err(format!("line {line_number}: unsupported escape: \\{other}")),
        }
    }

    Ok(parsed)
}

pub(crate) fn config_path() -> Result<PathBuf, String> {
    let Some(home) = env::var_os("HOME") else {
        return Err("HOME is not set".to_string());
    };
    Ok(PathBuf::from(home).join(CONFIG_RELATIVE_PATH))
}

pub(crate) fn cache_dir() -> Result<PathBuf, String> {
    let Some(home) = env::var_os("HOME") else {
        return Err("HOME is not set".to_string());
    };
    Ok(PathBuf::from(home).join(CACHE_RELATIVE_PATH))
}

pub(crate) fn find_profile(config: &Config, name: &str) -> Result<Profile, String> {
    config
        .profiles
        .iter()
        .find(|profile| profile.name == name)
        .cloned()
        .ok_or_else(|| {
            let available = config
                .profiles
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "unknown profile: {name}; available: {available} (defined in ~/{CONFIG_RELATIVE_PATH})"
            )
        })
}

pub(crate) fn expand_home(path: &str) -> Result<PathBuf, String> {
    if path == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_string());
    }

    if let Some(rest) = path.strip_prefix("~/") {
        let Some(home) = env::var_os("HOME") else {
            return Err("HOME is not set".to_string());
        };
        return Ok(PathBuf::from(home).join(rest));
    }

    Ok(PathBuf::from(path))
}
