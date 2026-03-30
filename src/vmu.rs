// SPDX-License-Identifier: GPL-3.0-or-later

//! VMU LCD framebuffer assets and compositing.
//!
//! The Dreamcast VMU LCD is 48×32 pixels, 1 bit per pixel (192 bytes).
//! Each row is 6 bytes, MSB = leftmost pixel.

/// VMU LCD dimensions.
pub const LCD_WIDTH: usize = 48;
pub const LCD_HEIGHT: usize = 32;
pub const LCD_BYTES: usize = LCD_WIDTH * LCD_HEIGHT / 8;

/// Reverse a byte's bits (MSB ↔ LSB).
const fn reverse_bits(b: u8) -> u8 {
    let mut r = 0u8;
    let mut i = 0;
    while i < 8 {
        r |= ((b >> i) & 1) << (7 - i);
        i += 1;
    }
    r
}

/// Rotate a framebuffer 180° (flip both horizontally and vertically).
///
/// The VMU mounts upside-down in the controller, so the screen image must
/// be rotated 180° before sending. This reverses the byte array and
/// bit-reverses each byte.
pub fn rotate_180(frame: &mut [u8; LCD_BYTES]) {
    // Reverse the array (flips vertically + shifts columns)
    frame.reverse();
    // Bit-reverse each byte (flips horizontally)
    for byte in frame.iter_mut() {
        *byte = reverse_bits(*byte);
    }
}

/// Pulsar logo — 48×32 1bpp bitmap.
///
/// Neutron star with two jets at ~30° from vertical.
/// Displayed on the VMU when no game is writing to the LCD.
#[rustfmt::skip]
pub const PULSAR_LOGO: [u8; LCD_BYTES] = [
    // Row  0:  ............................XXXXXXXX............
    0x00, 0x00, 0x00, 0x0F, 0xF0, 0x00,
    // Row  1:  ...........................XXXXXXXX.............
    0x00, 0x00, 0x00, 0x1F, 0xE0, 0x00,
    // Row  2:  ...........................XXXXXXX..............
    0x00, 0x00, 0x00, 0x1F, 0xC0, 0x00,
    // Row  3:  ..........................XXXXXXX...............
    0x00, 0x00, 0x00, 0x3F, 0x80, 0x00,
    // Row  4:  ..........................XXXXXX................
    0x00, 0x00, 0x00, 0x3F, 0x00, 0x00,
    // Row  5:  .........................XXXXXX.................
    0x00, 0x00, 0x00, 0x7E, 0x00, 0x00,
    // Row  6:  .........................XXXXX..................
    0x00, 0x00, 0x00, 0x7C, 0x00, 0x00,
    // Row  7:  ........................XXXXX...................
    0x00, 0x00, 0x00, 0xF8, 0x00, 0x00,
    // Row  8:  ........................XXXX....................
    0x00, 0x00, 0x00, 0xF0, 0x00, 0x00,
    // Row  9:  .......................XXXX.....................
    0x00, 0x00, 0x01, 0xE0, 0x00, 0x00,
    // Row 10:  .......................XXX......................
    0x00, 0x00, 0x01, 0xC0, 0x00, 0x00,
    // Row 11:  ....................XXXXXXXX....................  star top
    0x00, 0x00, 0x0F, 0xF0, 0x00, 0x00,
    // Row 12:  ..................XXXXXXXXXXXX..................
    0x00, 0x00, 0x3F, 0xFC, 0x00, 0x00,
    // Row 13:  .................XXXXXXXXXXXXXX.................
    0x00, 0x00, 0x7F, 0xFE, 0x00, 0x00,
    // Row 14:  .................XXXXXXXXXXXXXX.................
    0x00, 0x00, 0x7F, 0xFE, 0x00, 0x00,
    // Row 15:  ................XXXXXXXXXXXXXXXX................  widest
    0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00,
    // Row 16:  ................XXXXXXXXXXXXXXXX................
    0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00,
    // Row 17:  .................XXXXXXXXXXXXXX.................
    0x00, 0x00, 0x7F, 0xFE, 0x00, 0x00,
    // Row 18:  .................XXXXXXXXXXXXXX.................
    0x00, 0x00, 0x7F, 0xFE, 0x00, 0x00,
    // Row 19:  ..................XXXXXXXXXXXX..................
    0x00, 0x00, 0x3F, 0xFC, 0x00, 0x00,
    // Row 20:  ....................XXXXXXXX....................  star bottom
    0x00, 0x00, 0x0F, 0xF0, 0x00, 0x00,
    // Row 21:  ......................XXX.......................
    0x00, 0x00, 0x03, 0x80, 0x00, 0x00,
    // Row 22:  .....................XXXX.......................
    0x00, 0x00, 0x07, 0x80, 0x00, 0x00,
    // Row 23:  ....................XXXX........................
    0x00, 0x00, 0x0F, 0x00, 0x00, 0x00,
    // Row 24:  ...................XXXXX........................
    0x00, 0x00, 0x1F, 0x00, 0x00, 0x00,
    // Row 25:  ..................XXXXX.........................
    0x00, 0x00, 0x3E, 0x00, 0x00, 0x00,
    // Row 26:  .................XXXXXX.........................
    0x00, 0x00, 0x7E, 0x00, 0x00, 0x00,
    // Row 27:  ................XXXXXX..........................
    0x00, 0x00, 0xFC, 0x00, 0x00, 0x00,
    // Row 28:  ...............XXXXXXX..........................
    0x00, 0x01, 0xFC, 0x00, 0x00, 0x00,
    // Row 29:  ..............XXXXXXX...........................
    0x00, 0x03, 0xF8, 0x00, 0x00, 0x00,
    // Row 30:  .............XXXXXXXX...........................
    0x00, 0x07, 0xF8, 0x00, 0x00, 0x00,
    // Row 31:  ............XXXXXXXX............................
    0x00, 0x0F, 0xF0, 0x00, 0x00, 0x00,
];

