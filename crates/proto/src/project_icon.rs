//! Shared project artwork defaults and checkout-relative path validation.

/// Slots in the monogram palette a project's `color_index` selects.
pub const COLOR_COUNT: u32 = 8;

/// A project's palette slot: the user's choice, else derived from its path.
pub fn color_index(space: &crate::Space) -> u32 {
    space.color_index.map_or_else(
        || path_color_index(&space.path),
        |index| index % COLOR_COUNT,
    )
}

/// 32-bit FNV-1a of the path (`"home"` without a project), so a project
/// keeps the same color on every device.
pub fn path_color_index(path: &str) -> u32 {
    let hash = path.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    hash % COLOR_COUNT
}

pub const ICON_PATHS: &[&str] = &[
    "public/apple-touch-icon.png",
    "apple-touch-icon.png",
    "public/favicon.svg",
    "favicon.svg",
    "public/favicon.png",
    "public/icon.png",
    "public/logo.png",
    "favicon.png",
    "app/icon.png",
    "src/app/icon.png",
    "public/favicon.ico",
    "favicon.ico",
    "app/favicon.ico",
    "static/favicon.ico",
    "src-tauri/icons/icon.png",
    "assets/icon.png",
    "src/assets/icon.png",
];

pub fn normalize_path(path: Option<&str>) -> Result<Option<String>, &'static str> {
    let Some(path) = path.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if path.len() > 1024
        || path.starts_with('/')
        || path.contains(['\\', ':'])
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("Use a path inside the project, such as apps/web/public/icon.png");
    }
    let extension = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if !matches!(
        extension.as_str(),
        "png" | "svg" | "ico" | "jpg" | "jpeg" | "webp" | "gif"
    ) {
        return Err("Choose a PNG, SVG, ICO, JPEG, WebP or GIF image");
    }
    Ok(Some(path.to_owned()))
}

pub fn candidates(icon_path: Option<&str>) -> Vec<&str> {
    match icon_path {
        Some(path) => vec![path],
        None => ICON_PATHS.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_checkout_relative_artwork() {
        assert_eq!(
            normalize_path(Some(" apps/web/public/icon.png "))
                .unwrap()
                .as_deref(),
            Some("apps/web/public/icon.png")
        );
        assert_eq!(normalize_path(Some("  ")).unwrap(), None);
        for path in [
            "/tmp/icon.png",
            "../icon.png",
            "a/../icon.png",
            "C:\\icon.png",
            "https://example.com/icon.png",
            "a//icon.png",
            "file.txt",
        ] {
            assert!(normalize_path(Some(path)).is_err(), "{path}");
        }
        assert_eq!(candidates(Some("apps/web/icon.svg")), ["apps/web/icon.svg"]);
    }
}
