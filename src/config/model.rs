// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The configuration data types: the value types the rest of the crate shares,
//! and one sandbox's settings as they are written.

use crate::name::Name;
use anyhow::Result;
use clap::ValueEnum;
use serde::Deserialize;
use std::collections::BTreeMap;

const DEFAULT_SESSION_RULES: &[&str] = &[
    "talk=org.freedesktop.DBus",
    "talk=org.freedesktop.portal.*",
    "talk=org.gnome.SettingsDaemon.MediaKeys",
    "talk=org.kde.StatusNotifierWatcher",
    "talk=org.freedesktop.Notifications",
];

const DEFAULT_SYSTEM_RULES: &[&str] = &["talk=org.freedesktop.DBus"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(super) struct MainConfig {
    /// The directory the sandboxes are created under, one subdirectory each.
    /// Unset means ~/Sandbox.
    pub sandbox_root: Option<String>,
}

impl MainConfig {
    pub fn validate(&self) -> Result<()> {
        if let Some(root) = &self.sandbox_root {
            if root.trim().is_empty() {
                anyhow::bail!("config.toml: sandbox_root is empty");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BindMode {
    /// Read-write, mounted nosuid,nodev.
    #[default]
    Rw,
    /// Read-only, mounted ro,nosuid,nodev.
    Ro,
    /// Read-write with device access (nosuid only), for device nodes and
    /// sockets that have to stay usable, like bubblewrap's --dev-bind.
    Dev,
}

/// How ~/.pki is exposed. The variant name is also the spelling used by the
/// configuration file and by --pki, so both stay lower case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PkiMode {
    Ro,
    Rw,
    Isolate,
}

/// A [[bind]] as it is written. The destination defaults to the source and
/// the mode to read-write; the mount layer applies both when it builds the
/// tree, since that is the only place they have meaning.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Bind {
    pub source: String,
    /// None means the source path.
    pub dest: Option<String>,
    /// Defaults to read-write.
    pub mode: BindMode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Shares {
    pub x11: bool,
    pub wayland: bool,
    pub dri: bool,
    pub audio: bool,
    pub resolv_conf: bool,
    pub pki: PkiMode,
}

impl Default for Shares {
    fn default() -> Shares {
        Shares {
            x11: true,
            wayland: true,
            dri: true,
            audio: true,
            resolv_conf: true,
            pki: PkiMode::Ro,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Gui {
    pub gtk_config: bool,
    pub icons: bool,
    pub fonts: bool,
    pub fontconfig: bool,
    pub ibus: bool,
}

impl Default for Gui {
    fn default() -> Gui {
        Gui {
            gtk_config: true,
            icons: true,
            fonts: true,
            fontconfig: true,
            ibus: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Dbus {
    /// Share the raw sockets instead of filtered proxies (dangerous).
    pub raw: bool,
    pub debug: bool,
    /// The extra --{talk,own,see,call}= rules.
    pub session: Vec<String>,
    pub system: Vec<String>,
    /// Whether the built-in rules are left out when the rule list is built.
    pub no_default_rules: bool,
}

impl Dbus {
    /// The rules xdg-dbus-proxy gets for the session bus: the built-in ones
    /// unless they were turned off, then whatever the configuration added.
    pub fn session_rules(&self) -> Vec<String> {
        self.rules(DEFAULT_SESSION_RULES, &self.session)
    }

    pub fn system_rules(&self) -> Vec<String> {
        self.rules(DEFAULT_SYSTEM_RULES, &self.system)
    }

    fn rules(&self, defaults: &[&str], extra: &[String]) -> Vec<String> {
        let mut rules = Vec::new();

        if !self.no_default_rules {
            rules.extend(defaults.iter().map(|rule| rule.to_string()));
        }
        rules.extend(extra.iter().cloned());

        rules
    }
}

/// What a sandbox does about its hostname. One key, three spellings: a name
/// unshares UTS and sets it, true keeps the host's UTS namespace, false asks
/// for the derived name without leaving the key out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Hostname {
    /// Share the host's UTS namespace, so the hostname stays the host's.
    Real,
    /// Unshare UTS and set this name.
    Named(String),
    /// Unshare UTS and derive the name from the host and the sandbox. The
    /// namespace layer turns it into the derived name when it sets the UTS
    /// namespace up.
    #[default]
    Auto,
}

impl Hostname {
    /// What the sandbox's uts namespace should be called, given the host's own
    /// name. Auto is the only variant that needs one, and this is the only
    /// place the derived form is spelled out; reading the host's name is the
    /// caller's job, since only it knows when that is still visible.
    pub(crate) fn resolve(&self, host: &str, sandbox: &Name) -> Hostname {
        match self {
            Hostname::Auto => Hostname::Named(format!("{host}-sbox-{sandbox}")),
            hostname => hostname.clone(),
        }
    }

    /// The name to set, when the sandbox owns its UTS namespace.
    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            Hostname::Named(name) => Some(name),
            Hostname::Real | Hostname::Auto => None,
        }
    }
}

impl<'de> Deserialize<'de> for Hostname {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Hostname, D::Error> {
        /// One key, two shapes: a string names the sandbox, a flag keeps or
        /// derives the name. A flag is not a name, so both spellings land in
        /// the same enum.
        struct Spelling;

        impl serde::de::Visitor<'_> for Spelling {
            type Value = Hostname;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a hostname, true for the host's, or false for the derived one")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Hostname, E> {
                Ok(Hostname::Named(value.to_string()))
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Hostname, E> {
                Ok(if value {
                    Hostname::Real
                } else {
                    Hostname::Auto
                })
            }
        }

        deserializer.deserialize_any(Spelling)
    }
}

/// One [env] entry: a string sets the variable, false removes it. The two
/// spellings mirror hostname's, so a key reads the same way whether it names a
/// value or asks for no value at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnvValue {
    Value(String),
    Remove,
}

impl<'de> Deserialize<'de> for EnvValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<EnvValue, D::Error> {
        /// One key, two shapes: a string is the value to set, false means the
        /// variable is removed instead.
        struct Spelling;

        impl serde::de::Visitor<'_> for Spelling {
            type Value = EnvValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a value to set, or false to remove the variable")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<EnvValue, E> {
                Ok(EnvValue::Value(value.to_string()))
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<EnvValue, E> {
                if value {
                    return Err(E::custom(
                        "true is not a value: write a string to set this variable, or false to remove it",
                    ));
                }
                Ok(EnvValue::Remove)
            }
        }

        deserializer.deserialize_any(Spelling)
    }
}

/// One sandbox's settings as the files and the command line describe them.
/// Defaults that depend on the invocation -- the derived hostname, the sandbox
/// directory, the built-in dbus rules -- are applied by the layer that uses
/// them, not here.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SandboxConfig {
    pub hostname: Hostname,
    /// The in-sandbox $HOME as written; `~` is expanded when it is used.
    pub dir: String,
    pub command: Vec<String>,
    pub nested: bool,
    pub fakeroot: bool,
    pub env: BTreeMap<String, EnvValue>,
    pub shares: Shares,
    pub gui: Gui,
    pub dbus: Dbus,
    #[serde(rename = "bind", default)]
    pub binds: Vec<Bind>,
}

impl Default for SandboxConfig {
    fn default() -> SandboxConfig {
        SandboxConfig {
            hostname: Hostname::Auto,
            dir: String::new(),
            command: default_command(),
            nested: false,
            fakeroot: false,
            env: BTreeMap::new(),
            shares: Shares::default(),
            gui: Gui::default(),
            dbus: Dbus::default(),
            binds: Vec::new(),
        }
    }
}

fn default_command() -> Vec<String> {
    vec![
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()),
        "-l".into(),
    ]
}

impl SandboxConfig {
    /// Reject rules that xdg-dbus-proxy would silently ignore.
    pub fn validate(&self) -> Result<()> {
        self.dbus
            .session
            .iter()
            .chain(self.dbus.system.iter())
            .try_for_each(|rule| Self::check_dbus_rules(rule))
    }

    /// The one place that decides what a dbus rule may look like. The command line
    /// and the configuration file both come through here.
    fn check_dbus_rules(rule: &str) -> Result<()> {
        let (kind, target) = rule.split_once('=').ok_or_else(|| {
            anyhow::anyhow!(
                "expected KIND=RULE, for example talk=org.freedesktop.DBus, got {rule:?}"
            )
        })?;
        if !matches!(kind, "talk" | "own" | "see" | "call") {
            anyhow::bail!("unknown dbus rule kind {kind:?}: expected talk, own, see or call");
        }
        if target.is_empty() {
            anyhow::bail!("dbus rule {rule:?} has no target");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SESSION_RULES, Dbus, EnvValue, Hostname, SandboxConfig};
    use crate::name::Name;

    #[test]
    fn auto_resolves_to_the_derived_name() {
        let name = Name::parse("work").expect("name");
        assert_eq!(
            Hostname::Auto.resolve("host", &name),
            Hostname::Named("host-sbox-work".into())
        );
    }

    #[test]
    fn a_named_hostname_and_the_host_flag_are_kept() {
        let name = Name::parse("work").expect("name");
        assert_eq!(
            Hostname::Named("chosen".into()).resolve("host", &name),
            Hostname::Named("chosen".into())
        );
        assert_eq!(Hostname::Real.resolve("host", &name), Hostname::Real);
    }

    #[test]
    fn an_env_entry_sets_or_removes() {
        let config: SandboxConfig =
            toml::from_str("[env]\nFOO = \"bar\"\nBAR = false\n").expect("toml");
        assert_eq!(config.env.get("FOO"), Some(&EnvValue::Value("bar".into())));
        assert_eq!(config.env.get("BAR"), Some(&EnvValue::Remove));
    }

    #[test]
    fn true_is_not_an_env_value() {
        let error = toml::from_str::<SandboxConfig>("[env]\nFOO = true\n").expect_err("rejected");
        assert!(error.to_string().contains("false"), "{error}");
    }

    #[test]
    fn dbus_rules_put_defaults_first() {
        let mut dbus = Dbus::default();
        dbus.session.push("talk=org.example".into());

        let rules = dbus.session_rules();
        assert_eq!(rules.len(), DEFAULT_SESSION_RULES.len() + 1);
        assert_eq!(
            rules.first().map(String::as_str),
            Some(DEFAULT_SESSION_RULES[0])
        );
        assert_eq!(rules.last().map(String::as_str), Some("talk=org.example"));
    }

    #[test]
    fn dbus_rules_can_skip_defaults() {
        let mut dbus = Dbus {
            no_default_rules: true,
            ..Dbus::default()
        };
        dbus.system.push("own=org.example".into());

        assert_eq!(dbus.system_rules(), ["own=org.example".to_string()]);
    }
}
