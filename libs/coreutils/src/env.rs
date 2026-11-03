//! Shell variables and the environment.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// An ordered set of shell/environment variables.
#[derive(Clone, Debug, Default)]
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    /// Creates an empty environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the default FEOS environment.
    pub fn feos() -> Self {
        let mut env = Self::new();
        env.set("HOME", "/");
        env.set("PWD", "/");
        env.set("OLDPWD", "/");
        env.set("USER", "feos");
        env.set("LOGNAME", "feos");
        env.set("HOSTNAME", "feos");
        env.set("SHELL", "/bin/sh");
        env.set("PATH", "/bin:/usr/bin:/");
        env.set("TERM", "feos");
        env.set("LANG", "C");
        env.set("PS1", "feos:\\w\\$ ");
        env
    }

    /// Returns a variable value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Sets a variable.
    pub fn set(&mut self, name: &str, value: &str) {
        self.vars.insert(name.to_string(), value.to_string());
    }

    /// Removes a variable and returns its old value.
    pub fn unset(&mut self, name: &str) -> Option<String> {
        self.vars.remove(name)
    }

    /// Iterates over variables in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Returns all `NAME=value` entries for `env`.
    pub fn entries(&self) -> Vec<String> {
        self.vars
            .iter()
            .map(|(key, value)| {
                let mut entry = key.clone();
                entry.push('=');
                entry.push_str(value);
                entry
            })
            .collect()
    }
}
