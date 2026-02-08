//! Small helpers shared across modules.

use std::borrow::Cow;

/// Expand a leading `~` to `$HOME`.
///
/// - `"~"` → `"/home/user"`
/// - `"~/foo"` → `"/home/user/foo"`
/// - Anything else passes through unchanged.
pub fn expand_tilde(path: &str) -> Cow<'_, str> {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            if path == "~" {
                return Cow::Owned(home);
            }
            return Cow::Owned(format!("{}{}", home, &path[1..]));
        }
    }
    Cow::Borrowed(path)
}