// ── Battery icon ────────────────────────────────────────────────────────────

/// Battery icon dimensions (12×7 pixels).
pub const BATTERY_WIDTH: usize = 12;
pub const BATTERY_HEIGHT: usize = 7;

/// Position of the battery icon on the VMU LCD (top-right corner).
pub const BATTERY_X: usize = LCD_WIDTH - BATTERY_WIDTH;
pub const BATTERY_Y: usize = 0;

/// Battery outline mask — 12×7, 1bpp.
///
/// Pixels owned by the overlay (1 = overlay controls this pixel).
/// Applied as: `output = (game & !mask) | icon`
///
/// ```text
///  .XXXXXXXXXX.
///  X..........X
///  X..........XX  ← nub
///  X..........XX
///  X..........XX
///  X..........X
///  .XXXXXXXXXX.
/// ```
#[rustfmt::skip]
const BATTERY_MASK: [[u8; 2]; BATTERY_HEIGHT] = [
    //  .XXXXXXXXXX.       (cols 1-10)
    [0x7F, 0xE0],
    //  XXXXXXXXXXXX       (all cols)
    [0xFF, 0xF0],
    //  XXXXXXXXXXXX
    [0xFF, 0xF0],
    //  XXXXXXXXXXXX
    [0xFF, 0xF0],
    //  XXXXXXXXXXXX
    [0xFF, 0xF0],
    //  XXXXXXXXXXXX
    [0xFF, 0xF0],
    //  .XXXXXXXXXX.
    [0x7F, 0xE0],
];

/// Battery outline (border + nub), no bars filled.
///
/// ```text
///  .XXXXXXXXX..
///  X..........X.
///  X..........XX  ← nub
///  X..........XX
///  X..........XX
///  X..........X.
///  .XXXXXXXXX..
/// ```
#[rustfmt::skip]
const BATTERY_OUTLINE: [[u8; 2]; BATTERY_HEIGHT] = [
    //  .XXXXXXXXX..       (cols 1-9, border top)
    [0x7F, 0xC0],
    //  X.........X.       (cols 0, 10)
    [0x80, 0x20],
    //  X.........XX       (cols 0, 10-11, nub starts)
    [0x80, 0x30],
    //  X.........XX
    [0x80, 0x30],
    //  X.........XX
    [0x80, 0x30],
    //  X.........X.       (cols 0, 10)
    [0x80, 0x20],
    //  .XXXXXXXXX..       (cols 1-9, border bottom)
    [0x7F, 0xC0],
];

/// Column positions (within the 12px icon) for each of the 4 bars.
/// Each bar is 1px wide, 3px tall (rows 2-4).
/// Bars at icon columns 2, 4, 6, 8.
const BAR_COLS: [usize; 4] = [2, 4, 6, 8];

/// Number of bars to show for a given battery percentage.
#[must_use]
pub const fn bars_for_percent(percent: u8) -> u8 {
    match percent {
        75..=100 => 4,
        50..=74 => 3,
        25..=49 => 2,
        10..=24 => 1,
        _ => 0,
    }
}

