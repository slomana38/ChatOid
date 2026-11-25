use crate::identity::Role;

use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, RichText, Shape, Stroke, Ui, Vec2, pos2, vec2,
};

pub const INK: Color32 = Color32::from_rgb(237, 246, 242);
pub const MUTED: Color32 = Color32::from_rgb(167, 181, 185);
pub const INVALID: Color32 = Color32::from_rgb(239, 111, 124);

pub fn install_auth_font(ctx: &egui::Context) {
    #[cfg(windows)]
    let mut paths = std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .into_iter()
        .map(|dir| dir.join("Fonts").join("seguisb.ttf"));
    #[cfg(not(windows))]
    let mut paths = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    ]
    .into_iter()
    .map(std::path::PathBuf::from);
    let Some(bytes) = paths.find_map(|path| std::fs::read(path).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut family = fonts.families[&egui::FontFamily::Proportional].clone();
    fonts.font_data.insert(
        "auth-semibold".into(),
        egui::FontData::from_owned(bytes).into(),
    );
    family.insert(0, "auth-semibold".into());
    fonts
        .families
        .insert(egui::FontFamily::Name("auth-semibold".into()), family);
    ctx.set_fonts(fonts);
    ctx.data_mut(|data| data.insert_temp(egui::Id::new("chatoid-auth-font"), true));
}

pub fn auth_text<'a>(ui: &Ui, value: &'a mut String, hint: &str) -> egui::TextEdit<'a> {
    let custom = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(egui::Id::new("chatoid-auth-font")))
        .unwrap_or(false);
    let family = if custom {
        egui::FontFamily::Name("auth-semibold".into())
    } else {
        egui::FontFamily::Proportional
    };
    let font = FontId::new(15., family);
    egui::TextEdit::singleline(value)
        .font(font.clone())
        .hint_text(RichText::new(hint).font(font).color(MUTED))
        .horizontal_align(egui::Align::Center)
        .vertical_align(egui::Align::Center)
}

pub fn field_rules(ui: &Ui, field: &egui::Response, invalid: bool, rule: &str) {
    if invalid {
        ui.painter().rect_stroke(
            field.rect,
            8.,
            Stroke::new(1.5_f32, INVALID),
            egui::StrokeKind::Inside,
        );
    }
    if ui.is_rect_visible(field.rect) && (field.hovered() || field.has_focus()) {
        egui::Popup::from_response(field)
            .kind(egui::PopupKind::Tooltip)
            .sense(egui::Sense::empty())
            .align(egui::RectAlign::RIGHT_START)
            .align_alternatives(&[egui::RectAlign::LEFT_START, egui::RectAlign::TOP_END])
            .gap(7.)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(8, 9, 11))
                    .stroke(Stroke::new(
                        1_f32,
                        if invalid {
                            INVALID
                        } else {
                            Color32::from_gray(54)
                        },
                    ))
                    .corner_radius(7.)
                    .inner_margin(8),
            )
            .show(|ui| {
                ui.set_max_width(155.);
                ui.label(RichText::new(rule).size(12.).color(INK));
            });
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub base: Color32,
    pub panel: Color32,
    pub raised: Color32,
    pub border: Color32,
    pub accent: Color32,
    pub tint: Color32,
    pub chrome: Color32,
}

pub fn palette(role: Role) -> Palette {
    let colors = match role {
        Role::Human => [
            [20, 23, 25],
            [27, 31, 33],
            [35, 40, 42],
            [53, 62, 64],
            crate::branding::accent(false),
            [39, 67, 59],
            [29, 35, 36],
        ],
        Role::Agent => [
            [23, 22, 27],
            [31, 29, 36],
            [41, 38, 47],
            [61, 56, 72],
            crate::branding::accent(true),
            [60, 51, 76],
            [35, 32, 42],
        ],
    };
    let [base, panel, raised, border, accent, tint, chrome] =
        colors.map(|[r, g, b]| Color32::from_rgb(r, g, b));
    Palette {
        base,
        panel,
        raised,
        border,
        accent,
        tint,
        chrome,
    }
}

fn current(ctx: &egui::Context) -> Palette {
    ctx.data(|data| data.get_temp::<Palette>(egui::Id::new("chatoid-palette")))
        .unwrap_or_else(|| palette(Role::Human))
}

pub fn theme(ctx: &egui::Context, role: Role) {
    let colors = palette(role);
    ctx.data_mut(|data| data.insert_temp(egui::Id::new("chatoid-palette"), colors));
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = colors.base;
    visuals.window_fill = colors.panel;
    visuals.extreme_bg_color = colors.base;
    visuals.faint_bg_color = colors.panel;
    visuals.override_text_color = Some(INK);
    visuals.weak_text_color = Some(MUTED);
    visuals.hyperlink_color = colors.accent;
    visuals.selection.bg_fill = colors.tint;
    visuals.selection.stroke = Stroke::new(1_f32, colors.accent);
    visuals.widgets.inactive.bg_fill = colors.raised;
    visuals.widgets.inactive.weak_bg_fill = colors.raised;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1_f32, colors.border);
    visuals.widgets.hovered.bg_fill = colors.tint;
    visuals.widgets.hovered.weak_bg_fill = colors.tint;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1_f32, colors.accent.gamma_multiply(0.7));
    visuals.widgets.active.bg_fill = colors.tint;
    visuals.widgets.active.weak_bg_fill = colors.tint;
    visuals.widgets.active.bg_stroke = Stroke::new(1_f32, colors.accent);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1_f32, colors.border);
    visuals.window_corner_radius = 14.into();
    for widgets in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widgets.corner_radius = 8.into();
    }
    style.visuals = visuals;
    style.spacing.item_spacing = vec2(10., 8.);
    style.spacing.button_padding = vec2(12., 7.);
    style.spacing.interact_size.y = 32.;
    style.spacing.text_edit_width = 280.;
    ctx.set_style(style);
}

pub fn primary<'a>(ui: &Ui, label: &'a str) -> egui::Button<'a> {
    let colors = current(ui.ctx());
    egui::Button::new(RichText::new(label).strong().color(colors.base))
        .fill(colors.accent)
        .stroke(Stroke::new(1_f32, colors.accent))
        .corner_radius(8.)
}

