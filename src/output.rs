use clap::ValueEnum;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::error::{Result, other};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Json,
    Text,
    Table,
}

pub struct Response {
    pub value: Value,
    pub text: Option<String>,
}

pub fn render(title: &str, response: Response, format: OutputFormat) -> Result<String> {
    Ok(match format {
        OutputFormat::Json => {
            let mut out = serde_json::to_string_pretty(&response.value)
                .map_err(|error| other(error.to_string()))?;
            out.push('\n');
            out
        }
        OutputFormat::Text => match response.text {
            Some(text) => text,
            None => {
                let mut out = String::new();
                text(&response.value, None, &mut out);
                out
            }
        },
        OutputFormat::Table => {
            let mut out = String::new();
            table(title, &response.value, &mut out);
            out
        }
    })
}

fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(string) => string.clone(),
        other => other.to_string(),
    }
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Array(_) | Value::Object(_))
}

fn line(label: Option<&str>, values: Vec<String>, out: &mut String) {
    let mut fields = Vec::new();
    if let Some(label) = label {
        fields.push(label.to_uppercase());
    }
    fields.extend(values);
    out.push_str(&fields.join("\t"));
    out.push('\n');
}

fn text(value: &Value, label: Option<&str>, out: &mut String) {
    match value {
        Value::Object(map) => {
            let scalars: Vec<String> = map
                .values()
                .filter(|value| is_scalar(value))
                .map(scalar)
                .collect();
            if !scalars.is_empty() {
                line(label, scalars, out);
            }
            for (key, value) in map.iter().filter(|(_, value)| !is_scalar(value)) {
                text(value, Some(key), out);
            }
        }
        Value::Array(items) if items.iter().all(is_scalar) => {
            if !items.is_empty() {
                line(label, items.iter().map(scalar).collect(), out);
            }
        }
        Value::Array(items) => {
            for item in items {
                text(item, label, out);
            }
        }
        value => line(label, vec![scalar(value)], out),
    }
}

fn cell_lines(value: &Value) -> Vec<String> {
    let rendered = match value {
        Value::Array(items) => items.iter().map(scalar).collect::<Vec<_>>().join("\n"),
        value => scalar(value),
    };
    let rendered = rendered.replace('\t', "    ");
    let lines: Vec<String> = rendered.lines().map(str::to_string).collect();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn pad(value: &str, width: usize) -> String {
    format!("{value}{}", " ".repeat(width.saturating_sub(value.width())))
}

fn table(title: &str, value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let rows: Vec<(String, Vec<String>)> = map
                .iter()
                .filter(|(_, value)| {
                    is_scalar(value)
                        || matches!(value, Value::Array(items) if items.iter().all(is_scalar))
                })
                .map(|(key, value)| (key.clone(), cell_lines(value)))
                .collect();
            let key_width = rows.iter().map(|(key, _)| key.width()).max().unwrap_or(0);
            let value_width = rows
                .iter()
                .flat_map(|(_, lines)| lines.iter().map(|line| line.width()))
                .max()
                .unwrap_or(0);
            let inner = (key_width + value_width + 5).max(title.width() + 2);
            let value_width = inner - key_width - 5;
            out.push_str(&format!("{}\n", "-".repeat(inner + 2)));
            let left = (inner - title.width()) / 2;
            out.push_str(&format!(
                "|{}{}{}|\n",
                " ".repeat(left),
                title,
                " ".repeat(inner - title.width() - left)
            ));
            let border = format!(
                "+{}+{}+\n",
                "-".repeat(key_width + 2),
                "-".repeat(value_width + 2)
            );
            if !rows.is_empty() {
                out.push_str(&border);
                for (key, lines) in &rows {
                    for (index, line) in lines.iter().enumerate() {
                        let key = if index == 0 { key.as_str() } else { "" };
                        out.push_str(&format!(
                            "| {} | {} |\n",
                            pad(key, key_width),
                            pad(line, value_width)
                        ));
                    }
                }
                out.push_str(&border);
            }
            for (key, value) in map.iter().filter(|(_, value)| match value {
                Value::Object(_) => true,
                Value::Array(items) => !items.iter().all(is_scalar),
                _ => false,
            }) {
                table(key, value, out);
            }
        }
        Value::Array(items) if !items.iter().all(is_scalar) => {
            for item in items {
                table(title, item, out);
            }
        }
        value => {
            let lines = cell_lines(value);
            let width = lines
                .iter()
                .map(|line| line.width())
                .max()
                .unwrap_or(0)
                .max(title.width());
            out.push_str(&format!("{}\n", "-".repeat(width + 4)));
            out.push_str(&format!("| {} |\n", pad(title, width)));
            out.push_str(&format!("+{}+\n", "-".repeat(width + 2)));
            for line in lines {
                out.push_str(&format!("| {} |\n", pad(&line, width)));
            }
            out.push_str(&format!("+{}+\n", "-".repeat(width + 2)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_puts_scalars_on_one_line_and_lists_under_their_key() {
        let value = json!({
            "Channel": "email",
            "Sealed": false,
            "Attachments": [{"Path": "a.pdf", "Bytes": 3}],
            "To": ["a@example.com", "b@example.com"],
        });
        let mut out = String::new();
        text(&value, None, &mut out);
        assert_eq!(
            out,
            "email\tFalse\nATTACHMENTS\ta.pdf\t3\nTO\ta@example.com\tb@example.com\n"
        );
    }

    #[test]
    fn table_aligns_wide_characters() {
        let mut out = String::new();
        table("Review", &json!({"Body": "本文\nab"}), &mut out);
        let widths: Vec<usize> = out.lines().map(UnicodeWidthStr::width).collect();
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]), "{out}");
    }
}