/// Render the battery icon into a 12×7 buffer with the given number of bars (0-4).
#[must_use]
pub fn render_battery(bars: u8) -> [[u8; 2]; BATTERY_HEIGHT] {
    let mut icon = BATTERY_OUTLINE;

    // Fill bars (rows 2-4, each bar is 1px wide)
    for &col in BAR_COLS.iter().take(bars as usize) {
        for row in &mut icon[2..5] {
            // Set the bit at `col` within the 12-bit icon row.
            // Bit 7 of byte 0 = col 0, bit 0 of byte 0 = col 7,
            // bit 7 of byte 1 = col 8, etc.
            if col < 8 {
                row[0] |= 1 << (7 - col);
            } else {
                row[1] |= 1 << (15 - col);
            }
        }
    }

    icon
}

/// Composite the battery icon onto a VMU framebuffer at the top-right corner.
///
/// Uses AND-then-OR masked blit:
///   `output[i] = (frame[i] & !mask[i]) | icon[i]`
pub fn composite_battery(frame: &mut [u8; LCD_BYTES], percent: u8, visible: bool) {
    if !visible {
        return;
    }

    let bars = bars_for_percent(percent);
    let icon = render_battery(bars);

    let bytes_per_row = LCD_WIDTH / 8; // 6

    for iy in 0..BATTERY_HEIGHT {
        let frame_row = BATTERY_Y + iy;
        if frame_row >= LCD_HEIGHT {
            break;
        }

        // The icon is 12px wide starting at BATTERY_X (col 36).
        // Col 36 = byte 4 bit 3..0 and byte 5 bit 7..4.
        let byte_offset = BATTERY_X / 8; // 4
        let bit_offset = BATTERY_X % 8; // 4

        let row_start = frame_row * bytes_per_row;

        // Shift mask and icon right by bit_offset to align with frame bytes.
        let mask_hi = BATTERY_MASK[iy][0] >> bit_offset;
        let mask_lo =
            (BATTERY_MASK[iy][0] << (8 - bit_offset)) | (BATTERY_MASK[iy][1] >> bit_offset);

        let icon_hi = icon[iy][0] >> bit_offset;
        let icon_lo = (icon[iy][0] << (8 - bit_offset)) | (icon[iy][1] >> bit_offset);

        frame[row_start + byte_offset] = (frame[row_start + byte_offset] & !mask_hi) | icon_hi;
        if byte_offset + 1 < bytes_per_row {
            frame[row_start + byte_offset + 1] =
                (frame[row_start + byte_offset + 1] & !mask_lo) | icon_lo;
        }
    }
}

// ── Rotating pulsar animation ──────────────────────────────────────────────

/// Number of rotation frames in the animation.
pub const ROTATION_FRAMES: u8 = 12;

const STAR_CENTER_X: i16 = 24; // Center of 48px wide screen
const STAR_CENTER_Y: i16 = 16; // Center of 32px tall screen
const STAR_RADIUS_X: i16 = 8;
const STAR_RADIUS_Y: i16 = 5;

/// Jet cone definitions for each animation frame (12 frames = 30° steps).
/// Each entry is (tip_dx, tip_dy, spread_dx, spread_dy):
///   - (tip_dx, tip_dy): center of the cone's far end, relative to star center
///   - (spread_dx, spread_dy): offset from tip center to each edge of the cone
///
/// The cone is a filled triangle: star center → tip+spread → tip-spread.
/// Lower jet mirrors the upper: negate all offsets.
///
/// Scaled to reach screen edges (48x32), accounting for the wider aspect ratio.
#[rustfmt::skip]
const JET_CONES: [(i16, i16, i16, i16); 12] = [
    // (tip_dx, tip_dy, spread_dx, spread_dy)
    // Screen is 48x32, center at (24,16). Max reach: ±23 horiz, ±15 vert.
    (  3, -15,  4,  0),  //   0° — nearly vertical, up
    ( 15, -15,  4,  2),  //  30° — stretched diagonal
    ( 21, -13,  2,  4),  //  60° — stretched diagonal
    ( 22,  -3,  0,  4),  //  90° — horizontal right (was good)
    ( 21,  13,  2,  4),  // 120° — stretched diagonal
    ( 15,  15,  4,  2),  // 150° — stretched diagonal
    (  3,  15,  4,  0),  // 180° — nearly vertical, down
    (-15,  15, -4,  2),  // 210° — stretched diagonal
    (-21,  13, -2,  4),  // 240° — stretched diagonal
    (-22,  -3,  0,  4),  // 270° — horizontal left (was good)
    (-21, -13, -2,  4),  // 300° — stretched diagonal
    (-15, -15, -4,  2),  // 330° — stretched diagonal
];

