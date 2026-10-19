pub const BLUE: [u8; 3] = [64, 156, 255];
pub const GREY: [u8; 3] = [150, 154, 166];
pub const AMBER: [u8; 3] = [232, 184, 72];
pub const RED: [u8; 3] = [242, 108, 96];

const SAMPLES: u32 = 4;

fn sample(dx: f32, dy: f32, unit: f32, outer: f32, blade: [u8; 3]) -> [f32; 4] {
    let radius = (dx * dx + dy * dy).sqrt();
    let angle = dy.atan2(dx);
    let [r, g, b] = blade;
    if radius < 3.5 * unit {
        [240.0, 240.0, 240.0, 255.0]
    } else if radius < outer && (angle * 3.0 + radius / (6.0 * unit)).sin() > 0.15 {
        [f32::from(r), f32::from(g), f32::from(b), 255.0]
    } else if radius < outer {
        [30.0, 40.0, 56.0, 255.0]
    } else {
        [0.0, 0.0, 0.0, 0.0]
    }
}

pub fn fan_rgba(size: u32, blade: [u8; 3]) -> Vec<u8> {
    let unit = size as f32 / 32.0;
    let center = size as f32 / 2.0;
    let outer = center - unit;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    let count = (SAMPLES * SAMPLES) as f32;
    for y in 0..size {
        for x in 0..size {
            let mut sum = [0.0_f32; 4];
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let dx = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32 - center;
                    let dy = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32 - center;
                    let [r, g, b, a] = sample(dx, dy, unit, outer, blade);
                    let weight = a / 255.0;
                    sum[0] += r * weight;
                    sum[1] += g * weight;
                    sum[2] += b * weight;
                    sum[3] += a;
                }
            }
            let alpha = sum[3] / count;
            let coverage = (alpha / 255.0).max(f32::EPSILON);
            let channel = |value: f32| (value / count / coverage).round().clamp(0.0, 255.0) as u8;
            rgba.extend_from_slice(&[
                channel(sum[0]),
                channel(sum[1]),
                channel(sum[2]),
                alpha.round().clamp(0.0, 255.0) as u8,
            ]);
        }
    }
    rgba
}
