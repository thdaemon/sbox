//! Sandbox names.

use std::fmt;

/// A validated sandbox name. It keys the runtime socket directory and the
/// state directory, so it stays a file-name-safe token everywhere it travels
/// instead of being re-validated (or forgotten) at each step.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub(crate) struct Name(String);

impl Name {
    pub fn parse(name: &str) -> Result<Name, String> {
        if name.is_empty() {
            return Err("the sandbox name cannot be empty".into());
        }

        // The name becomes a path component under the socket and state roots,
        // so the two directory aliases would escape or point at the root itself.
        if matches!(name, "." | "..") {
            return Err(format!(
                "invalid sandbox name {name:?}: it must not be '.' or '..'"
            ));
        }

        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(format!(
                "invalid sandbox name {name:?}: use letters, digits, '.', '_' or '-'"
            ));
        }

        Ok(Name(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Name;

    #[test]
    fn plain_tokens_are_accepted() {
        for name in ["work", "a.b_c-d", "1234"] {
            assert_eq!(Name::parse(name).expect("valid").as_str(), name);
        }
    }

    #[test]
    fn anything_that_is_not_a_file_name_is_rejected() {
        for name in ["", ".", "..", "two words", "path/../x", "a\0b"] {
            assert!(Name::parse(name).is_err(), "{name:?} was accepted");
        }
    }
}