/// Set a pixel in the framebuffer. x=0 is leftmost, y=0 is topmost.
fn set_pixel(frame: &mut [u8; LCD_BYTES], x: i16, y: i16) {
    if x < 0 || x >= LCD_WIDTH as i16 || y < 0 || y >= LCD_HEIGHT as i16 {
        return;
    }
    let row = y as usize;
    let col = x as usize;
    let byte_idx = row * (LCD_WIDTH / 8) + col / 8;
    let bit_idx = 7 - (col % 8);
    frame[byte_idx] |= 1 << bit_idx;
}

/// Draw a filled triangle using scanline fill.
/// Vertices: (x0,y0), (x1,y1), (x2,y2).
fn fill_triangle(frame: &mut [u8; LCD_BYTES], verts: [(i16, i16); 3]) {
    let min_y = verts[0].1.min(verts[1].1).min(verts[2].1).max(0);
    let max_y = verts[0]
        .1
        .max(verts[1].1)
        .max(verts[2].1)
        .min(LCD_HEIGHT as i16 - 1);

    for y in min_y..=max_y {
        let mut min_x = i16::MAX;
        let mut max_x = i16::MIN;

        // Find x intersections with each edge at this scanline
        for i in 0..3 {
            let (x0, y0) = verts[i];
            let (x1, y1) = verts[(i + 1) % 3];

            // Skip horizontal edges or edges that don't cross this scanline
            if y0 == y1 {
                if y == y0 {
                    min_x = min_x.min(x0.min(x1));
                    max_x = max_x.max(x0.max(x1));
                }
                continue;
            }

            if (y < y0.min(y1)) || (y > y0.max(y1)) {
                continue;
            }

            // Linear interpolation: x = x0 + (y - y0) * (x1 - x0) / (y1 - y0)
            let x = x0 + (y - y0) * (x1 - x0) / (y1 - y0);
            min_x = min_x.min(x);
            max_x = max_x.max(x);
        }

        if min_x <= max_x {
            let x0 = min_x.max(0) as usize;
            let x1 = max_x.min(LCD_WIDTH as i16 - 1) as usize;
            let row_base = y as usize * (LCD_WIDTH / 8);
            let first_byte = x0 / 8;
            let last_byte = x1 / 8;

            if first_byte == last_byte {
                // Span fits in one byte
                let mask = (0xFF >> (x0 % 8)) & (0xFF << (7 - x1 % 8));
                frame[row_base + first_byte] |= mask;
            } else {
                // First partial byte
                frame[row_base + first_byte] |= 0xFF >> (x0 % 8);
                // Full middle bytes
                for b in (first_byte + 1)..last_byte {
                    frame[row_base + b] = 0xFF;
                }
                // Last partial byte
                frame[row_base + last_byte] |= 0xFF << (7 - x1 % 8);
            }
        }
    }
}

/// Draw the star ellipse at the center.
fn draw_star(frame: &mut [u8; LCD_BYTES]) {
    for y in -STAR_RADIUS_Y..=STAR_RADIUS_Y {
        let ry2 = STAR_RADIUS_Y * STAR_RADIUS_Y;
        let rx2 = STAR_RADIUS_X * STAR_RADIUS_X;
        let x_max_sq = rx2 * (ry2 - y * y) / ry2;
        let mut x = 0i16;
        while x * x <= x_max_sq {
            set_pixel(frame, STAR_CENTER_X + x, STAR_CENTER_Y + y);
            set_pixel(frame, STAR_CENTER_X - x, STAR_CENTER_Y + y);
            x += 1;
        }
    }
}

/// Build an animated pulsar frame for the given rotation step.
///
/// `step` should be 0..ROTATION_FRAMES-1, cycling to create the animation.
/// Jets are drawn as filled cones (triangles) that reach the screen edges.
///
/// Returns raw content — battery overlay and 180° rotation are applied by the
/// generic VMU writer so they work regardless of content source.
#[must_use]
pub fn build_animated_frame(step: u8) -> [u8; LCD_BYTES] {
    let mut frame = [0u8; LCD_BYTES];
    let idx = (step % ROTATION_FRAMES) as usize;
    let (tdx, tdy, sdx, sdy) = JET_CONES[idx];

    let cx = STAR_CENTER_X;
    let cy = STAR_CENTER_Y;

    // Upper jet cone: triangle from star center to two spread points at tip
    fill_triangle(
        &mut frame,
        [
            (cx, cy),
            (cx + tdx + sdx, cy + tdy + sdy),
            (cx + tdx - sdx, cy + tdy - sdy),
        ],
    );

    // Lower jet cone: mirror
    fill_triangle(
        &mut frame,
        [
            (cx, cy),
            (cx - tdx - sdx, cy - tdy - sdy),
            (cx - tdx + sdx, cy - tdy + sdy),
        ],
    );

    // Draw star on top of jets
    draw_star(&mut frame);

    frame
}
