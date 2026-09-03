//! Resolving which compose project to attach to.

/// Derives a compose project name from a directory, the way Compose itself does:
/// lower-cased, with everything outside `[a-z0-9_-]` dropped and any leading
/// separators trimmed.
pub fn normalize(name: &str) -> String {
    let lowered: String = name
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    lowered.trim_start_matches(['_', '-']).to_string()
}

/// The project to use when none was given on the command line or in config:
/// `COMPOSE_PROJECT_NAME` if set, else the working directory's basename.
pub fn detect() -> Option<String> {
    if let Ok(name) = std::env::var("COMPOSE_PROJECT_NAME") {
        if !name.is_empty() {
            return Some(name);
        }
    }
    let cwd = std::env::current_dir().ok()?;
    let base = cwd.file_name()?.to_str()?;
    let normalized = normalize(base);
    (!normalized.is_empty()).then_some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_compose_does() {
        assert_eq!(normalize("My.Project"), "myproject");
        assert_eq!(normalize("feature/PLAT-42"), "featureplat-42");
        assert_eq!(normalize("uncloak-identity"), "uncloak-identity");
        assert_eq!(normalize("_leading"), "leading");
    }
}
