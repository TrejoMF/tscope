use ratatui::style::Color;

/// Ordered list of colors the settings modal cycles through. Name first,
/// so it shows up nicely in the config file and the UI.
pub const PALETTE: &[(&str, Color)] = &[
    ("blue", Color::Blue),
    ("cyan", Color::Cyan),
    ("green", Color::Green),
    ("magenta", Color::Magenta),
    ("yellow", Color::Yellow),
    ("red", Color::Red),
    ("light-blue", Color::LightBlue),
    ("light-cyan", Color::LightCyan),
    ("light-green", Color::LightGreen),
    ("light-magenta", Color::LightMagenta),
    ("light-yellow", Color::LightYellow),
    ("light-red", Color::LightRed),
    ("white", Color::White),
    ("gray", Color::Gray),
];

pub fn color_from_name(name: &str) -> Option<Color> {
    PALETTE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
}

pub fn color_index(name: &str) -> usize {
    PALETTE.iter().position(|(n, _)| *n == name).unwrap_or(0)
}

pub fn color_name_by_index(idx: usize) -> &'static str {
    PALETTE[idx.rem_euclid(PALETTE.len())].0
}