pub fn password_eye(ui: &mut Ui, rect: Rect, revealed: bool) -> egui::Response {
    let response = ui.put(
        rect,
        egui::Button::new("").min_size(rect.size()).frame(false),
    );
    let label = if revealed {
        "Parolayı gizle"
    } else {
        "Parolayı göster"
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let colors = current(ui.ctx());
    let color = if response.hovered() || response.has_focus() || revealed {
        colors.accent
    } else {
        MUTED
    };
    let center = response.rect.center();
    let left = center - vec2(8., 0.);
    let right = center + vec2(8., 0.);
    let stroke = Stroke::new(1.4_f32, color);
    let draw = ui.painter();
    draw.add(egui::epaint::QuadraticBezierShape::from_points_stroke(
        [left, center + vec2(0., -10.), right],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
    draw.add(egui::epaint::QuadraticBezierShape::from_points_stroke(
        [left, center + vec2(0., 10.), right],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
    draw.circle_filled(center, 2.2, color);
    if revealed {
        draw.line_segment([center + vec2(-8., 7.), center + vec2(8., -7.)], stroke);
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(label)
}

pub fn app_icon(role: Role) -> egui::IconData {
    let size = 48;
    egui::IconData {
        rgba: crate::branding::icon_rgba(role == Role::Agent, size),
        width: size,
        height: size,
    }
}

pub fn brand(ui: &mut Ui, label: &str) {
    brand_mark(ui, label, 32., 20.);
}

pub fn workspace_brand(ui: &mut Ui, label: &str) {
    brand_mark(ui, label, 26., 17.);
}

fn brand_mark(ui: &mut Ui, label: &str, side: f32, title_size: f32) {
    let colors = current(ui.ctx());
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), egui::Sense::hover());
        ui.painter().rect_filled(rect, 8., colors.accent);
        glyph(ui.painter(), rect.center(), side * 0.28, colors.base);
        ui.label(
            RichText::new(label)
                .size(title_size)
                .strong()
                .color(colors.accent),
        );
    });
}

fn glyph(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let a = center + vec2(-radius, radius * 0.54);
    let b = center + vec2(0., -radius * 0.66);
    let c = center + vec2(radius, radius * 0.54);
    painter.add(Shape::line(
        vec![a, b, c],
        Stroke::new(radius * 0.14, color),
    ));
    for p in [a, b, c] {
        painter.circle_filled(p, radius * 0.19, color);
    }
}

pub fn network(ui: &mut Ui, size: Vec2, role: Role, activity: Option<f64>) -> Option<Role> {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let paint = ui.painter_at(rect.shrink(1.));
    let colors = palette(role);
    let accent = colors.accent;
    paint.rect_filled(rect, 18., colors.panel);
    let center = pos2(rect.center().x, rect.top() + rect.height() * 0.54);
    let scale = (rect.width().min(rect.height()) * 0.32).min(165.);
    for y in 1..(rect.height() / 22.) as u32 {
        for x in 1..(rect.width() / 22.) as u32 {
            paint.circle_filled(
                rect.min + vec2(x as f32 * 22., y as f32 * 22.),
                0.9,
                colors.border,
            );
        }
    }
    for ring in [0.45, 0.73, 1., 1.21] {
        paint.circle_stroke(center, scale * ring, Stroke::new(1_f32, colors.border));
    }
    let time = ui.input(|input| input.time) as f32;
    let angle = time * 0.34;
    for (radius, phase, tick_count) in [(scale * 0.74, -angle, 44_u32), (scale * 1.14, angle, 64)] {
        for tick in 0..tick_count {
            let theta = phase + std::f32::consts::TAU * tick as f32 / tick_count as f32;
            let ray = vec2(theta.cos(), theta.sin());
            let graduated = tick % 4 == 0;
            paint.line_segment(
                [
                    center + ray * (radius - if graduated { 5. } else { 2. }),
                    center + ray * radius,
                ],
                Stroke::new(
                    1_f32,
                    accent.gamma_multiply(if graduated { 0.65 } else { 0.29 }),
                ),
            );
        }
    }
    for index in 0..3 {
        let start = angle + std::f32::consts::TAU * index as f32 / 3.;
        let points = (0..=20)
            .map(|step| {
                let theta = start + step as f32 * 0.042;
                center + vec2(theta.cos(), theta.sin()) * (scale * 1.04)
            })
            .collect();
        paint.add(Shape::line(
            points,
            Stroke::new(2_f32, accent.gamma_multiply(0.55)),
        ));
    }
    if ui.is_rect_visible(rect) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
    }
    let people = center + vec2(-scale * 0.86, -scale * 0.53);
    let agent = center + vec2(scale * 0.86, scale * 0.50);
    let curve = egui::epaint::CubicBezierShape::from_points_stroke(
        [
            people,
            people + vec2(scale * 0.92, -scale * 0.27),
            agent + vec2(-scale * 0.81, scale * 0.40),
            agent,
        ],
        false,
        Color32::TRANSPARENT,
        Stroke::new(2_f32, accent.gamma_multiply(0.55)),
    );
    let elapsed = activity.map(|started| ui.input(|i| i.time) - started);
    if let Some(elapsed) = elapsed.filter(|elapsed| (0. ..0.72).contains(elapsed)) {
        let t = 1. - (1. - (elapsed / 0.72) as f32).powi(2);
        let point = curve.sample(if role == Role::Agent { t } else { 1. - t });
        paint.circle_filled(point, 13., accent.gamma_multiply(0.27));
        paint.circle_filled(point, 4.5, accent);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(16));
    }
    paint.add(curve);
    paint.circle_filled(center, 37., colors.base);
    paint.circle_stroke(center, 37., Stroke::new(1_f32, accent));
    glyph(&paint, center, 17., accent);
    paint.text(
        rect.left_top() + vec2(22., 24.),
        Align2::LEFT_TOP,
        "Hesap türünü seç",
        FontId::proportional(22.),
        INK,
    );
    let mut choice = None;
    for (point, target, label, method) in [
        (people, Role::Human, "İnsan", "Kullanıcı adı ve parola"),
        (agent, Role::Agent, "Ajan", ".key dosyası"),
    ] {
        let tile = Rect::from_center_size(point, vec2(149., 70.));
        let id = ui.id().with(("role", target == Role::Agent));
        let response = ui.interact(tile, id, egui::Sense::click());
        let selected = role == target;
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::SelectableLabel,
                true,
                selected,
                format!("{label} hesabı, {method}"),
            )
        });
        let hovered = response.hovered();
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked() && !selected {
            choice = Some(target);
        }
        let glow = ui.ctx().animate_bool_with_time(
            id.with("glow"),
            selected || hovered || response.has_focus(),
            0.16,
        );
        let color = palette(target).accent;
        if glow > 0. {
            paint.rect_filled(tile.expand(5.), 17., color.gamma_multiply(glow * 0.15));
        }
        paint.rect_filled(
            tile,
            13.,
            if selected { colors.raised } else { colors.base },
        );
        paint.rect_stroke(
            tile,
            13.,
            Stroke::new(
                1_f32,
                if selected || hovered || response.has_focus() {
                    color
                } else {
                    colors.border
                },
            ),
            egui::StrokeKind::Inside,
        );
        let symbol = tile.left_center() + vec2(20., 0.);
        paint.circle_filled(symbol, 13., color.gamma_multiply(0.23));
        if target == Role::Human {
            paint.circle_filled(symbol + vec2(0., -3.), 3., color);
            paint.line_segment(
                [symbol + vec2(-5., 5.), symbol + vec2(5., 5.)],
                Stroke::new(2_f32, color),
            );
        } else {
            paint.circle_stroke(symbol + vec2(-3., -2.), 3.5, Stroke::new(1.4_f32, color));
            paint.line_segment([symbol, symbol + vec2(6., 6.)], Stroke::new(1.4_f32, color));
            paint.line_segment(
                [symbol + vec2(3., 4.), symbol + vec2(6., 1.)],
                Stroke::new(1.4_f32, color),
            );
        }
        let origin = tile.left_top() + vec2(39., 16.);
        paint.text(
            origin,
            Align2::LEFT_TOP,
            label,
            FontId::proportional(17.),
            if selected { color } else { INK },
        );
        paint.text(
            origin + vec2(0., 25.),
            Align2::LEFT_TOP,
            method,
            FontId::proportional(11.),
            MUTED,
        );
        if selected {
            paint.circle_filled(tile.right_top() + vec2(-11., 11.), 3., color);
        }
    }
    paint.text(
        rect.left_bottom() + vec2(22., -21.),
        Align2::LEFT_BOTTOM,
        if role == Role::Human {
            "İnsan girişi seçili"
        } else {
            "Ajan girişi seçili"
        },
        FontId::proportional(13.),
        accent,
    );
    paint.rect_stroke(
        rect,
        18.,
        Stroke::new(1_f32, colors.border),
        egui::StrokeKind::Inside,
    );
    choice
}

