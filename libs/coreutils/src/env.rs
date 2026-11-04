//! Shell variables and the environment.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Most bytes all variables together may hold. The kernel heap is a fixed
/// arena, so `x=$x$x` in a loop has to fail with an error, not exhaust it.
pub const MAX_ENV_BYTES: usize = 4 * 1024 * 1024;

/// One variable and its attributes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Var {
    /// The value.
    pub value: String,
    /// True when the variable is passed to commands.
    pub exported: bool,
    /// True when `readonly` has locked it.
    pub readonly: bool,
}

/// Why a variable could not be set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarError {
    /// The variable is read-only.
    ReadOnly,
    /// The value does not fit the environment budget or the heap.
    NoSpace,
}

impl VarError {
    /// A short human-readable description.
    pub const fn text(self) -> &'static str {
        match self {
            Self::ReadOnly => "readonly variable",
            Self::NoSpace => "out of variable space",
        }
    }
}

/// An ordered set of shell/environment variables.
#[derive(Clone, Debug, Default)]
pub struct Env {
    vars: BTreeMap<String, Var>,
    bytes: usize,
}

impl Env {
    /// Creates an empty environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the default FEOS environment.
    pub fn feos() -> Self {
        let mut env = Self::new();
        for (name, value) in [
            ("HOME", "/"),
            ("PWD", "/"),
            ("OLDPWD", "/"),
            ("USER", "feos"),
            ("LOGNAME", "feos"),
            ("HOSTNAME", "feos"),
            ("SHELL", "/bin/sh"),
            ("PATH", "/bin:/usr/bin:/"),
            ("TERM", "feos"),
            ("LANG", "C"),
        ] {
            env.set(name, value);
            env.export(name);
        }
        env.set("PS1", "feos:\\w\\$ ");
        env
    }

    /// Returns a variable value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(|var| var.value.as_str())
    }

    /// Sets a variable, ignoring `readonly`. The value is dropped when it does
    /// not fit the budget; use [`Env::try_set`] to learn about that.
    pub fn set(&mut self, name: &str, value: &str) {
        let _ = self.store(name, value);
    }

    /// Sets a variable unless it is read-only or the value does not fit.
    pub fn try_set(&mut self, name: &str, value: &str) -> Result<(), VarError> {
        if self.is_readonly(name) {
            return Err(VarError::ReadOnly);
        }
        self.store(name, value)
    }

    fn store(&mut self, name: &str, value: &str) -> Result<(), VarError> {
        let old = self.vars.get(name).map_or(0, |var| var.value.len());
        let new_bytes = (self.bytes + value.len()).saturating_sub(old);
        if new_bytes > MAX_ENV_BYTES {
            return Err(VarError::NoSpace);
        }
        let mut copy = String::new();
        copy.try_reserve_exact(value.len())
            .map_err(|_| VarError::NoSpace)?;
        copy.push_str(value);
        self.bytes = new_bytes;
        match self.vars.get_mut(name) {
            Some(var) => var.value = copy,
            None => {
                self.vars.insert(
                    name.to_string(),
                    Var {
                        value: copy,
                        ..Var::default()
                    },
                );
            }
        }
        Ok(())
    }

    /// Removes a variable and returns its old value.
    pub fn unset(&mut self, name: &str) -> Option<String> {
        let var = self.vars.remove(name)?;
        self.bytes = self.bytes.saturating_sub(var.value.len());
        Some(var.value)
    }

    /// Marks a variable as exported, creating nothing when it is unset.
    pub fn export(&mut self, name: &str) {
        if let Some(var) = self.vars.get_mut(name) {
            var.exported = true;
        }
    }

    /// Removes the exported mark.
    pub fn unexport(&mut self, name: &str) {
        if let Some(var) = self.vars.get_mut(name) {
            var.exported = false;
        }
    }

    /// True when the variable is passed to commands.
    pub fn is_exported(&self, name: &str) -> bool {
        self.vars.get(name).is_some_and(|var| var.exported)
    }

    /// Locks a variable against assignment and `unset`.
    pub fn set_readonly(&mut self, name: &str) {
        if let Some(var) = self.vars.get_mut(name) {
            var.readonly = true;
        } else {
            self.vars.insert(
                name.to_string(),
                Var {
                    readonly: true,
                    ..Var::default()
                },
            );
        }
    }

    /// True when `readonly` has locked the variable.
    pub fn is_readonly(&self, name: &str) -> bool {
        self.vars.get(name).is_some_and(|var| var.readonly)
    }

    /// Takes a copy of a variable with its attributes.
    pub fn var(&self, name: &str) -> Option<Var> {
        self.vars.get(name).cloned()
    }

    /// Puts back a variable copied by [`Env::var`], or removes it for `None`.
    pub fn restore(&mut self, name: &str, var: Option<Var>) {
        let old = self.vars.get(name).map_or(0, |var| var.value.len());
        let new = var.as_ref().map_or(0, |var| var.value.len());
        self.bytes = (self.bytes + new).saturating_sub(old);
        match var {
            Some(var) => {
                self.vars.insert(name.to_string(), var);
            }
            None => {
                self.vars.remove(name);
            }
        }
    }

    /// Bytes of variable values currently held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Removes every variable that is not exported, as a new process would.
    pub fn retain_exported(&mut self) {
        self.vars
            .retain(|name, var| var.exported || name == "PS1" || var.readonly && var.exported);
        self.bytes = self.vars.values().map(|var| var.value.len()).sum();
    }

    /// Iterates over variables in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars
            .iter()
            .map(|(key, var)| (key.as_str(), var.value.as_str()))
    }

    /// Returns all `NAME=value` entries of exported variables, for `env`.
    pub fn entries(&self) -> Vec<String> {
        self.vars
            .iter()
            .filter(|(_, var)| var.exported)
            .map(|(key, var)| {
                let mut entry = key.clone();
                entry.push('=');
                entry.push_str(&var.value);
                entry
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_refuses_oversized_values() {
        let mut env = Env::new();
        let big = "x".repeat(MAX_ENV_BYTES);
        assert_eq!(env.try_set("A", &big), Ok(()));
        assert_eq!(env.try_set("B", "y"), Err(VarError::NoSpace));
        env.unset("A");
        assert_eq!(env.bytes(), 0);
        assert_eq!(env.try_set("B", "y"), Ok(()));
    }

    #[test]
    fn readonly_blocks_try_set_only() {
        let mut env = Env::new();
        env.set("A", "1");
        env.set_readonly("A");
        assert_eq!(env.try_set("A", "2"), Err(VarError::ReadOnly));
        assert_eq!(env.get("A"), Some("1"));
    }

    #[test]
    fn only_exported_variables_appear_in_entries() {
        let mut env = Env::new();
        env.set("A", "1");
        env.set("B", "2");
        env.export("B");
        assert_eq!(env.entries(), ["B=2"]);
    }

    #[test]
    fn restore_undoes_a_change_with_attributes() {
        let mut env = Env::new();
        env.set("A", "1");
        env.export("A");
        let saved = env.var("A");
        env.set("A", "long value");
        env.unexport("A");
        env.restore("A", saved);
        assert_eq!(env.get("A"), Some("1"));
        assert!(env.is_exported("A"));
        assert_eq!(env.bytes(), 1);
    }
}
