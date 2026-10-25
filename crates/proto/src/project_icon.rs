//! Shared project artwork defaults and checkout-relative path validation.

/// Slots in the legacy monogram palette a project's `color_index` selects.
pub const COLOR_COUNT: u32 = 8;

/// The palette `color_index` selects: (dark appearance, light appearance).
/// Every peer indexes it, so keep the order stable.
pub const LEGACY_PALETTE: [(u32, u32); COLOR_COUNT as usize] = [
    (0x94a3b8, 0x475569), // slate
    (0x93c5fd, 0x2563eb), // blue
    (0xc4b5fd, 0x7c3aed), // violet
    (0xfda4af, 0xbe123c), // rose
    (0xfcd34d, 0xa16207), // amber
    (0x6ee7b7, 0x047857), // emerald
    (0x5eead4, 0x0f766e), // teal
    (0xfdba74, 0xc2410c), // orange
];

/// Further curated tones. They are saved as custom colors, which older peers
/// show as the nearest legacy slot.
pub const EXTRA_PALETTE: [u32; 12] = [
    0xa8a29e, // stone
    0xef4444, // red
    0xec4899, // pink
    0xd946ef, // fuchsia
    0xa855f7, // purple
    0x6366f1, // indigo
    0x0ea5e9, // sky
    0x06b6d4, // cyan
    0x22c55e, // green
    0x84cc16, // lime
    0xeab308, // yellow
    0xa0522d, // brown
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProjectColor {
    /// A legacy palette slot, drawn with its per-appearance pair.
    Palette(u32),
    /// `0xRRGGBB`, drawn through [`appearance_tone`].
    Custom(u32),
}

pub const SWATCHES: [ProjectColor; 20] = {
    use EXTRA_PALETTE as X;
    use ProjectColor::{Custom, Palette};
    [
        Palette(0),    // slate
        Custom(X[0]),  // stone
        Custom(X[1]),  // red
        Palette(3),    // rose
        Custom(X[2]),  // pink
        Custom(X[3]),  // fuchsia
        Custom(X[4]),  // purple
        Palette(2),    // violet
        Custom(X[5]),  // indigo
        Palette(1),    // blue
        Custom(X[6]),  // sky
        Custom(X[7]),  // cyan
        Palette(6),    // teal
        Palette(5),    // emerald
        Custom(X[8]),  // green
        Custom(X[9]),  // lime
        Custom(X[10]), // yellow
        Palette(4),    // amber
        Palette(7),    // orange
        Custom(X[11]), // brown
    ]
};

/// The color a project wears. A custom color applies only while
/// `color_index` still holds its nearest slot, so an older peer that picks
/// another slot (or resets it) wins over a custom color it can't see.
pub fn project_color(space: &crate::Space) -> ProjectColor {
    match space.color.as_deref().and_then(parse_color) {
        Some(rgb) if space.color_index == Some(nearest_index(rgb)) => ProjectColor::Custom(rgb),
        _ => ProjectColor::Palette(color_index(space)),
    }
}

/// [`project_color`]'s custom color as `#rrggbb`, if it applies.
pub fn custom_color(space: &crate::Space) -> Option<String> {
    match project_color(space) {
        ProjectColor::Custom(rgb) => Some(format_color(rgb)),
        ProjectColor::Palette(_) => None,
    }
}

/// `#RRGGBB` (the `#` is optional) as `0xRRGGBB`.
pub fn parse_color(color: &str) -> Option<u32> {
    let color = color.trim();
    let hex = color.strip_prefix('#').unwrap_or(color);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

pub fn format_color(rgb: u32) -> String {
    format!("#{:06x}", rgb & 0xff_ffff)
}

/// Blank means no custom color; anything else must be `#RRGGBB`.
pub fn normalize_color(color: Option<&str>) -> Result<Option<String>, &'static str> {
    let Some(color) = color.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    parse_color(color)
        .map(|rgb| Some(format_color(rgb)))
        .ok_or("Use a hex color such as #4f8cff")
}

/// The legacy slot nearest in hue (grays go to slate). Saved beside a custom
/// color so older peers still show something close.
pub fn nearest_index(rgb: u32) -> u32 {
    let (hue, saturation, _) = hsl(rgb);
    if saturation < 0.25 {
        return 0;
    }
    let distance = |slot: u32| {
        let (dark, light) = LEGACY_PALETTE[slot as usize];
        [dark, light]
            .into_iter()
            .map(|tone| {
                let delta = (hsl(tone).0 - hue).abs();
                delta.min(360.0 - delta)
            })
            .fold(f32::MAX, f32::min)
    };
    (1..COLOR_COUNT)
        .min_by(|a, b| distance(*a).total_cmp(&distance(*b)))
        .unwrap_or(0)
}

/// A custom color kept legible in each appearance, like the legacy pairs:
/// bright enough on dark surfaces, dark enough on light ones. Only the HSL
/// lightness moves; the hue stays.
pub fn appearance_tone(rgb: u32, dark: bool) -> u32 {
    // Relative luminance bounds of the legacy pairs' two sides.
    const DARK_MIN: f32 = 0.35;
    const LIGHT_MAX: f32 = 0.18;
    let legible = |tone: u32| {
        if dark {
            luminance(tone) >= DARK_MIN
        } else {
            luminance(tone) <= LIGHT_MAX
        }
    };
    if legible(rgb) {
        return rgb;
    }
    let (hue, saturation, lightness) = hsl(rgb);
    // The legible end of the range, narrowed toward the original lightness.
    let (mut near, mut far) = if dark {
        (lightness, 1.0)
    } else {
        (lightness, 0.0)
    };
    for _ in 0..16 {
        let mid = (near + far) / 2.0;
        if legible(from_hsl(hue, saturation, mid)) {
            far = mid;
        } else {
            near = mid;
        }
    }
    from_hsl(hue, saturation, far)
}

fn luminance(rgb: u32) -> f32 {
    let linear = |shift: u32| {
        let c = ((rgb >> shift) & 0xff) as f32 / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
}

fn hsl(rgb: u32) -> (f32, f32, f32) {
    let channel = |shift: u32| ((rgb >> shift) & 0xff) as f32 / 255.0;
    let (r, g, b) = (channel(16), channel(8), channel(0));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let delta = max - min;
    if delta <= f32::EPSILON {
        return (0.0, 0.0, lightness);
    }
    let saturation = delta / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (hue, saturation, lightness)
}

fn from_hsl(hue: f32, saturation: f32, lightness: f32) -> u32 {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let x = chroma * (1.0 - ((hue / 60.0).rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match (hue / 60.0) as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = lightness - chroma / 2.0;
    let byte = |v: f32| (((v + m) * 255.0).round().clamp(0.0, 255.0)) as u32;
    (byte(r) << 16) | (byte(g) << 8) | byte(b)
}

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

/// The artwork extensions as a workspace file search glob.
pub const IMAGE_GLOB: &str = "*.{png,svg,ico,jpg,jpeg,webp,gif}";

/// Build output and dependencies the artwork browser skips even when a
/// repository doesn't ignore them.
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "bower_components",
    "vendor",
    "Pods",
    "target",
    "dist",
    "build",
    "out",
    "DerivedData",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".cache",
    "coverage",
];

const ICON_WORDS: &[&str] = &[
    "favicon",
    "apple-touch",
    "app-icon",
    "appicon",
    "icon",
    "logo",
    "brand",
];

/// Workspace file searches that list artwork for the browser: likely icon
/// names, then every image, or the user's filter.
pub fn browse_queries(filter: &str) -> Vec<String> {
    match filter.trim() {
        "" => ["icon", "logo", ""]
            .iter()
            .map(|query| format!("{query} {IMAGE_GLOB}").trim().to_owned())
            .collect(),
        filter => vec![format!("{filter} {IMAGE_GLOB}")],
    }
}

/// Valid artwork outside dependency and build folders.
pub fn is_browsable(path: &str) -> bool {
    normalize_path(Some(path)).is_ok_and(|path| path.is_some())
        && !path
            .split('/')
            .rev()
            .skip(1)
            .any(|part| SKIPPED_DIRS.contains(&part))
}

/// Browsable artwork, deduplicated, most likely icons first: filter matches
/// in the name before those in the path, then names like icon, logo or
/// favicon (or inside an icons folder), then shallower paths.
pub fn rank_browse_results(filter: &str, paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let filter = filter.trim().to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut paths: Vec<String> = paths
        .into_iter()
        .filter(|path| is_browsable(path) && seen.insert(path.clone()))
        .collect();
    paths.sort_by_cached_key(|path| {
        let lower = path.to_lowercase();
        let (folders, name) = lower.rsplit_once('/').unwrap_or(("", &lower));
        let filter_rank = if filter.is_empty() || name.contains(&filter) {
            0
        } else if lower.contains(&filter) {
            1
        } else {
            2
        };
        // Favicons first, then other icons, logos and brand art.
        let icon_rank = ICON_WORDS
            .iter()
            .position(|word| name.contains(word))
            .unwrap_or_else(|| {
                ICON_WORDS.len() + usize::from(!ICON_WORDS.iter().any(|w| folders.contains(w)))
            });
        let depth = path.matches('/').count();
        (filter_rank, icon_rank, depth, lower.len(), lower)
    });
    paths
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

    #[test]
    fn validates_hex_colors() {
        assert_eq!(parse_color("#4F8CFF"), Some(0x4f8cff));
        assert_eq!(parse_color(" 4f8cff "), Some(0x4f8cff));
        for color in [
            "", "#", "#fff", "#4f8cf", "#4f8cff0", "#4g8cff", "##4f8cff", "red",
        ] {
            assert_eq!(parse_color(color), None, "{color}");
        }
        assert_eq!(
            normalize_color(Some(" #4F8CFF ")).unwrap().as_deref(),
            Some("#4f8cff")
        );
        assert_eq!(normalize_color(Some(" ")).unwrap(), None);
        assert_eq!(normalize_color(None).unwrap(), None);
        assert!(normalize_color(Some("#12345")).is_err());
        assert_eq!(format_color(0x0000ff), "#0000ff");
    }

    #[test]
    fn custom_colors_map_to_the_nearest_legacy_slot() {
        for (slot, (dark, light)) in LEGACY_PALETTE.iter().enumerate() {
            assert_eq!(nearest_index(*dark), slot as u32, "{dark:06x}");
            assert_eq!(nearest_index(*light), slot as u32, "{light:06x}");
        }
        for (rgb, slot) in [
            (0xa8a29e, 0), // stone → slate
            (0x6b7280, 0), // gray → slate
            (0x000000, 0),
            (0xffffff, 0),
            (0xef4444, 3), // red → rose
            (0xec4899, 3), // pink → rose
            (0xa855f7, 2), // purple → violet
            (0x0ea5e9, 1), // sky → blue
            (0x06b6d4, 6), // cyan → teal
            (0x22c55e, 5), // green → emerald
            (0xeab308, 4), // yellow → amber
            (0xa0522d, 7), // brown → orange
        ] {
            assert_eq!(nearest_index(rgb), slot, "{rgb:06x}");
        }
    }

    #[test]
    fn custom_color_wins_only_beside_its_legacy_slot() {
        let mut space: crate::Space = serde_json::from_value(serde_json::json!({
            "id": "s", "deviceId": "d", "path": "/p", "createdAt": "2026-01-01T00:00:00Z",
        }))
        .unwrap();
        assert_eq!(
            project_color(&space),
            ProjectColor::Palette(path_color_index("/p"))
        );
        space.color = Some("#ef4444".into());
        space.color_index = Some(nearest_index(0xef4444));
        assert_eq!(project_color(&space), ProjectColor::Custom(0xef4444));
        assert_eq!(custom_color(&space).as_deref(), Some("#ef4444"));
        // An older peer picked another slot, or reset the color.
        space.color_index = Some(1);
        assert_eq!(project_color(&space), ProjectColor::Palette(1));
        space.color_index = None;
        assert_eq!(
            project_color(&space),
            ProjectColor::Palette(path_color_index("/p"))
        );
        space.color = Some("not a color".into());
        space.color_index = Some(2);
        assert_eq!(project_color(&space), ProjectColor::Palette(2));
    }

    #[test]
    fn appearance_tones_stay_legible() {
        let navy = appearance_tone(0x1e3a8a, true);
        assert!(luminance(navy) >= 0.34, "{navy:06x}");
        let cream = appearance_tone(0xfde68a, false);
        assert!(luminance(cream) <= 0.18, "{cream:06x}");
        // The hue survives.
        assert!((hsl(navy).0 - hsl(0x1e3a8a).0).abs() < 3.0);
        // Legacy pairs are already legible on their side.
        for (dark, light) in LEGACY_PALETTE {
            assert_eq!(appearance_tone(dark, true), dark);
            assert_eq!(appearance_tone(light, false), light);
        }
    }

    #[test]
    fn browse_ranks_likely_icons_first_and_skips_build_output() {
        assert_eq!(
            browse_queries(""),
            [
                format!("icon {IMAGE_GLOB}"),
                format!("logo {IMAGE_GLOB}"),
                IMAGE_GLOB.to_owned()
            ]
        );
        assert_eq!(browse_queries(" hero "), [format!("hero {IMAGE_GLOB}")]);
        let ranked = rank_browse_results(
            "",
            [
                "docs/screenshots/home.png",
                "apps/web/public/favicon.ico",
                "public/logo.svg",
                "node_modules/pkg/icon.png",
                "apps/web/dist/icon.png",
                "src/main.rs",
                "assets/icons/close.svg",
                "public/logo.svg",
                "hero.jpg",
            ]
            .map(String::from),
        );
        assert_eq!(
            ranked,
            [
                "apps/web/public/favicon.ico",
                "public/logo.svg",
                "assets/icons/close.svg",
                "hero.jpg",
                "docs/screenshots/home.png",
            ]
        );
        let ranked = rank_browse_results(
            "home",
            ["home/logo.png", "docs/home.png", "public/icon.png"].map(String::from),
        );
        assert_eq!(
            ranked,
            ["docs/home.png", "home/logo.png", "public/icon.png"]
        );
    }
}