pub fn avatar(ui: &mut Ui, name: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    draw_avatar(ui.painter(), rect, name, current(ui.ctx()));
}

fn draw_avatar(painter: &egui::Painter, rect: Rect, name: &str, colors: Palette) {
    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    painter.circle_filled(rect.center(), rect.width() * 0.5, colors.tint);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        initial,
        FontId::proportional(15.),
        colors.accent,
    );
}

pub fn chat_row(
    ui: &mut Ui,
    name: &str,
    preview: &str,
    time: &str,
    unread: usize,
    targeted: bool,
    selected: bool,
) -> egui::Response {
    let colors = current(ui.ctx());
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), 53.), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, name)
    });
    let fill = if selected {
        colors.tint
    } else if response.hovered() {
        colors.raised
    } else {
        colors.panel
    };
    let paint = ui.painter_at(rect);
    paint.rect_filled(rect, 10., fill);
    if response.has_focus() {
        paint.rect_stroke(
            rect,
            10.,
            Stroke::new(1_f32, colors.accent.gamma_multiply(0.65)),
            egui::StrokeKind::Inside,
        );
    }
    if selected {
        paint.rect_filled(
            Rect::from_center_size(rect.left_center() + vec2(2., 0.), vec2(2., 24.)),
            1.,
            colors.accent,
        );
    }
    draw_avatar(
        &paint,
        Rect::from_center_size(pos2(rect.left() + 24., rect.center().y), Vec2::splat(32.)),
        name,
        colors,
    );
    let x = rect.left() + 47.;
    let name_width = (rect.width() - 100.).max(28.);
    let mut title =
        egui::text::LayoutJob::simple(name.to_owned(), FontId::proportional(14.), INK, name_width);
    title.wrap.max_rows = 1;
    paint.galley(pos2(x, rect.top() + 8.), paint.layout_job(title), INK);
    let mut detail = egui::text::LayoutJob::simple(
        preview.to_owned(),
        FontId::proportional(12.),
        MUTED,
        (name_width + 28.).max(30.),
    );
    detail.wrap.max_rows = 1;
    paint.galley(pos2(x, rect.top() + 28.), paint.layout_job(detail), MUTED);
    paint.text(
        rect.right_top() + vec2(-9., 8.),
        Align2::RIGHT_TOP,
        time,
        FontId::proportional(11.),
        MUTED,
    );
    if unread > 0 {
        let count = if targeted {
            format!("@{unread}")
        } else {
            unread.to_string()
        };
        paint.text(
            rect.right_bottom() + vec2(-9., -8.),
            Align2::RIGHT_BOTTOM,
            count,
            FontId::proportional(12.),
            colors.accent,
        );
    }
    response
}

pub fn chat_background(ui: &Ui, rect: Rect) {
    let colors = current(ui.ctx());
    let paint = ui.painter_at(rect);
    let dot = colors.border.gamma_multiply(0.16);
    for y in 1..(rect.height() / 28.) as u32 {
        for x in 1..(rect.width() / 28.) as u32 {
            let position = rect.min
                + vec2(
                    x as f32 * 28. + if y % 2 == 0 { 9. } else { 0. },
                    y as f32 * 28.,
                );
            paint.circle_filled(position, 0.8, dot);
        }
    }
}

pub fn wallpaper(ui: &Ui, rect: Rect, settings: &super::preferences::Wallpaper) {
    use super::preferences::Background;
    if !rect.is_positive() {
        return;
    }
    let paint = ui.painter_at(rect);
    match settings.background {
        Background::Orbit => {
            paint.rect_filled(rect, 0., current(ui.ctx()).base);
            chat_background(ui, rect);
        }
        Background::Plain => {
            let [r, g, b] = settings.color;
            paint.rect_filled(rect, 0., Color32::from_rgb(r, g, b));
        }
        Background::Photo => {
            paint.rect_filled(rect, 0., current(ui.ctx()).base);
            if let Some(path) = &settings.photo {
                let id = egui::Id::new(("wallpaper", path));
                let existing = ui
                    .ctx()
                    .data(|data| data.get_temp::<egui::TextureHandle>(id));
                let texture = existing.or_else(|| {
                    let image = image::open(path).ok()?.into_rgba8();
                    let cg = egui::ColorImage::from_rgba_unmultiplied(
                        [image.width() as usize, image.height() as usize],
                        image.as_raw(),
                    );
                    let texture = ui.ctx().load_texture(
                        "Sohbet arka planı",
                        cg,
                        egui::TextureOptions::LINEAR,
                    );
                    ui.ctx()
                        .data_mut(|data| data.insert_temp(id, texture.clone()));
                    Some(texture)
                });
                if let Some(texture) = texture {
                    let size = texture.size_vec2();
                    let scale = (rect.width() / size.x).max(rect.height() / size.y);
                    paint.image(
                        texture.id(),
                        Rect::from_center_size(rect.center(), size * scale),
                        Rect::from_min_max(pos2(0., 0.), pos2(1., 1.)),
                        Color32::WHITE,
                    );
                }
            }
        }
    }
    paint.rect_filled(
        rect,
        0.,
        Color32::from_black_alpha((u16::from(settings.dim) * 255 / 100) as u8),
    );
}

