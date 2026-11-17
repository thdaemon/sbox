//! Load and merge layered TOML documents into one typed value.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::path::Path;

/// Reads TOML files, merges them in order, and deserializes the result once.
///
/// Tables merge by key, the keys named in append concatenate their arrays,
/// and everything else is replaced by the later layer. A missing file is an
/// empty layer, so the standard optional files need no existence check.
pub(super) struct ConfigLoader<'a> {
    append: &'a [&'a str],
    merged: toml::Value,
}

impl<'a> ConfigLoader<'a> {
    /// A loader whose layers append the arrays under the given keys.
    pub fn new(append: &'a [&'a str]) -> ConfigLoader<'a> {
        ConfigLoader {
            append,
            merged: toml::Value::Table(Default::default()),
        }
    }

    /// Read and merge one file.
    pub fn layer(mut self, path: &Path) -> Result<ConfigLoader<'a>> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;

            let overlay: toml::Value =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

            Self::merge(&mut self.merged, overlay, "", self.append);
        }

        Ok(self)
    }

    /// Merge an overlay into a base: tables recurse, the keys in append append,
    /// everything else replaces. The path is the dotted path of the base, used
    /// only to decide appending.
    fn merge(base: &mut toml::Value, overlay: toml::Value, path: &str, append: &[&str]) {
        match (base, overlay) {
            (toml::Value::Table(base), toml::Value::Table(overlay)) => {
                for (key, value) in overlay {
                    let child = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };

                    if let Some(slot) = base.get_mut(&key) {
                        Self::merge(slot, value, &child, append);
                    } else {
                        base.insert(key, value);
                    }
                }
            }
            (toml::Value::Array(base), toml::Value::Array(overlay)) => {
                if append.contains(&path) {
                    base.extend(overlay);
                } else {
                    *base = overlay;
                }
            }
            (base, overlay) => *base = overlay,
        }
    }

    /// Deserialize the merged layers into the final value.
    pub fn finish<T: DeserializeOwned>(self) -> Result<T> {
        self.merged
            .try_into()
            .context("deserializing the merged configuration")
    }
}

#[cfg(test)]
mod tests {
    use crate::config::ConfigLoader;

    const APPEND: &[&str] = &["bind", "dbus.session", "dbus.system"];

    fn merged(base: &str, overlay: &str) -> toml::Value {
        let mut base: toml::Value = toml::from_str(base).expect("base");
        let overlay: toml::Value = toml::from_str(overlay).expect("overlay");
        ConfigLoader::merge(&mut base, overlay, "", APPEND);
        base
    }

    #[test]
    fn tables_merge_by_key() {
        let value = merged(
            "[shares]\nx11 = true\nwayland = true\n",
            "[shares]\nx11 = false\n",
        );
        let shares = value.get("shares").expect("shares");
        assert_eq!(shares.get("x11").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(shares.get("wayland").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn the_named_arrays_append() {
        let value = merged(
            "[[bind]]\nsource = \"/one\"\n[dbus]\nsession = [\"talk=a\"]\n",
            "[[bind]]\nsource = \"/two\"\n[dbus]\nsession = [\"talk=b\"]\n",
        );
        assert_eq!(
            value.get("bind").and_then(|v| v.as_array()).map(Vec::len),
            Some(2)
        );
        let session = value
            .get("dbus")
            .and_then(|dbus| dbus.get("session"))
            .and_then(|v| v.as_array())
            .expect("session");
        assert_eq!(session.len(), 2);
    }

    #[test]
    fn other_arrays_are_replaced() {
        let value = merged("command = [\"/bin/a\"]\n", "command = [\"/bin/b\"]\n");
        let command = value
            .get("command")
            .and_then(|v| v.as_array())
            .expect("command");
        assert_eq!(command.len(), 1);
        assert_eq!(command[0].as_str(), Some("/bin/b"));
    }
}
