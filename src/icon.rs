//! Shared build-time and runtime application icon.
pub fn icon_rgba() -> Vec<u8> {
    let mut rgba = vec![0u8; 32 * 32 * 4];
    for y in 0..32 {
        for x in 0..32 {
            let dx = x as i32 - 16;
            let dy = y as i32 - 16;
            if dx * dx + dy * dy < 225 {
                let arrow = ((13..=18).contains(&x) && (9..=23).contains(&y))
                    || ((6..=13).contains(&y) && (x as i32 - 16).abs() <= 13 - y as i32);
                let color = if arrow {
                    [242, 248, 255, 255]
                } else {
                    [32, 114, 208, 255]
                };
                rgba[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4].copy_from_slice(&color);
            }
        }
    }
    rgba
}
pub fn icon_ico() -> Vec<u8> {
    let mut result = vec![0, 0, 1, 0, 1, 0, 32, 32, 0, 0, 1, 0, 32, 0];
    result.extend((40u32 + 4096 + 128).to_le_bytes());
    result.extend(22u32.to_le_bytes());
    result.extend(40u32.to_le_bytes());
    result.extend(32i32.to_le_bytes());
    result.extend(64i32.to_le_bytes());
    result.extend(1u16.to_le_bytes());
    result.extend(32u16.to_le_bytes());
    result.extend([0u8; 24]);
    let rgba = icon_rgba();
    for y in (0..32).rev() {
        for x in 0..32 {
            let i = (y * 32 + x) * 4;
            result.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    result.extend([0u8; 128]);
    result
}