pub fn draft_image(ui: &mut Ui, path: &std::path::Path) {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "png" | "jpg" | "jpeg") {
        return;
    }
    let id = egui::Id::new(("draft-image", path));
    let cached = ui
        .ctx()
        .data(|data| data.get_temp::<Option<egui::TextureHandle>>(id));
    let texture = cached.unwrap_or_else(|| {
        let load = || {
            let mut reader = image::ImageReader::open(path)
                .ok()?
                .with_guessed_format()
                .ok()?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(6400);
            limits.max_image_height = Some(6400);
            limits.max_alloc = Some(128 * 1024 * 1024);
            reader.limits(limits);
            let image = reader.decode().ok()?.thumbnail(96, 56).to_rgba8();
            let color = egui::ColorImage::from_rgba_unmultiplied(
                [image.width() as usize, image.height() as usize],
                image.as_raw(),
            );
            Some(
                ui.ctx()
                    .load_texture("Ek önizlemesi", color, egui::TextureOptions::LINEAR),
            )
        };
        let loaded = load();
        ui.ctx()
            .data_mut(|data| data.insert_temp(id, loaded.clone()));
        loaded
    });
    if let Some(texture) = texture {
        ui.image((texture.id(), texture.size_vec2()));
    }
}

const BUBBLE_PADDING: f32 = 9.;
const MESSAGE_SIZE: f32 = 14.;
const STAMP_SIZE: f32 = 10.5;

pub struct BubbleSize {
    width: f32,
    inline_stamp: bool,
}

pub fn measure_bubble(
    ui: &Ui,
    body: &str,
    stamp: &str,
    extra_width: f32,
    checks: u8,
) -> BubbleSize {
    let width_of = |text: &str, size| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), FontId::proportional(size), INK)
            .size()
            .x
    };
    let body_width = body
        .lines()
        .map(|line| width_of(line, MESSAGE_SIZE))
        .fold(0_f32, f32::max);
    let stamp_width = width_of(stamp, STAMP_SIZE) + if checks > 0 { 18. } else { 0. };
    let lane = ui.available_width();
    let maximum = (lane * 0.70).clamp(1., 414.);
    let inline_width = body_width + stamp_width + 10.;
    let inline_stamp = !body.contains('\n') && inline_width <= maximum;
    let content = if inline_stamp {
        inline_width
    } else {
        body_width.max(stamp_width)
    };
    BubbleSize {
        width: content.max(extra_width).max(46.).min(maximum).ceil(),
        inline_stamp,
    }
}

pub fn bubble_lane(
    ui: &mut Ui,
    mine: bool,
    size: &BubbleSize,
    replies: bool,
    draw: impl FnOnce(&mut Ui),
) -> (egui::Response, bool) {
    let colors = current(ui.ctx());
    let width = ui.available_width();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.;
        let mut reply = false;
        let mut reply_button = None;
        if mine {
            let action_width = if replies { 28. } else { 0. };
            ui.add_space((width - size.width - BUBBLE_PADDING * 2. - action_width - 6.).max(0.));
            if replies {
                reply_button = Some(chat_button(ui, false, true, false));
            }
        }
        let frame = egui::Frame::new()
            .fill(if mine { colors.tint } else { colors.panel })
            .corner_radius(egui::CornerRadius {
                nw: if mine { 11 } else { 4 },
                ne: if mine { 4 } else { 11 },
                sw: 11,
                se: 11,
            })
            .inner_margin(egui::Margin::symmetric(BUBBLE_PADDING as i8, 6))
            .show(ui, |ui| {
                ui.set_width(size.width);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(size.width);
                    ui.spacing_mut().item_spacing.y = 3.;
                    draw(ui);
                });
            });
        if !mine && replies {
            reply_button = Some(chat_button(ui, false, true, false));
        }
        if let Some(button) = reply_button {
            let hovered = ui
                .input(|input| input.pointer.hover_pos())
                .is_some_and(|position| frame.response.rect.union(button.rect).contains(position));
            if hovered || button.has_focus() {
                paint_chat_icon(ui, &button, false, true);
            }
            reply = button.clicked();
        }
        (frame.response, reply)
    })
    .inner
}

pub fn participant_avatar(
    ui: &mut Ui,
    card: &crate::identity::Card,
    photo: Option<&crate::protocol::AvatarPhoto>,
) -> egui::Response {
    profile_avatar(ui, card, photo, 28.)
}

pub fn profile_avatar(
    ui: &mut Ui,
    card: &crate::identity::Card,
    photo: Option<&crate::protocol::AvatarPhoto>,
    size: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::click());
    let accent = participant_color(&card.data.key);
    let initials: String = card
        .data
        .name
        .split_whitespace()
        .take(2)
        .filter_map(|word| word.chars().next())
        .flat_map(char::to_uppercase)
        .collect();
    if let Some(texture) = photo.and_then(|photo| avatar_texture(ui.ctx(), &card.data.key, photo)) {
        let mut mesh = egui::Mesh::with_texture(texture.id());
        mesh.vertices.push(egui::epaint::Vertex {
            pos: rect.center(),
            uv: pos2(0.5, 0.5),
            color: Color32::WHITE,
        });
        let segments = 32;
        for index in 0..segments {
            let angle = index as f32 * std::f32::consts::TAU / segments as f32;
            let v = vec2(angle.cos(), angle.sin());
            mesh.vertices.push(egui::epaint::Vertex {
                pos: rect.center() + v * size / 2.,
                uv: pos2(0.5 + v.x / 2., 0.5 + v.y / 2.),
                color: Color32::WHITE,
            });
            mesh.add_triangle(0, index + 1, (index + 1) % segments + 1);
        }
        ui.painter().add(Shape::mesh(mesh));
    } else {
        ui.painter()
            .circle_filled(rect.center(), size / 2., accent.gamma_multiply(0.28));
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            initials,
            FontId::proportional(size * 0.38),
            accent,
        );
    }
    response
        .on_hover_text(format!("{} · @{}", card.data.name, card.data.handle))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn profile_hero(ui: &mut Ui, draw: impl FnOnce(&mut Ui)) {
    let colors = current(ui.ctx());
    let ornament = ui.painter().add(Shape::Noop);
    let card_fill = ui.painter().add(Shape::Noop);
    let width = ui.available_width();
    let result = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(17, 18))
        .show(ui, |ui| {
            ui.set_min_width((width - 34.).max(0.));
            draw(ui);
        });
    let rect = result.response.rect;
    let painter = ui.painter();
    painter.set(ornament, Shape::rect_filled(rect, 18., colors.tint));
    let mut rings = Vec::new();
    let center = rect.right_center() - vec2(41., 7.);
    let clip = rect.shrink(2.);
    let ring = Stroke::new(1_f32, colors.accent.gamma_multiply(0.19));
    for radius in [22_f32, 33., 43.] {
        if rect.height() >= 90. || radius < 40. {
            let radius = radius.min((rect.height() - 8.) / 2.);
            rings.push(Shape::circle_stroke(center, radius.max(1.), ring));
        }
    }
    if clip.is_positive() {
        painter
            .with_clip_rect(clip)
            .set(card_fill, Shape::Vec(rings));
    }
    painter.rect_stroke(
        rect,
        18.,
        Stroke::new(1_f32, colors.accent.gamma_multiply(0.27)),
        egui::StrokeKind::Inside,
    );
}

