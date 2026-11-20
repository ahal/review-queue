//! Configurable actions for opening reviews, with validated TUI shortcuts.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use crate::config::TargetConfig;

#[derive(Debug, Clone)]
pub enum Action {
    Browser,
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
    let mut targets = vec![Target {
        id: "browser".into(),
        description: "Open in browser".into(),
        key: Some('o'),
        action: Action::Browser,
    }];
    for (id, config) in cfg {
        let target = if let Some(idx) = targets.iter().position(|t| t.id == *id) {
            if config.has_command_options {
                bail!("built-in target `{id}` only allows description and key");
            }
            &mut targets[idx]
        } else {
            let Some(command) = &config.command else {
                if id == "workspace" {
                    bail!(
                        "the built-in `workspace` target was removed; give `[targets.workspace]` \
                         a command (e.g. a tmux popup), see README"
                    );
                }
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
            [Some('o')]
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
command = "tmux display-popup -E"
key = "w"
"#,
        )
        .unwrap();
        assert_eq!(
            targets.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["browser", "a", "workspace", "z"]
        );
        assert_eq!(targets[0].key, Some('b'));
        assert_eq!(targets[0].description, "Browser");
        assert_eq!(targets[2].key, Some('w'));
        assert!(matches!(
            targets[2].action,
            Action::Command {
                workspace: true,
                ..
            }
        ));
    }

    #[test]
    fn old_workspace_builtin_requires_migration() {
        let err = configured("[targets.workspace]\nkey = 'w'").unwrap_err();
        assert!(
            err.to_string()
                .contains("built-in `workspace` target was removed")
        );
    }

    #[test]
    fn invalid_targets() {
        for text in [
            "[targets.browser]\nkey = 'j'",
            "[targets.browser]\nkey = 'ab'",
            "[targets.custom]\nkey = 'v'",
            "[targets.browser]\ncommand = 'echo'",
            "[targets.workspace]\nworkspace = true",
            "[targets.x]\ncommand = 'x'\nkey = 'o'",
            "[targets.browser]\nwait = true",
            "[targets.browser]\nkey = '\\n'",
        ] {
            assert!(configured(text).is_err(), "{text}");
        }
    }
}
