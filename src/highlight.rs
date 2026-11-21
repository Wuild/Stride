use crate::color::Color32;

const COMMAND: Color32 = Color32::from_rgb(126, 223, 199);
const STRING: Color32 = Color32::from_rgb(225, 191, 126);
const FLAG: Color32 = Color32::from_rgb(159, 187, 250);
const OPERATOR: Color32 = Color32::from_rgb(206, 164, 246);
const VARIABLE: Color32 = Color32::from_rgb(240, 157, 173);
const COMMENT: Color32 = Color32::from_rgb(127, 143, 159);
const ERROR: Color32 = Color32::from_rgb(247, 142, 150);

pub fn shell(text: &[char]) -> Vec<Option<Color32>> {
    let mut colors = vec![None; text.len()];
    let mut at = 0;
    let mut command = true;
    while at < text.len() {
        let start = at;
        let c = text[at];
        at += 1;
        let color = match c {
            ' ' | '\t' => continue,
            '#' => {
                colors[start..].fill(Some(COMMENT));
                break;
            }
            '\'' | '"' => {
                at = quoted_end(text, at, c);
                command = false;
                STRING
            }
            '|' | '&' | ';' | '(' | ')' => {
                command = true;
                OPERATOR
            }
            '<' | '>' => OPERATOR,
            '$' => {
                while at < text.len()
                    && (text[at].is_alphanumeric() || matches!(text[at], '_' | '{' | '}'))
                {
                    at += 1;
                }
                VARIABLE
            }
            _ => {
                while at < text.len()
                    && !text[at].is_whitespace()
                    && !matches!(
                        text[at],
                        '\'' | '"' | '|' | '&' | ';' | '(' | ')' | '<' | '>' | '$'
                    )
                {
                    if text[at] == '\\' && at + 1 < text.len() {
                        at += 1;
                    }
                    at += 1;
                }
                if command {
                    command = !matches!(text[start..at].iter().position(|c| *c == '='), None);
                    if command { VARIABLE } else { COMMAND }
                } else if c == '-' {
                    FLAG
                } else {
                    continue;
                }
            }
        };
        colors[start..at].fill(Some(color));
    }
    colors
}

fn quoted_end(text: &[char], mut i: usize, quote: char) -> usize {
    while i < text.len() {
        let c = text[i];
        i += 1;
        if c == quote {
            break;
        }
        if quote == '"' && c == '\\' && i < text.len() {
            i += 1;
        }
    }
    i
}

/// Highlights recognizable structured text. The caller preserves application-supplied colors.
pub fn output(text: &[char]) -> Vec<Option<Color32>> {
    let mut colors = vec![None; text.len()];
    let first = text.iter().position(|c| !c.is_whitespace());
    let json = first.is_some_and(|i| match text[i] {
        '{' | '[' => text.iter().any(|c| *c == '"' || *c == ',' || *c == '{'),
        '"' => text.iter().any(|c| *c == ':'),
        _ => false,
    });
    let mut at = 0;
    while at < text.len() {
        let start = at;
        let c = text[at];
        at += 1;
        let color = match c {
            '"' if json => {
                at = quoted_end(text, at, c);
                if text[at..].iter().find(|c| !c.is_whitespace()) == Some(&':') {
                    FLAG
                } else {
                    STRING
                }
            }
            '0'..='9' | '-' if json => {
                while at < text.len()
                    && (text[at].is_ascii_digit()
                        || matches!(text[at], '.' | 'e' | 'E' | '+' | '-'))
                {
                    at += 1;
                }
                VARIABLE
            }
            '{' | '}' | '[' | ']' | ':' | ',' if json => OPERATOR,
            c if c.is_ascii_alphabetic() => {
                while at < text.len() && text[at].is_ascii_alphabetic() {
                    at += 1;
                }
                let word: String = text[start..at].iter().collect();
                let level = first == Some(start)
                    || text.get(start.wrapping_sub(1)) == Some(&'[')
                    || text.get(at) == Some(&':');
                match word.as_str() {
                    "error" | "ERROR" | "fatal" | "FATAL" | "FAILED" if level => ERROR,
                    "warning" | "WARN" | "WARNING" if level => STRING,
                    "info" | "INFO" | "SUCCESS" if level => COMMAND,
                    "debug" | "DEBUG" | "TRACE" if level => COMMENT,
                    "true" | "false" | "null" if json => VARIABLE,
                    _ => continue,
                }
            }
            _ => continue,
        };
        colors[start..at].fill(Some(color));
    }
    colors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_shell_command_flags_quotes_and_pipeline() {
        let text: Vec<_> = "rg -i 'needle' | sort".chars().collect();
        let colors = shell(&text);
        for (token, color) in [
            ("rg", COMMAND),
            ("-i", FLAG),
            ("'needle'", STRING),
            ("|", OPERATOR),
            ("sort", COMMAND),
        ] {
            let start = text.iter().collect::<String>().find(token).unwrap();
            assert!(
                colors[start..start + token.len()]
                    .iter()
                    .all(|value| *value == Some(color))
            );
        }
    }

    #[test]
    fn colors_json_and_log_markers_but_leaves_prose_alone() {
        let colors = output(&r#""ok": true"#.chars().collect::<Vec<_>>());
        assert_eq!(colors[0], Some(FLAG));
        assert_eq!(colors[6], Some(VARIABLE));
        assert_eq!(
            output(&"[ERROR] failed".chars().collect::<Vec<_>>())[1],
            Some(ERROR)
        );
        assert!(
            output(&"plain text".chars().collect::<Vec<_>>())
                .iter()
                .all(Option::is_none)
        );
    }
}