pub fn profile_section(ui: &mut Ui, icon: Outline, title: &str, draw: impl FnOnce(&mut Ui)) {
    let colors = current(ui.ctx());
    let width = ui.available_width();
    egui::Frame::new()
        .fill(colors.panel)
        .stroke(Stroke::new(1_f32, colors.border.gamma_multiply(0.76)))
        .corner_radius(14)
        .inner_margin(egui::Margin::symmetric(15, 13))
        .show(ui, |ui| {
            ui.set_min_width((width - 32.).max(0.));
            label_icon(ui, icon, RichText::new(title).size(14.).strong());
            ui.add_space(4.);
            draw(ui);
        });
}

pub fn profile_action(
    ui: &mut Ui,
    icon: Outline,
    title: &str,
    detail: &str,
    expanded: bool,
) -> egui::Response {
    let colors = current(ui.ctx());
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, 53.), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, title));
    let painter = ui.painter();
    let active = response.hovered() || response.has_focus() || expanded;
    if active {
        painter.rect_filled(rect, 10., colors.raised);
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect,
            10.,
            Stroke::new(1_f32, colors.accent),
            egui::StrokeKind::Inside,
        );
    }
    let center = rect.left_center() + vec2(21., 0.);
    painter.rect_filled(
        Rect::from_center_size(center, Vec2::splat(29.)),
        9.,
        colors.tint,
    );
    outline(
        painter,
        icon,
        Rect::from_center_size(center, Vec2::splat(18.)),
        colors.accent,
    );
    for (text, size, vertical_offset, ink) in [(title, 13.5, -16., INK), (detail, 11.5, 3., MUTED)]
    {
        let mut layout = egui::text::LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(size),
            ink,
            (width - 77.).max(20.),
        );
        layout.wrap.max_rows = 1;
        let caption = ui.fonts_mut(|fonts| fonts.layout_job(layout));
        painter.galley(
            pos2(rect.left() + 46., rect.center().y + vertical_offset),
            caption,
            ink,
        );
    }
    let caret = rect.right_center() - vec2(14., 0.);
    let points = if expanded {
        [vec2(1.5, -3.5), vec2(-2., 0.), vec2(1.5, 3.5)]
    } else {
        [vec2(-1.5, -3.5), vec2(2., 0.), vec2(-1.5, 3.5)]
    };
    painter.add(Shape::line(
        points.map(|point| caret + point).to_vec(),
        Stroke::new(1.25_f32, if active { colors.accent } else { MUTED }),
    ));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn account_menu(ui: &mut Ui, name: &str, contents: impl FnOnce(&mut Ui)) {
    let colors = current(ui.ctx());
    let caption = ui
        .painter()
        .layout_no_wrap(name.to_owned(), FontId::proportional(13.), INK);
    let button = egui::Button::new("")
        .min_size(vec2(caption.size().x + 43., 34.))
        .corner_radius(9)
        .fill(colors.raised);
    let response = egui::containers::menu::MenuButton::from_button(button)
        .ui(ui, contents)
        .0;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name));
    let rect = response.rect;
    outline(
        ui.painter(),
        Outline::People,
        Rect::from_center_size(rect.left_center() + vec2(12., 0.), Vec2::splat(13.)),
        colors.accent,
    );
    ui.painter().galley(
        pos2(rect.left() + 22., rect.center().y - caption.size().y / 2.),
        caption,
        INK,
    );
    let caret = rect.right_center() - vec2(11., 0.);
    ui.painter().add(Shape::line(
        vec![
            caret + vec2(-3., -1.5),
            caret + vec2(0., 1.5),
            caret + vec2(3., -1.5),
        ],
        Stroke::new(1.25_f32, colors.accent),
    ));
}

fn avatar_texture(
    ctx: &egui::Context,
    key: &str,
    photo: &crate::protocol::AvatarPhoto,
) -> Option<egui::TextureHandle> {
    let id = egui::Id::new(("participant-photo", key));
    if let Some((digest, texture)) =
        ctx.data(|data| data.get_temp::<(String, egui::TextureHandle)>(id))
        && digest == photo.digest
    {
        return Some(texture);
    }
    let image = crate::protocol::AvatarPhoto::decode(&photo.data)
        .ok()?
        .into_rgba8();
    let rgba = egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    );
    let texture = ctx.load_texture(format!("avatar-{key}"), rgba, egui::TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(id, (photo.digest.clone(), texture.clone())));
    Some(texture)
}

pub fn attachment_preview(
    ui: &mut Ui,
    preview: &crate::protocol::AttachmentPreview,
    key: impl std::hash::Hash,
) -> Option<egui::Response> {
    if !(1..=crate::protocol::AttachmentPreview::MAX_DIMENSION).contains(&preview.width)
        || !(1..=crate::protocol::AttachmentPreview::MAX_DIMENSION).contains(&preview.height)
    {
        return None;
    }
    let width = (preview.width as f32).min(ui.available_width());
    let height = width * preview.height as f32 / preview.width as f32;
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return Some(response);
    }
    let cache = egui::Id::new(("attachment-preview", key));
    let texture = ui
        .ctx()
        .data(|data| data.get_temp::<egui::TextureHandle>(cache))
        .or_else(|| {
            let image = preview.decode().ok()?.into_rgba8();
            let rgba = egui::ColorImage::from_rgba_unmultiplied(
                [image.width() as usize, image.height() as usize],
                image.as_raw(),
            );
            let texture = ui.ctx().load_texture(
                format!("attachment-{cache:?}"),
                rgba,
                egui::TextureOptions::LINEAR,
            );
            ui.ctx()
                .data_mut(|data| data.insert_temp(cache, texture.clone()));
            Some(texture)
        });
    if let Some(texture) = texture {
        ui.painter().image(
            texture.id(),
            rect,
            Rect::from_min_max(pos2(0., 0.), pos2(1., 1.)),
            Color32::WHITE,
        );
    }
    Some(
        response
            .on_hover_text("Resmi büyük görüntüle")
            .on_hover_cursor(egui::CursorIcon::ZoomIn),
    )
}

pub fn participant_color(key: &str) -> Color32 {
    let colors = [
        Color32::from_rgb(157, 226, 203),
        Color32::from_rgb(154, 199, 248),
        Color32::from_rgb(211, 180, 247),
        Color32::from_rgb(244, 196, 145),
        Color32::from_rgb(244, 172, 190),
        Color32::from_rgb(205, 225, 151),
    ];
    let hash = key.bytes().fold(0_usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte.into())
    });
    colors[hash % colors.len()]
}

