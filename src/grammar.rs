use std::{ffi::OsString, fs};

use clap::{Arg, Command, builder::ValueRange};

use crate::error::{Result, parameter};

pub fn pascal_case(kebab: &str) -> String {
    kebab
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect()
}

fn expand_file_value(token: &str) -> Result<String> {
    let (prefix, value) = match token.split_once('=') {
        Some((name, value)) if name.starts_with("--") => (format!("{name}="), value),
        _ => (String::new(), token),
    };
    match value.strip_prefix("file://") {
        Some(path) => {
            let content = fs::read_to_string(path).map_err(|error| {
                parameter(format!(
                    "Error parsing parameter: Unable to load paramfile {value}: {error}"
                ))
            })?;
            Ok(format!("{prefix}{content}"))
        }
        None => Ok(token.to_string()),
    }
}

fn find_long<'a>(command: &'a Command, name: &str) -> Option<&'a Arg> {
    command
        .get_arguments()
        .find(|arg| arg.get_long() == Some(name))
}

fn value_range(arg: &Arg) -> ValueRange {
    arg.get_num_args().unwrap_or_else(|| {
        if arg.get_action().takes_values() {
            ValueRange::SINGLE
        } else {
            ValueRange::EMPTY
        }
    })
}

/// Expands `file://` values and rewrites a trailing `help` on an operation into `--help`.
pub fn prepare(root: &Command, raw: Vec<OsString>) -> Result<Vec<String>> {
    let mut argv = Vec::new();
    for token in raw {
        let token = token
            .into_string()
            .map_err(|token| parameter(format!("argument is not UTF-8: {token:?}")))?;
        argv.push(if argv.is_empty() {
            token
        } else {
            expand_file_value(&token)?
        });
    }

    let mut command = root;
    let mut path = Vec::new();
    let mut index = 1;
    while index < argv.len() {
        let token = argv[index].clone();
        if token == "--" {
            break;
        }
        if let Some(name) = token.strip_prefix("--") {
            if let Some(arg) = find_long(command, name) {
                let range = value_range(arg);
                let mut taken = 0;
                while range.takes_values()
                    && index + 1 < argv.len()
                    && taken < range.max_values()
                    && !argv[index + 1].starts_with("--")
                    && (range.min_values() > 0
                        || arg
                            .get_possible_values()
                            .iter()
                            .any(|value| value.matches(&argv[index + 1], false)))
                {
                    index += 1;
                    taken += 1;
                }
            }
        } else if !token.starts_with('-') {
            if let Some(subcommand) = command.find_subcommand(&token) {
                command = subcommand;
                path.push(token);
            } else if token == "help" && !command.has_subcommands() && !path.is_empty() {
                argv[index] = "--help".to_string();
            }
        }
        index += 1;
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_case_joins_kebab_words() {
        assert_eq!(pascal_case("manifest-file"), "ManifestFile");
        assert_eq!(pascal_case("cc"), "Cc");
    }
}
