use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::error::ConfigError;

pub fn globset(patterns: &[String], key: &str) -> Result<GlobSet, ConfigError> {
    let mut builder = GlobSetBuilder::new();
    for (i, pattern) in patterns.iter().enumerate() {
        let glob = Glob::new(pattern).map_err(|err| {
            ConfigError::new(format!("{key}[{i}]"), format!("invalid glob: {err}"))
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|err| ConfigError::new(key, format!("invalid glob set: {err}")))
}

pub fn path_allowed(path: &str, include: &GlobSet, exclude: &GlobSet, include_empty: bool) -> bool {
    if include_empty || !include.is_match(path) {
        return false;
    }
    !exclude.is_match(path)
}