pub fn message_stamp(ui: &mut Ui, stamp: &str, title: &str, ink: Color32, checks: u8) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.;
        let time = |ui: &mut Ui| {
            ui.label(RichText::new(stamp).size(STAMP_SIZE).color(MUTED))
                .on_hover_text(title);
        };
        let mark = |ui: &mut Ui| {
            if checks == 0 {
                return;
            }
            let (rect, response) = ui.allocate_exact_size(vec2(14., 12.), egui::Sense::hover());
            let origin = rect.left_center();
            let painter = ui.painter();
            if checks > 1 {
                painter.add(Shape::line(
                    vec![
                        origin + vec2(6., 1.5),
                        origin + vec2(7.5, 3.),
                        origin + vec2(13., -3.5),
                    ],
                    Stroke::new(1.35_f32, ink),
                ));
            }
            painter.add(Shape::line(
                vec![
                    origin + vec2(1., 0.5),
                    origin + vec2(3.5, 3.),
                    origin + vec2(9., -3.5),
                ],
                Stroke::new(1.35_f32, ink),
            ));
            response.on_hover_text(title);
        };
        if ui.layout().main_dir() == egui::Direction::RightToLeft {
            mark(ui);
            time(ui);
        } else {
            time(ui);
            mark(ui);
        }
    });
}

pub fn bubble_text(
    ui: &mut Ui,
    size: &BubbleSize,
    body: &str,
    stamp: &str,
    title: &str,
    color: Color32,
    checks: u8,
) {
    let time = |ui: &mut Ui| message_stamp(ui, stamp, title, color, checks);
    if size.inline_stamp {
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(body).size(MESSAGE_SIZE)).extend());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), time);
        });
    } else {
        ui.add(egui::Label::new(RichText::new(body).size(MESSAGE_SIZE)).wrap());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), time);
    }
}

pub fn chat_icon_button(ui: &mut Ui, send: bool, enabled: bool) -> egui::Response {
    chat_button(ui, send, enabled, true)
}

fn chat_button(ui: &mut Ui, send: bool, enabled: bool, draw: bool) -> egui::Response {
    let colors = current(ui.ctx());
    let (size, label) = if send {
        (36., "Gönder")
    } else {
        (28., "Yanıtla")
    };
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("")
            .min_size(Vec2::splat(size))
            .frame(send)
            .fill(if send {
                colors.accent
            } else {
                Color32::TRANSPARENT
            })
            .stroke(Stroke::NONE)
            .corner_radius(if send { 10 } else { 6 }),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    if draw {
        paint_chat_icon(ui, &response, send, enabled);
    }
    response
        .on_hover_text(label)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn paint_chat_icon(ui: &Ui, response: &egui::Response, send: bool, enabled: bool) {
    let colors = current(ui.ctx());
    let center = response.rect.center();
    let ink = if send {
        colors.base
    } else if response.hovered() || response.has_focus() {
        colors.accent
    } else {
        INK
    };
    let stroke = Stroke::new(
        1.8_f32,
        if enabled {
            ink
        } else {
            ink.gamma_multiply(0.55)
        },
    );
    let coordinates = if send {
        [vec2(-7., -6.), vec2(7., 0.), vec2(-7., 6.), vec2(-4., 0.)]
    } else {
        [vec2(-7., -2.), vec2(1., -2.), vec2(5., 1.), vec2(5., 6.)]
    };
    ui.painter().add(Shape::line(
        coordinates.iter().map(|offset| center + *offset).collect(),
        stroke,
    ));
    let arrow = if send {
        [vec2(-4., 0.), vec2(1., 0.), vec2(7., 0.)]
    } else {
        [vec2(-2., -7.), vec2(-7., -2.), vec2(-2., 3.)]
    };
    ui.painter().add(Shape::line(
        arrow.iter().map(|offset| center + *offset).collect(),
        stroke,
    ));
}

#[derive(Clone, Copy)]
pub enum Outline {
    Edit,
    Loop,
    Goal,
    Settings,
    Chat,
    Archive,
    People,
    Host,
    Agent,
    Lock,
    Link,
    Plus,
    Copy,
    Search,
    Check,
    Cross,
    Attachment,
    Emoji,
}

fn outline(paint: &egui::Painter, icon: Outline, rect: Rect, ink: Color32) {
    let at = |x, y| {
        pos2(
            rect.left() + rect.width() * x,
            rect.top() + rect.height() * y,
        )
    };
    let line = Stroke::new((rect.width() * 0.082).max(1.), ink);
    let trace = |points: &[(f32, f32)]| {
        paint.add(Shape::line(
            points.iter().map(|&(x, y)| at(x, y)).collect(),
            line,
        ));
    };
    let box_stroke = |left, top, right, bottom| {
        paint.rect_stroke(
            Rect::from_min_max(at(left, top), at(right, bottom)),
            rect.width() * 0.1,
            line,
            egui::StrokeKind::Inside,
        );
    };
    match icon {
        Outline::Loop => {
            paint.circle_stroke(at(0.50, 0.50), rect.width() * 0.32, line);
            trace(&[(0.63, 0.18), (0.80, 0.18), (0.80, 0.35)]);
            trace(&[(0.37, 0.82), (0.20, 0.82), (0.20, 0.65)]);
        }
        Outline::Goal => {
            paint.circle_stroke(at(0.50, 0.50), rect.width() * 0.34, line);
            paint.circle_stroke(at(0.50, 0.50), rect.width() * 0.20, line);
            paint.circle_filled(at(0.50, 0.50), rect.width() * 0.055, ink);
        }
        Outline::Edit => {
            trace(&[
                (0.19, 0.79),
                (0.25, 0.53),
                (0.69, 0.13),
                (0.87, 0.32),
                (0.41, 0.74),
                (0.19, 0.79),
            ]);
            trace(&[(0.60, 0.23), (0.77, 0.42)]);
        }
        Outline::Chat => {
            trace(&[
                (0.20, 0.81),
                (0.20, 0.24),
                (0.83, 0.24),
                (0.83, 0.65),
                (0.41, 0.65),
                (0.20, 0.81),
            ]);
            for x in [0.36, 0.51, 0.66] {
                paint.circle_filled(at(x, 0.44), rect.width() * 0.041, ink);
            }
        }
        Outline::Archive => {
            box_stroke(0.15, 0.17, 0.85, 0.38);
            trace(&[(0.22, 0.42), (0.22, 0.80), (0.78, 0.80), (0.78, 0.42)]);
            trace(&[(0.41, 0.53), (0.59, 0.53)]);
        }
        Outline::People => {
            paint.circle_stroke(at(0.48, 0.32), rect.width() * 0.14, line);
            trace(&[
                (0.20, 0.82),
                (0.22, 0.70),
                (0.33, 0.60),
                (0.63, 0.60),
                (0.74, 0.70),
                (0.76, 0.82),
            ]);
        }
        Outline::Host => {
            box_stroke(0.16, 0.17, 0.84, 0.47);
            box_stroke(0.16, 0.53, 0.84, 0.83);
            for y in [0.32, 0.68] {
                paint.circle_filled(at(0.33, y), rect.width() * 0.042, ink);
            }
        }
        Outline::Agent => glyph(paint, rect.center(), rect.width() * 0.38, ink),
        Outline::Lock => {
            box_stroke(0.23, 0.45, 0.77, 0.86);
            trace(&[
                (0.34, 0.45),
                (0.34, 0.28),
                (0.40, 0.18),
                (0.60, 0.18),
                (0.66, 0.28),
                (0.66, 0.45),
            ]);
            trace(&[(0.50, 0.60), (0.50, 0.71)]);
        }
        Outline::Link => {
            trace(&[
                (0.47, 0.29),
                (0.64, 0.15),
                (0.81, 0.18),
                (0.85, 0.35),
                (0.70, 0.53),
            ]);
            trace(&[
                (0.53, 0.71),
                (0.36, 0.85),
                (0.19, 0.82),
                (0.15, 0.65),
                (0.30, 0.47),
            ]);
            trace(&[(0.37, 0.63), (0.63, 0.37)]);
        }
        Outline::Plus => {
            trace(&[(0.20, 0.50), (0.80, 0.50)]);
            trace(&[(0.50, 0.20), (0.50, 0.80)]);
        }
        Outline::Copy => {
            box_stroke(0.34, 0.34, 0.83, 0.85);
            trace(&[
                (0.65, 0.22),
                (0.65, 0.15),
                (0.17, 0.15),
                (0.17, 0.66),
                (0.24, 0.66),
            ]);
        }
        Outline::Search => {
            paint.circle_stroke(at(0.43, 0.43), rect.width() * 0.24, line);
            trace(&[(0.61, 0.61), (0.83, 0.83)]);
        }
        Outline::Check => trace(&[(0.20, 0.52), (0.39, 0.72), (0.81, 0.29)]),
        Outline::Settings => {
            paint.circle_stroke(at(0.50, 0.50), rect.width() * 0.29, line);
            paint.circle_stroke(at(0.50, 0.50), rect.width() * 0.11, line);
            for i in 0..8 {
                let angle = std::f32::consts::TAU * i as f32 / 8.;
                let (y, x) = angle.sin_cos();
                trace(&[
                    (0.5 + x * 0.29, 0.5 + y * 0.29),
                    (0.5 + x * 0.40, 0.5 + y * 0.40),
                ]);
            }
        }
        Outline::Cross => {
            trace(&[(0.25, 0.25), (0.75, 0.75)]);
            trace(&[(0.75, 0.25), (0.25, 0.75)]);
        }
        Outline::Attachment => trace(&[
            (0.40, 0.62),
            (0.68, 0.34),
            (0.68, 0.24),
            (0.60, 0.19),
            (0.51, 0.23),
            (0.23, 0.51),
            (0.18, 0.65),
            (0.22, 0.78),
            (0.35, 0.83),
            (0.48, 0.78),
            (0.80, 0.46),
        ]),
        Outline::Emoji => {
            paint.circle_stroke(at(0.50, 0.50), rect.width() * 0.36, line);
            for x in [0.38, 0.62] {
                paint.circle_filled(at(x, 0.40), rect.width() * 0.04, ink);
            }
            trace(&[(0.34, 0.60), (0.43, 0.69), (0.57, 0.69), (0.66, 0.60)]);
        }
    }
}

pub fn slash_command_suggestion(
    ui: &mut Ui,
    icon: Outline,
    command: &str,
    description: &str,
    selected: bool,
) -> egui::Response {
    let colors = current(ui.ctx());
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, 48.), egui::Sense::click());
    let active = selected || response.hovered() || response.has_focus();
    if active {
        ui.painter().rect_filled(rect, 8., colors.raised);
    }
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            8.,
            Stroke::new(1_f32, colors.accent),
            egui::StrokeKind::Inside,
        );
    }
    let badge = Rect::from_center_size(pos2(rect.left() + 22., rect.center().y), Vec2::splat(28.));
    ui.painter().rect_filled(badge, 8., colors.tint);
    outline(
        ui.painter(),
        icon,
        Rect::from_center_size(badge.center(), Vec2::splat(17.)),
        colors.accent,
    );
    ui.painter().text(
        pos2(rect.left() + 45., rect.top() + 8.),
        egui::Align2::LEFT_TOP,
        command,
        FontId::proportional(13.5),
        INK,
    );
    ui.painter().text(
        pos2(rect.left() + 45., rect.top() + 27.),
        egui::Align2::LEFT_TOP,
        description,
        FontId::proportional(11.5),
        MUTED,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn small_icon(ui: &mut Ui, icon: Outline) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.), egui::Sense::hover());
    outline(ui.painter(), icon, rect, current(ui.ctx()).accent);
}

