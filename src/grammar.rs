use std::{ffi::OsString, fs};

use clap::{Arg, ArgAction, Args, Command, ValueEnum, builder::ValueRange};
use serde_json::{Map, Value};

use crate::error::{Result, parameter};

const CLI_INPUT_JSON: &str = "cli-input-json";
const GENERATE_CLI_SKELETON: &str = "generate-cli-skeleton";

#[derive(Args)]
pub struct CliInputArgs {
    /// Read the parameters from a JSON object keyed by the PascalCase parameter names. Parameters on the command line take precedence
    #[arg(long, value_name = "JSON")]
    cli_input_json: Option<String>,
    /// Print a JSON skeleton of the input parameters (input, default) or of the output (output) and exit
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "input", value_name = "input|output")]
    generate_cli_skeleton: Option<SkeletonKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SkeletonKind {
    Input,
    Output,
}

pub struct Prepared {
    pub argv: Vec<String>,
    pub path: Vec<String>,
    pub skeleton: Option<SkeletonKind>,
}

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

fn is_negation(command: &Command, arg: &Arg) -> bool {
    arg.get_long()
        .and_then(|long| long.strip_prefix("no-"))
        .is_some_and(|positive| find_long(command, positive).is_some())
}

fn is_parameter(command: &Command, arg: &Arg) -> bool {
    !is_negation(command, arg)
        && !arg.is_global_set()
        && !matches!(
            arg.get_id().as_str(),
            "help" | "version" | "cli_input_json" | "generate_cli_skeleton"
        )
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

/// Expands `file://` values, rewrites a trailing `help` on an operation into `--help`, and merges `--cli-input-json` into the argument list.
pub fn prepare(root: &Command, raw: Vec<OsString>) -> Result<Prepared> {
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
    let mut cli_input_json = None;
    let mut skeleton = None;
    let mut index = 1;
    while index < argv.len() {
        let token = argv[index].clone();
        if token == "--" {
            break;
        }
        if let Some(name) = token.strip_prefix("--") {
            let (name, inline) = match name.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (name, None),
            };
            let mut values = Vec::new();
            if let Some(arg) = find_long(command, name) {
                let range = value_range(arg);
                if let Some(value) = inline {
                    values.push(value);
                } else if range.takes_values() {
                    while index + 1 < argv.len()
                        && values.len() < range.max_values()
                        && !argv[index + 1].starts_with("--")
                        && (range.min_values() > 0
                            || arg
                                .get_possible_values()
                                .iter()
                                .any(|value| value.matches(&argv[index + 1], false)))
                    {
                        index += 1;
                        values.push(argv[index].clone());
                    }
                }
            }
            match name {
                CLI_INPUT_JSON => cli_input_json = values.into_iter().next(),
                GENERATE_CLI_SKELETON => {
                    let value = values.into_iter().next();
                    skeleton = Some(match value.as_deref() {
                        None | Some("input") => SkeletonKind::Input,
                        Some("output") => SkeletonKind::Output,
                        Some(other) => {
                            return Err(parameter(format!(
                                "--generate-cli-skeleton must be input or output, not {other}"
                            )));
                        }
                    });
                }
                _ => {}
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

    if let Some(input) = cli_input_json.filter(|_| skeleton.is_none()) {
        let extra = cli_input_tokens(command, &argv, &input)?;
        argv.extend(extra);
    }
    Ok(Prepared {
        argv,
        path,
        skeleton,
    })
}

fn cli_input_tokens(command: &Command, argv: &[String], input: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(input)
        .map_err(|error| parameter(format!("--cli-input-json is not valid JSON: {error}")))?;
    let Value::Object(map) = value else {
        return Err(parameter("--cli-input-json must be a JSON object"));
    };
    let mut tokens = Vec::new();
    for (key, value) in map {
        let arg = command
            .get_arguments()
            .filter(|arg| is_parameter(command, arg))
            .find(|arg| arg.get_long().map(pascal_case).as_deref() == Some(key.as_str()))
            .ok_or_else(|| parameter(format!("Unknown parameter in --cli-input-json: {key}")))?;
        let long = arg.get_long().unwrap_or_default();
        let on_command_line = argv.iter().any(|token| {
            token == &format!("--{long}")
                || token.starts_with(&format!("--{long}="))
                || token == &format!("--no-{long}")
        });
        if on_command_line {
            continue;
        }
        if !value_range(arg).takes_values() {
            match value {
                Value::Bool(true) => tokens.push(format!("--{long}")),
                Value::Bool(false) => {
                    if find_long(command, &format!("no-{long}")).is_some() {
                        tokens.push(format!("--no-{long}"));
                    }
                }
                _ => {
                    return Err(parameter(format!(
                        "--cli-input-json: {key} must be true or false"
                    )));
                }
            }
            continue;
        }
        let items = match value {
            Value::Null => Vec::new(),
            Value::Array(items) => items,
            value => vec![value],
        };
        for item in items {
            let rendered = match item {
                Value::String(string) => string,
                item => item.to_string(),
            };
            tokens.push(format!("--{long}={rendered}"));
        }
    }
    Ok(tokens)
}

pub fn input_skeleton(command: &Command) -> Value {
    let mut map = Map::new();
    for arg in command
        .get_arguments()
        .filter(|arg| is_parameter(command, arg))
    {
        let Some(long) = arg.get_long() else {
            continue;
        };
        let range = value_range(arg);
        let sample = if !range.takes_values() {
            Value::Bool(false)
        } else {
            let default = arg
                .get_default_values()
                .first()
                .map(|value| value.to_string_lossy().into_owned());
            let possible = arg
                .get_possible_values()
                .first()
                .map(|value| value.get_name().to_string());
            Value::String(default.or(possible).unwrap_or_default())
        };
        let multiple = range.max_values() > 1 || matches!(arg.get_action(), ArgAction::Append);
        map.insert(
            pascal_case(long),
            if multiple && range.takes_values() {
                Value::Array(vec![sample])
            } else {
                sample
            },
        );
    }
    Value::Object(map)
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
