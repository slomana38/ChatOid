pub const fn accent(agent: bool) -> [u8; 3] {
    if agent {
        [198, 190, 255]
    } else {
        [162, 239, 204]
    }
}

pub fn icon_rgba(agent: bool, size: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    let nodes = [(12., 29.), (24., 16.), (36., 29.)];
    let background = accent(agent);
    let foreground: [u8; 3] = if agent { [21, 18, 33] } else { [13, 20, 22] };
    let scale = 48. / size as f32;
    for y in 0..size {
        for x in 0..size {
            let (x, y) = ((x as f32 + 0.5) * scale, (y as f32 + 0.5) * scale);
            let edge = ((x - 24.).abs() - 12.)
                .max(0.)
                .hypot(((y - 24.).abs() - 12.).max(0.));
            let alpha = ((11.5 - edge) / scale + 0.5).clamp(0., 1.);
            let dot = nodes
                .iter()
                .map(|&(a, b)| (x - a).hypot(y - b) - 3.0)
                .fold(f32::INFINITY, f32::min);
            let line = nodes
                .windows(2)
                .map(|pair| {
                    let ((a, b), (c, d)) = (pair[0], pair[1]);
                    let (dx, dy) = (c - a, d - b);
                    let t = (((x - a) * dx + (y - b) * dy) / (dx * dx + dy * dy)).clamp(0., 1.);
                    (x - a - dx * t).hypot(y - b - dy * t) - 1.35
                })
                .fold(f32::INFINITY, f32::min);
            let stroke = (0.5 - dot.min(line) / scale).clamp(0., 1.);
            for (back, front) in background.into_iter().zip(foreground) {
                rgba.push(
                    (f32::from(back) * (1. - stroke) + f32::from(front) * stroke).round() as u8,
                );
            }
            rgba.push((255. * alpha).round() as u8);
        }
    }
    rgba
}