pub fn rail_button(ui: &mut Ui, icon: Outline, label: &str, active: bool) -> egui::Response {
    let colors = current(ui.ctx());
    let response = ui.add_sized(
        [40., 40.],
        egui::Button::new("").corner_radius(10).fill(if active {
            colors.tint
        } else {
            Color32::TRANSPARENT
        }),
    );
    outline(
        ui.painter(),
        icon,
        Rect::from_center_size(response.rect.center(), Vec2::splat(22.)),
        if active || response.hovered() {
            colors.accent
        } else {
            MUTED
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response.on_hover_text(label)
}

pub fn avatar_option(ui: &mut Ui, index: usize, selected: bool) -> egui::Response {
    let [r, g, b] = crate::protocol::AVATAR_PRESETS[index];
    let color = Color32::from_rgb(r, g, b);
    let response = ui.add_sized(
        [32., 32.],
        egui::Button::new("")
            .corner_radius(16)
            .fill(Color32::from_rgb(27, 29, 43))
            .stroke(if selected {
                Stroke::new(2_f32, color)
            } else {
                Stroke::NONE
            }),
    );
    let center = response.rect.center();
    match index % 3 {
        0 => {
            ui.painter().circle_filled(center, 8., color);
        }
        1 => {
            ui.painter().add(Shape::convex_polygon(
                vec![
                    center + vec2(0., -9.),
                    center + vec2(9., 0.),
                    center + vec2(0., 9.),
                    center + vec2(-9., 0.),
                ],
                color,
                Stroke::NONE,
            ));
        }
        _ => {
            ui.painter()
                .rect_filled(Rect::from_center_size(center, Vec2::splat(14.)), 1., color);
        }
    }
    response.on_hover_text(format!("Avatar {}", index + 1))
}

pub fn composer_popup(ui: &mut Ui, icon: Outline, label: &str, contents: impl FnOnce(&mut Ui)) {
    let button = composer_button(ui, icon, label, false);
    egui::Popup::menu(&button).show(contents);
}

pub fn composer_button(ui: &mut Ui, icon: Outline, label: &str, active: bool) -> egui::Response {
    let colors = current(ui.ctx());
    let button = ui.add_sized(
        [36., 36.],
        egui::Button::new("")
            .frame(false)
            .fill(if active {
                colors.tint
            } else {
                Color32::TRANSPARENT
            })
            .corner_radius(9),
    );
    if button.hovered() {
        ui.painter().rect_filled(button.rect, 9., colors.raised);
    }
    outline(
        ui.painter(),
        icon,
        Rect::from_center_size(button.rect.center(), Vec2::splat(23.)),
        if button.hovered() || active {
            colors.accent
        } else {
            INK
        },
    );
    button.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    button
        .on_hover_text(label)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn label_icon(ui: &mut Ui, icon: Outline, text: RichText) {
    ui.horizontal(|ui| {
        small_icon(ui, icon);
        ui.label(text);
    });
}

pub fn work_tab(ui: &mut Ui, icon: Outline, label: &str, selected: bool) -> egui::Response {
    let colors = current(ui.ctx());
    let ink = if selected { colors.accent } else { MUTED };
    let text = ui
        .painter()
        .layout_no_wrap(label.to_owned(), FontId::proportional(13.), ink);
    let (rect, response) =
        ui.allocate_exact_size(vec2(text.size().x + 44., 35.), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label)
    });
    let paint = ui.painter();
    if selected || response.hovered() {
        paint.rect_filled(rect, 8., if selected { colors.tint } else { colors.raised });
    }
    if response.has_focus() {
        paint.rect_stroke(
            rect,
            8.,
            Stroke::new(1_f32, colors.accent),
            egui::StrokeKind::Inside,
        );
    }
    outline(
        paint,
        icon,
        Rect::from_center_size(rect.left_center() + vec2(19., 0.), Vec2::splat(18.)),
        ink,
    );
    let y = rect.center().y - text.size().y / 2.;
    paint.galley(pos2(rect.left() + 34., y), text, ink);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn page_heading(ui: &mut Ui, icon: Outline, title: &str, detail: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.), egui::Sense::hover());
        outline(ui.painter(), icon, rect, current(ui.ctx()).accent);
        ui.heading(RichText::new(title).size(30.).strong());
    });
    if !detail.is_empty() {
        ui.label(RichText::new(detail).size(14.).color(MUTED));
    }
    ui.add_space(15.);
}

