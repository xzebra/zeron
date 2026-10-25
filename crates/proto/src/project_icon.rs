//! Project palette and checkout-relative artwork paths.

pub const AUTOMATIC_COLOR_COUNT: u32 = 8;
pub const COLOR_COUNT: u32 = 20;

/// (dark, light). Keep the first eight stable for automatic project colors.
pub const PALETTE: [(u32, u32); COLOR_COUNT as usize] = [
    (0x94a3b8, 0x475569),
    (0x93c5fd, 0x2563eb),
    (0xc4b5fd, 0x7c3aed),
    (0xfda4af, 0xbe123c),
    (0xfcd34d, 0xa16207),
    (0x6ee7b7, 0x047857),
    (0x5eead4, 0x0f766e),
    (0xfdba74, 0xc2410c),
    (0xa8a29e, 0x78716c),
    (0xf87171, 0xdc2626),
    (0xf472b6, 0xdb2777),
    (0xe879f9, 0xc026d3),
    (0xc084fc, 0x9333ea),
    (0xa5b4fc, 0x4f46e5),
    (0x38bdf8, 0x0284c7),
    (0x22d3ee, 0x0891b2),
    (0x4ade80, 0x16a34a),
    (0xa3e635, 0x4d7c0f),
    (0xfde047, 0x854d0e),
    (0xd99775, 0x9a3412),
];

pub fn color_index(space: &crate::Space) -> u32 {
    space
        .color_index
        .filter(|index| *index < COLOR_COUNT)
        .unwrap_or_else(|| path_color_index(&space.path))
}

pub fn path_color_index(path: &str) -> u32 {
    let hash = path.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    hash % AUTOMATIC_COLOR_COUNT
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

pub const IMAGE_GLOB: &str = "*.{png,svg,ico,jpg,jpeg,webp,gif}";

pub fn browse_query(filter: &str) -> String {
    format!("{} {IMAGE_GLOB}", filter.trim()).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artwork_stays_inside_the_checkout() {
        assert_eq!(
            normalize_path(Some(" apps/web/icon.png "))
                .unwrap()
                .as_deref(),
            Some("apps/web/icon.png")
        );
        assert_eq!(normalize_path(Some(" ")).unwrap(), None);
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
    }
    #[test]
    fn automatic_colors_keep_the_original_eight_slots() {
        for (path, expected) in [("home", 6), ("/projects/app", 0), ("/projects/tools", 0)] {
            assert_eq!(path_color_index(path), expected);
        }
        let mut space: crate::Space = serde_json::from_value(serde_json::json!({"id":"s","deviceId":"d","path":"/p","createdAt":"2026-01-01T00:00:00Z"})).unwrap();
        space.color_index = Some(19);
        assert_eq!(color_index(&space), 19);
        space.color_index = Some(20);
        assert_eq!(color_index(&space), path_color_index("/p"));
    }
}
