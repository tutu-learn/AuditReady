//! Tray icon image: a blocky "S" (SEBRUS::OPS) in the theme cyan.
//!
//! Rasterized from a 5x7 pixel-font bitmap so the build doesn't need a
//! bundled asset file.

pub fn tray_icon_image() -> tray_icon::Icon {
    const SIZE: u32 = 32;
    const SCALE: u32 = 4;
    const GLYPH: [&str; 7] = [
        ".###.",
        "#...#",
        "#....",
        ".###.",
        "....#",
        "#...#",
        ".###.",
    ];
    let offset_x = (SIZE - 5 * SCALE) / 2;
    let offset_y = (SIZE - 7 * SCALE) / 2;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for (gy, row) in GLYPH.iter().enumerate() {
        for (gx, ch) in row.chars().enumerate() {
            if ch != '#' {
                continue;
            }
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    let x = offset_x + gx as u32 * SCALE + dx;
                    let y = offset_y + gy as u32 * SCALE + dy;
                    let idx = ((y * SIZE + x) * 4) as usize;
                    rgba[idx] = 0x3f;
                    rgba[idx + 1] = 0xe0;
                    rgba[idx + 2] = 0xff;
                    rgba[idx + 3] = 0xff;
                }
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("valid tray icon buffer")
}