pub fn sheet(ui: &mut Ui, show: impl FnOnce(&mut Ui)) {
    let colors = current(ui.ctx());
    let minimum = (ui.available_width() - 34.).max(0.);
    egui::Frame::new()
        .fill(colors.panel)
        .stroke(Stroke::new(1_f32, colors.border))
        .corner_radius(14)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_min_width(minimum);
            show(ui);
        });
}

pub fn emblem(ui: &mut Ui, icon: Outline) {
    let colors = current(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(188., 134.), egui::Sense::hover());
    let paint = ui.painter_at(rect);
    let center = rect.center();
    for x in 1..10 {
        for y in 1..7 {
            let point = rect.min + vec2(18. * x as f32, 18. * y as f32);
            paint.circle_filled(point, 0.9, colors.border.gamma_multiply(0.68));
        }
    }
    for r in [38., 54., 65.] {
        paint.circle_stroke(center, r, Stroke::new(1_f32, colors.border));
    }
    for point in [center + vec2(-51., -17.), center + vec2(51., 17.)] {
        paint.circle_filled(point, 5., colors.panel);
        paint.circle_stroke(point, 5., Stroke::new(1_f32, colors.accent));
    }
    let outer = Rect::from_center_size(center, Vec2::splat(58.));
    paint.rect_filled(outer, 17., colors.tint);
    paint.rect_stroke(
        outer,
        17.,
        Stroke::new(1_f32, colors.border),
        egui::StrokeKind::Inside,
    );
    outline(
        &paint,
        icon,
        Rect::from_center_size(center, Vec2::splat(33.)),
        colors.accent,
    );
}

pub fn status(ui: &mut Ui, label: &str, active: bool) {
    let colors = current(ui.ctx());
    let ink = if active { colors.accent } else { MUTED };
    egui::Frame::new()
        .fill(if active { colors.tint } else { colors.raised })
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(7.), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 2.8, ink);
                ui.label(RichText::new(label).size(12.).color(ink).strong());
            });
        });
}

pub fn host_map(ui: &mut Ui, hosting: bool, connected: usize) {
    let colors = current(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 100.), egui::Sense::hover());
    let paint = ui.painter_at(rect);
    let center = rect.center();
    let base = Rect::from_min_max(rect.min, rect.max);
    paint.rect_filled(base, 11., colors.base);
    for x in 1..(rect.width() / 15.) as u32 {
        for y in 1..7 {
            paint.circle_filled(
                rect.min + vec2(x as f32 * 15., y as f32 * 15.),
                0.7,
                colors.border,
            );
        }
    }
    let left = pos2(rect.left() + rect.width() * 0.25, center.y - 8.);
    let right = pos2(rect.left() + rect.width() * 0.75, center.y - 8.);
    let edge = if connected > 0 {
        colors.accent
    } else {
        colors.border
    };
    paint.line_segment([left, right], Stroke::new(1.5_f32, edge));
    paint.circle_filled(pos2(center.x, left.y), 3., edge);
    for (point, icon, label, active) in [
        (left, Outline::Host, "Bu cihaz", hosting),
        (right, Outline::People, "Ağ", connected > 0),
    ] {
        let tile = Rect::from_center_size(point, Vec2::splat(36.));
        paint.rect_filled(tile, 9., if active { colors.tint } else { colors.chrome });
        outline(
            &paint,
            icon,
            tile.shrink(7.),
            if active { colors.accent } else { MUTED },
        );
        paint.text(
            point + vec2(0., 25.),
            Align2::CENTER_TOP,
            label,
            FontId::proportional(12.),
            MUTED,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_marks_use_one_or_two_vector_paths() {
        for checks in [0, 1, 2] {
            let ctx = egui::Context::default();
            let output = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    message_stamp(ui, "21:50", "Mesaj durumu", Color32::LIGHT_BLUE, checks);
                });
            });
            let traces = output.shapes.iter().filter(|entry| {
                matches!(&entry.shape, Shape::Path(path) if !path.closed && path.points.len() == 3)
            }).count();
            assert_eq!(traces, usize::from(checks));
            assert!(output.shapes.iter().any(|entry| {
                matches!(&entry.shape, Shape::Text(text) if text.galley.job.text == "21:50")
            }));
        }
    }

    #[test]
    fn orbit_marks_move_between_frames() {
        let ctx = egui::Context::default();
        let frame = |time| {
            let input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    network(ui, vec2(470., 500.), Role::Human, None);
                });
            })
            .shapes
            .into_iter()
            .find_map(|shape| match shape.shape {
                Shape::LineSegment { points, .. } => Some(points),
                _ => None,
            })
            .expect("an orbit mark is drawn")
        };
        assert_ne!(frame(0.), frame(1.));
    }
}
