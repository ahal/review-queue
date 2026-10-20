//! Configurable actions for opening reviews, with validated TUI shortcuts.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use crate::config::TargetConfig;

#[derive(Debug, Clone)]
pub enum Action {
    Browser,
    Workspace,
    Command {
        command: String,
        workspace: bool,
        wait: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Target {
    pub id: String,
    pub description: String,
    pub key: Option<char>,
    pub action: Action,
}

pub fn resolve(cfg: &BTreeMap<String, TargetConfig>) -> Result<Vec<Target>> {
    let mut targets = vec![
        Target {
            id: "browser".into(),
            description: "Open in browser".into(),
            key: Some('o'),
            action: Action::Browser,
        },
        Target {
            id: "workspace".into(),
            description: "Open in workspace".into(),
            key: Some('w'),
            action: Action::Workspace,
        },
    ];
    for (id, config) in cfg {
        let target = if let Some(idx) = targets.iter().position(|t| t.id == *id) {
            if config.has_command_options {
                bail!("built-in target `{id}` only allows description and key");
            }
            &mut targets[idx]
        } else {
            let Some(command) = &config.command else {
                bail!("target `{id}` requires command");
            };
            targets.push(Target {
                id: id.clone(),
                description: id.clone(),
                key: None,
                action: Action::Command {
                    command: command.clone(),
                    workspace: config.workspace,
                    wait: config.wait,
                },
            });
            targets.last_mut().unwrap()
        };
        if let Some(description) = &config.description {
            target.description.clone_from(description);
        }
        if let Some(key) = &config.key {
            let mut chars = key.chars();
            target.key = chars.next();
            if chars.next().is_some() || target.key.is_some_and(char::is_control) {
                bail!("target `{id}` key must be a single printable character or empty");
            }
        }
    }
    let mut keys = BTreeSet::new();
    for target in &targets {
        if let Some(key) = target.key {
            if "jkhlqdfr?".contains(key) {
                bail!("target `{}` key `{key}` is reserved", target.id);
            }
            if !keys.insert(key) {
                bail!("duplicate target key `{key}`");
            }
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn configured(text: &str) -> Result<Vec<Target>> {
        resolve(&toml::from_str::<Config>(text).unwrap().targets)
    }

    #[test]
    fn defaults() {
        let targets = resolve(&BTreeMap::new()).unwrap();
        assert_eq!(
            targets.iter().map(|t| t.key).collect::<Vec<_>>(),
            [Some('o'), Some('w')]
        );
    }

    #[test]
    fn overrides_and_order() {
        let targets = configured(
            r#"
[targets.z]
command = "z"
[targets.a]
command = "a"
[targets.browser]
key = "b"
description = "Browser"
[targets.workspace]
key = ""
"#,
        )
        .unwrap();
        assert_eq!(
            targets.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["browser", "workspace", "a", "z"]
        );
        assert_eq!(targets[0].key, Some('b'));
        assert_eq!(targets[0].description, "Browser");
        assert_eq!(targets[1].key, None);
    }

    #[test]
    fn invalid_targets() {
        for text in [
            "[targets.browser]\nkey = 'w'",
            "[targets.browser]\nkey = 'j'",
            "[targets.browser]\nkey = 'ab'",
            "[targets.custom]\nkey = 'v'",
            "[targets.browser]\ncommand = 'echo'",
            "[targets.workspace]\nworkspace = true",
            "[targets.browser]\nwait = true",
            "[targets.browser]\nkey = '\\n'",
        ] {
            assert!(configured(text).is_err(), "{text}");
        }
    }
}
