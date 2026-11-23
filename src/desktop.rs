use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use eframe::egui::{
    self, Align, Color32, Context as UiContext, Frame, Layout, RichText, ScrollArea, Ui,
};
use zeroize::Zeroize;

#[cfg(windows)]
#[path = "desktop/paste.rs"]
mod paste;
#[path = "desktop/preferences.rs"]
mod preferences;
#[path = "desktop/remember.rs"]
mod remember;
#[cfg(windows)]
#[path = "desktop/taskbar.rs"]
mod taskbar;
#[path = "desktop/visual.rs"]
mod visual;

use preferences::{Background, DraftState, Preferences};
use visual::{INK, MUTED, Outline};

use crate::{
    agent::{FilePermit, Settings},
    host::{mention_ranges, mentions},
    identity::{
        AgentProfile, Card, HANDLE_RULE, Identity, NAME_RULE, PASSWORD_MIN, Role, pack,
        password_rule, validate_handle, validate_name, validate_password,
    },
    protocol::{AvatarPhoto, Grant, Message, Receipt, Request, Room, Status, allowed_ip},
    session::{Command, Link, Selection, Session, View},
};

pub fn run(role: Role) -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut data = None;
    let mut agent_key = None;
    while let Some(flag) = args.next() {
        if flag == "--data-dir" {
            data = Some(PathBuf::from(
                args.next().context("--data-dir için bir klasör gerekli")?,
            ));
        } else if flag == "--agent-key" && role == Role::Agent {
            agent_key = Some(PathBuf::from(
                args.next().context("--agent-key için bir dosya gerekli")?,
            ));
        } else {
            anyhow::bail!("Kullanım: [--data-dir KLASÖR] [--agent-key DOSYA (ajan istemcisi)]");
        }
    }
    let data = match data {
        Some(data) => data,
        None => ProjectDirs::from("org", "ChatOid", "ChatOid")
            .context("Yerel uygulama klasörü bulunamadı")?
            .data_local_dir()
            .to_owned(),
    };
    let title = if role == Role::Human {
        "ChatOid"
    } else {
        "ChatOid Ajan"
    };
    let size = if role == Role::Human {
        [1080., 720.]
    } else {
        [580., 670.]
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_icon(visual::app_icon(role))
            .with_min_inner_size(if role == Role::Human {
                [720., 505.]
            } else {
                [440., 460.]
            }),
        ..Default::default()
    };
    eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            visual::theme(&cc.egui_ctx, role);
            visual::install_auth_font(&cc.egui_ctx);
            let mut app = App::new(role, data);
            if let Some(path) = agent_key {
                match Identity::load(&path, Role::Agent, None)
                    .and_then(|identity| app.open_identity(identity))
                {
                    Ok(()) => {}
                    Err(error) => app.notice = format!("Ajan anahtarı açılamadı. {error:#}"),
                }
            } else {
                app.restore_saved();
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Chats,
    Archive,
    Contacts,
    Host,
    Agent,
    Settings,
    Profile,
}

struct App {
    role: Role,
    data: PathBuf,
    identity: Option<Identity>,
    session: Option<Session>,
    view: Option<View>,
    page: Page,
    signing_up: bool,
    handle: String,
    name: String,
    password: String,
    remember_me: bool,
    show_password: bool,
    auth_focus: bool,
    auth_activity: Option<f64>,
    #[cfg(windows)]
    taskbar_icon: Option<taskbar::TaskbarIcon>,
    #[cfg(windows)]
    paste_hook: Option<paste::NativePaste>,
    #[cfg(windows)]
    paste_pressed: bool,
    agent_path: String,
    agent_profile: AgentProfile,
    bind: String,
    interfaces: Vec<(String, std::net::IpAddr)>,
    import: String,
    search: String,
    pair: String,
    group_name: String,
    new_group_open: bool,
    selected: Option<Selection>,
    draft: String,
    preferences: Preferences,
    saved_preferences: Preferences,
    submitted: Option<(Selection, uuid::Uuid, DraftState)>,
    recovered_failures: std::collections::BTreeSet<uuid::Uuid>,
    settings_tab: usize,
    wallpaper_for_room: bool,
    name_suggestion: usize,
    signup_avatar: Option<PathBuf>,
    signup_palette: Option<usize>,
    profile_edit: String,
    contact_target: Option<crate::identity::Signed<crate::protocol::ContactRoute>>,
    pending_removal: Option<Selection>,
    device_preview: Option<crate::protocol::DeviceSummary>,
    attachment: Option<PathBuf>,
    profile_card: Option<Card>,
    message_details: Option<(Selection, i64)>,
    reply: Option<(i64, String, String)>,
    jump: Option<i64>,
    read_sent: Option<(Selection, i64)>,
    invite: bool,
    confirm_close: bool,
    settings: Settings,
    settings_dirty: bool,
    notice: String,
}

impl App {
    fn new(role: Role, data: PathBuf) -> Self {
        Self {
            role,
            data,
            identity: None,
            session: None,
            view: None,
            page: if role == Role::Human {
                Page::Chats
            } else {
                Page::Agent
            },
            signing_up: false,
            handle: String::new(),
            name: String::new(),
            password: String::new(),
            remember_me: false,
            show_password: false,
            auth_focus: true,
            auth_activity: None,
            #[cfg(windows)]
            taskbar_icon: None,
            #[cfg(windows)]
            paste_hook: None,
            #[cfg(windows)]
            paste_pressed: false,
            agent_path: String::new(),
            agent_profile: AgentProfile::default(),
            bind: if role == Role::Human {
                "127.0.0.1:47821"
            } else {
                "127.0.0.1:47822"
            }
            .into(),
            interfaces: Vec::new(),
            import: String::new(),
            search: String::new(),
            pair: String::new(),
            group_name: String::new(),
            new_group_open: false,
            selected: None,
            draft: String::new(),
            preferences: Preferences::default(),
            saved_preferences: Preferences::default(),
            submitted: None,
            recovered_failures: Default::default(),
            settings_tab: 0,
            wallpaper_for_room: false,
            name_suggestion: 0,
            signup_avatar: None,
            signup_palette: None,
            profile_edit: String::new(),
            contact_target: None,
            pending_removal: None,
            device_preview: None,
            attachment: None,
            profile_card: None,
            message_details: None,
            reply: None,
            jump: None,
            read_sent: None,
            invite: false,
            confirm_close: false,
            settings: Settings::default(),
            settings_dirty: false,
            notice: String::new(),
        }
    }

    fn profile_root(&self) -> PathBuf {
        self.data.join(match self.role {
            Role::Human => "human",
            Role::Agent => "agent",
        })
    }

    fn choose_auth_role(&mut self, ctx: &UiContext, role: Role) {
        if self.role == role || self.identity.is_some() || self.session.is_some() {
            return;
        }
        self.password.zeroize();
        #[cfg(windows)]
        let taskbar_icon = self.taskbar_icon.take();
        *self = Self::new(role, self.data.clone());
        #[cfg(windows)]
        {
            self.taskbar_icon = taskbar_icon;
        }
        self.auth_activity = Some(ctx.input(|input| input.time));
        visual::theme(ctx, role);
        ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(
            visual::app_icon(role).into(),
        )));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(
            match role {
                Role::Human => "ChatOid",
                Role::Agent => "ChatOid Ajan",
            }
            .into(),
        ));
        ctx.request_repaint();
    }

    fn identity_path(&self) -> Result<PathBuf> {
        let handle = self.handle.trim().to_ascii_lowercase();
        validate_handle(&handle)?;
        Ok(self.profile_root().join("accounts").join(format!(
            "{handle}.{}",
            if self.role == Role::Agent {
                "key"
            } else {
                "identity"
            }
        )))
    }

    fn sign_in(&mut self) -> Result<()> {
        let imported =
            self.role == Role::Agent && !self.signing_up && !self.agent_path.trim().is_empty();
        let path = if imported {
            PathBuf::from(self.agent_path.trim())
        } else {
            self.identity_path()?
        };
        let password = (self.role == Role::Human).then_some(self.password.as_str());
        let identity = if self.signing_up {
            let identity = if self.role == Role::Agent {
                Identity::generate_agent(&self.handle, &self.name, self.agent_profile.clone())?
            } else {
                Identity::generate(self.role, &self.handle, &self.name)?
            };
            identity.save(&path, password)?;
            identity
        } else {
            Identity::load(&path, self.role, password).map_err(|error| {
                if self.role == Role::Human {
                    anyhow::anyhow!("Kullanıcı adı veya parola yanlış.")
                } else {
                    error
                }
            })?
        };
        self.open_identity(identity)?;
        if self.signing_up && self.role == Role::Agent {
            if let Some(path) = self.signup_avatar.take() {
                self.command(Command::SetAvatar(Some(path)));
            } else if let Some(index) = self.signup_palette {
                self.command(Command::SetBuiltInAvatar(index));
            }
        }
        self.notice.clear();
        if self.role == Role::Human {
            let root = self.profile_root();
            let result = if self.remember_me {
                remember::save(&root, self.identity.as_ref().expect("signed in"))
            } else {
                remember::forget(&root)
            };
            if let Err(error) = result {
                self.notice = format!("Oturum tercihi kaydedilemedi. {error:#}");
            }
        } else if self.signing_up {
            self.notice = format!("Ajan anahtarı oluşturuldu · {}", path.display());
        }
        Ok(())
    }

    fn open_identity(&mut self, identity: Identity) -> Result<()> {
        self.session = Some(Session::new(
            self.profile_root().join("profiles"),
            identity.clone(),
            ProjectDirs::from("org", "ChatOid", "ChatOid")
                .map(|dirs| dirs.data_local_dir().join("local-invitations")),
        )?);
        self.identity = Some(identity);
        self.profile_edit = self
            .identity
            .as_ref()
            .map(|identity| identity.card.data.name.clone())
            .unwrap_or_default();
        if let Some(root) = self.preferences_root() {
            match Preferences::load(&root) {
                Ok(preferences) => {
                    self.saved_preferences = preferences.clone();
                    self.preferences = preferences;
                }
                Err(error) => self.notice = format!("Yerel ayarlar okunamadı. {error:#}"),
            }
        }
        let _ = self.refresh_interfaces();
        Ok(())
    }

    fn restore_saved(&mut self) {
        if self.role != Role::Human {
            return;
        }
        match remember::load(&self.profile_root()) {
            Ok(Some(identity)) => {
                self.handle = identity.card.data.handle.clone();
                self.remember_me = true;
                if let Err(error) = self.open_identity(identity) {
                    self.notice = format!("{error:#}");
                }
            }
            Ok(None) => {}
            Err(error) => self.notice = format!("Hatırlanan oturum açılamadı. {error:#}"),
        }
    }

    fn refresh_interfaces(&mut self) -> Result<()> {
        match if_addrs::get_if_addrs() {
            Ok(interfaces) => {
                self.interfaces = interfaces
                    .into_iter()
                    .filter_map(|interface| {
                        let ip = interface.ip();
                        allowed_ip(ip).then_some((interface.name, ip))
                    })
                    .collect();
                self.interfaces
                    .sort_by_key(|(name, ip)| (ip.is_loopback(), name.clone(), *ip));
                self.interfaces.dedup();
                Ok(())
            }
            Err(error) => Err(error).context("Ağ arayüzleri okunamadı"),
        }
    }

    fn sign_out(&mut self) -> Result<()> {
        if let Some(identity) = &self.identity
            && self.role == Role::Human
        {
            remember::forget_account(&self.profile_root(), &identity.card.data.handle)?;
        }
        let login = Self::new(self.role, self.data.clone());
        #[cfg(windows)]
        let login = Self {
            taskbar_icon: self.taskbar_icon.take(),
            ..login
        };
        *self = login;
        Ok(())
    }

    fn command(&mut self, command: Command) {
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.send.send(command).is_err())
        {
            self.notice = "Oturum çalışmıyor. Tekrar giriş yap.".into();
        }
    }

    fn update_view(&mut self, ctx: &UiContext) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        while let Ok(view) = session.receive.try_recv() {
            if let Some((selection, id, draft)) = &self.submitted {
                let link = view.local.hosts.get(&selection.host);
                let pending =
                    link.and_then(|link| link.queue.iter().find(|item| item.draft.id == *id));
                let sent = link.is_some_and(|link| {
                    link.messages
                        .values()
                        .any(|message| message.client_id == *id)
                });
                if sent || pending.is_some_and(|item| item.error.is_none()) {
                    let key = Self::draft_key(selection);
                    if self.selected.as_ref() == Some(selection) {
                        if self.draft == draft.text
                            && self.reply == draft.reply
                            && self.attachment == draft.attachment
                        {
                            self.draft.clear();
                            self.reply = None;
                            self.attachment = None;
                        }
                    } else if self.preferences.drafts.get(&key) == Some(draft) {
                        self.preferences.drafts.remove(&key);
                    }
                    self.submitted = None;
                } else if pending.is_some() || view.notice.is_some() {
                    self.submitted = None;
                }
            }
            if let Some(message) = &view.notice {
                self.notice = message.clone();
            }
            if let Some(code) = &view.copy {
                ctx.copy_text(code.clone());
            }
            if let Some(host) = &view.host {
                self.bind = host.socket.to_string();
            }
            if !self.settings_dirty {
                self.settings = view.local.settings.clone();
            }
            self.view = Some(view);
        }
    }

    fn login(&mut self, ctx: &UiContext) {
        let colors = visual::palette(self.role);
        let size = ctx.content_rect().size();
        egui::CentralPanel::default()
            .frame(Frame::new().fill(colors.base).inner_margin(22))
            .show(ctx, |ui| {
                let wide = size.x >= 875.;
                let available = ui.available_width();
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if wide {
                            ui.horizontal_top(|ui| {
                                let artwork_width = available * 0.49;
                                if let Some(role) = visual::network(
                                    ui,
                                    egui::vec2(artwork_width, (size.y - 44.).max(420.)),
                                    self.role,
                                    self.auth_activity,
                                ) {
                                    self.choose_auth_role(ctx, role);
                                }
                                ui.allocate_ui_with_layout(
                                    egui::vec2((available - artwork_width - 14.).max(360.), 0.),
                                    Layout::top_down(Align::Center),
                                    |ui| {
                                        let offset = if self.role == Role::Agent && self.signing_up
                                        {
                                            ((size.y - 640.) * 0.25).max(10.)
                                        } else {
                                            ((size.y - 460.) * 0.40).max(18.)
                                        };
                                        ui.add_space(offset);
                                        self.auth_form(ui);
                                    },
                                );
                            });
                        } else {
                            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                                ui.add_space(((size.y - 468.) * 0.30).max(14.));
                                ui.horizontal(|ui| {
                                    for (role, label) in [
                                        (Role::Human, "İnsan hesabı"),
                                        (Role::Agent, "Ajan hesabı"),
                                    ] {
                                        if ui.selectable_label(self.role == role, label).clicked() {
                                            self.choose_auth_role(ctx, role);
                                        }
                                    }
                                });
                                self.auth_form(ui);
                            });
                        }
                    });
            });
    }

    fn auth_form(&mut self, ui: &mut Ui) {
        let colors = visual::palette(self.role);
        let human = self.role == Role::Human;
        let (title, hint, action, switch_text, switch_action) = match (human, self.signing_up) {
            (true, false) => (
                "Giriş yap",
                "ChatOid hesabınla sohbetlerine dön.",
                "Giriş yap",
                "ChatOid’de yeni misin?",
                "Hesap oluştur",
            ),
            (true, true) => (
                "Hesap oluştur",
                "Hesabın bu cihazda saklanır.",
                "Hesap oluştur",
                "Zaten hesabın var mı?",
                "Giriş yap",
            ),
            (false, false) => (
                "Ajan anahtarını aç",
                "Bu cihazdaki bir ajanı aç veya .key dosyasının yolunu gir.",
                "Anahtarı aç",
                "Yeni bir ajan mı kuruyorsun?",
                "Ajan oluştur",
            ),
            (false, true) => (
                "Ajan oluştur",
                "Kimlik ve model bilgileri .key dosyasına kaydedilir.",
                ".key dosyasını oluştur",
                "Ajan anahtarın var mı?",
                "Anahtarı aç",
            ),
        };
        let width = ui.available_width().min(336.);
        ui.allocate_ui_with_layout(egui::vec2(width, 0.), Layout::top_down(Align::Min), |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 6.;
            ui.spacing_mut().interact_size.y = 20.;
            visual::brand(ui, if human { "ChatOid" } else { "ChatOid Ajan" });
            ui.add_space(18.);
            ui.heading(RichText::new(title).size(31.).strong().color(INK));
            ui.label(RichText::new(hint).color(MUTED));
            ui.add_space(14.);
            let mut fields = vec![auth_input(
                ui,
                "Kullanıcı adı",
                "ör. deniz",
                &mut self.handle,
            )];
            if self.auth_focus {
                fields[0].request_focus();
                self.auth_focus = false;
            }
            let valid_handle = self.identity_path().is_ok();
            visual::field_rules(
                ui,
                &fields[0],
                !self.handle.is_empty() && !valid_handle,
                HANDLE_RULE,
            );
            let mut valid_name = true;
            if self.signing_up {
                let name_field = auth_input(ui, "Görünen ad", "ör. Deniz", &mut self.name);
                valid_name = validate_name(self.name.trim()).is_ok();
                visual::field_rules(
                    ui,
                    &name_field,
                    !self.name.is_empty() && !valid_name,
                    NAME_RULE,
                );
                fields.push(name_field);
            }
            let mut valid_password = true;
            if human {
                let password_count = self.password.chars().count();
                ui.horizontal(|ui| {
                    visual::label_icon(ui, Outline::Lock, RichText::new("Parola").strong());
                    if self.signing_up {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let count =
                                format!("{}/{PASSWORD_MIN}", password_count.min(PASSWORD_MIN));
                            ui.label(RichText::new(count).size(12.).color(
                                if password_count >= PASSWORD_MIN {
                                    colors.accent
                                } else {
                                    MUTED
                                },
                            ));
                        });
                    }
                });
                let hint = if self.signing_up {
                    format!("En az {PASSWORD_MIN} karakter")
                } else {
                    "Parolan".into()
                };
                let password_field = ui.add_sized(
                    [width, 41.],
                    visual::auth_text(ui, &mut self.password, &hint)
                        .password(!self.show_password)
                        .margin(egui::Margin {
                            left: 39,
                            right: 39,
                            top: 5,
                            bottom: 5,
                        }),
                );
                let eye_center = password_field.rect.right_center() - egui::vec2(19., 0.);
                let eye_rect = egui::Rect::from_center_size(eye_center, egui::vec2(32., 32.));
                if visual::password_eye(ui, eye_rect, self.show_password).clicked() {
                    self.show_password = !self.show_password;
                    password_field.request_focus();
                }
                valid_password = validate_password(&self.password).is_ok();
                visual::field_rules(
                    ui,
                    &password_field,
                    !self.password.is_empty() && !valid_password,
                    &password_rule(),
                );
                fields.push(password_field);
                ui.horizontal(|ui| {
                    visual::small_icon(ui, Outline::Lock);
                    ui.checkbox(&mut self.remember_me, "Beni hatırla")
                        .on_hover_text(
                            "Bu cihazdaki sonraki açılışta hesabına otomatik giriş yap.",
                        );
                });
            } else if self.signing_up {
                self.agent_profile_fields(ui, &mut fields);
            } else {
                fields.push(auth_input(
                    ui,
                    ".key dosyası (isteğe bağlı)",
                    "Dosyanın tam yolu",
                    &mut self.agent_path,
                ));
            }
            if fields.iter().any(egui::Response::changed) {
                self.notice.clear();
            }
            ui.add_space(6.);
            if !self.notice.is_empty() {
                Frame::new()
                    .fill(colors.raised)
                    .stroke(egui::Stroke::new(1_f32, colors.border))
                    .corner_radius(8.)
                    .inner_margin(9)
                    .show(ui, |ui| {
                        ui.label(&self.notice);
                    });
            }
            let valid_profile = human || !self.signing_up || self.agent_profile.validate().is_ok();
            let ready = valid_profile
                && (valid_handle
                    || (!human && !self.signing_up && !self.agent_path.trim().is_empty()))
                && (!human
                    || if self.signing_up {
                        valid_password
                    } else {
                        !self.password.is_empty()
                    })
                && valid_name;
            let enter_field = ui
                .input(|input| input.key_pressed(egui::Key::Enter))
                .then(|| fields.iter().position(egui::Response::lost_focus))
                .flatten();
            let enter = enter_field.is_some_and(|index| {
                if let Some(next) = fields.get(index + 1) {
                    next.request_focus();
                    false
                } else {
                    true
                }
            });
            if ui
                .add_enabled_ui(ready, |ui| {
                    ui.add_sized([width, 42.], visual::primary(ui, action))
                })
                .inner
                .clicked()
                || (ready && enter)
            {
                self.submit_auth();
                if self.identity.is_none()
                    && human
                    && let Some(password) = fields.last()
                {
                    password.request_focus();
                }
            }
            ui.add_space(4.);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(switch_text).color(MUTED));
                if ui.link(switch_action).clicked() {
                    self.signing_up = !self.signing_up;
                    self.notice.clear();
                    self.show_password = false;
                    self.auth_focus = true;
                }
            });
        });
    }

    fn submit_auth(&mut self) {
        if let Err(error) = self.sign_in() {
            self.notice = format!("{error:#}");
        }
        self.password.zeroize();
        self.show_password = false;
    }

    fn agent_profile_fields(&mut self, ui: &mut Ui, fields: &mut Vec<egui::Response>) {
        const NAMES: [(&str, &str); 32] = [
            ("Poyraz", "poyraz"),
            ("Ada", "ada"),
            ("Aras", "aras"),
            ("Atlas", "atlas"),
            ("Bora", "bora"),
            ("Bulut", "bulut"),
            ("Ceren", "ceren"),
            ("Çağıl", "cagil"),
            ("Defne", "defne"),
            ("Deniz", "deniz"),
            ("Duru", "duru"),
            ("Doruk", "doruk"),
            ("Ekin", "ekin"),
            ("Ege", "ege"),
            ("Ela", "ela"),
            ("Gökçe", "gokce"),
            ("Ilgın", "ilgin"),
            ("Işık", "isik"),
            ("İdil", "idil"),
            ("İrem", "irem"),
            ("Kaya", "kaya"),
            ("Kuzey", "kuzey"),
            ("Lale", "lale"),
            ("Meriç", "meric"),
            ("Nehir", "nehir"),
            ("Okyanus", "okyanus"),
            ("Rüzgâr", "ruzgar"),
            ("Sarp", "sarp"),
            ("Selin", "selin"),
            ("Toprak", "toprak"),
            ("Yağmur", "yagmur"),
            ("Yıldız", "yildiz"),
        ];
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("Yeniden öner")
                .on_hover_text("Hazır ajan adlarından birini seç")
                .clicked()
            {
                let (name, handle) = NAMES[self.name_suggestion % NAMES.len()];
                self.name_suggestion += 1;
                self.name = name.into();
                self.handle = handle.into();
                let accounts = self.profile_root().join("accounts");
                let mut suffix = 2;
                while accounts.join(format!("{}.key", self.handle)).exists() {
                    self.handle = format!("{handle}_{suffix}");
                    suffix += 1;
                }
            }
            ui.menu_button("Hazır avatar", |ui| {
                for row in 0..3 {
                    ui.horizontal(|ui| {
                        for index in row * 4..row * 4 + 4 {
                            if visual::avatar_option(ui, index, self.signup_palette == Some(index))
                                .clicked()
                            {
                                self.signup_palette = Some(index);
                                self.signup_avatar = None;
                                ui.close();
                            }
                        }
                    });
                }
            });
            #[cfg(windows)]
            if ui.button("Fotoğraf seç").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("JPEG / PNG", &["jpg", "jpeg", "png"])
                    .pick_file()
            {
                self.signup_avatar = Some(path);
                self.signup_palette = None;
            }
        });
        fields.push(auth_input(
            ui,
            "Model adı",
            "ör. GPT-6 Astra veya Qwen 2.7 27B",
            &mut self.agent_profile.model,
        ));
        fields.push(auth_input(
            ui,
            "Görevi",
            "ör. Windows kurulumunu doğrulamak",
            &mut self.agent_profile.duty,
        ));
        if ui
            .checkbox(&mut self.agent_profile.is_sub_agent, "Bu bir alt ajan")
            .changed()
        {
            self.agent_profile.parent_agent = self.agent_profile.is_sub_agent.then(String::new);
        }
        if let Some(parent) = self.agent_profile.parent_agent.as_mut() {
            fields.push(auth_input(ui, "Üst ajanın adı", "ör. Poyraz", parent));
        }
    }

    fn navigation(&mut self, ctx: &UiContext) {
        let colors = visual::palette(self.role);
        egui::TopBottomPanel::top("navigation")
            .frame(
                Frame::new()
                    .fill(colors.panel)
                    .inner_margin(egui::Margin::symmetric(13, 5)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    visual::workspace_brand(
                        ui,
                        if self.role == Role::Human {
                            "ChatOid"
                        } else {
                            "ChatOid Ajan"
                        },
                    );
                    ui.separator();
                    let pending = self.view.as_ref().map_or(0, |v| {
                        v.local
                            .hosts
                            .values()
                            .map(|h| {
                                h.requests
                                    .iter()
                                    .filter(|r| {
                                        r.status == Status::Pending && r.from.data.key != self.id()
                                    })
                                    .count()
                            })
                            .sum()
                    });
                    let count = self.view.as_ref().map_or(0, |v| {
                        v.local
                            .hosts
                            .values()
                            .map(|h| {
                                h.archived
                                    .iter()
                                    .filter(|room| h.unread(**room, self.id()) > 0)
                                    .count()
                            })
                            .sum::<usize>()
                    });
                    if self.role == Role::Agent {
                        nav(ui, &mut self.page, Page::Agent, "Ajan");
                        nav(ui, &mut self.page, Page::Contacts, "Kişiler");
                        nav(ui, &mut self.page, Page::Host, "Host");
                        nav(ui, &mut self.page, Page::Settings, "Ayarlar");
                    } else if pending + count > 0 {
                        ui.weak(format!("{pending} istek · {count} okunmamış arşiv"));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let name = self
                            .view
                            .as_ref()
                            .and_then(|view| view.local.display_name.as_deref())
                            .unwrap_or(&self.identity.as_ref().expect("signed in").card.data.name);
                        let display_name = short(name, 21);
                        visual::account_menu(ui, &display_name, |ui| {
                            if ui.button("Profilim").clicked() {
                                self.page = Page::Profile;
                                ui.close();
                            }
                            #[cfg(windows)]
                            if ui.button("Profil fotoğrafı seç").clicked() {
                                ui.close();
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter("JPEG / PNG", &["jpg", "jpeg", "png"])
                                    .pick_file()
                                {
                                    self.command(Command::SetAvatar(Some(path)));
                                }
                            }
                            if self
                                .view
                                .as_ref()
                                .and_then(|view| view.local.avatar.as_ref())
                                .is_some_and(|avatar| avatar.data.is_some())
                                && ui.button("Profil fotoğrafını kaldır").clicked()
                            {
                                self.command(Command::SetAvatar(None));
                                ui.close();
                            }
                            if visual::work_tab(ui, Outline::Lock, "Oturumu kapat", false).clicked()
                            {
                                if let Err(error) = self.sign_out() {
                                    self.notice = format!("Oturum kapatılamadı. {error:#}");
                                }
                                ui.close();
                            }
                            if ui.button("Uygulamadan çık").clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });
                    });
                });
            });
        if self.role == Role::Human {
            egui::SidePanel::left("main-rail")
                .exact_width(55.)
                .resizable(false)
                .frame(Frame::new().fill(colors.panel).inner_margin(5))
                .show(ctx, |ui| {
                    for (page, icon, label) in [
                        (Page::Chats, Outline::Chat, "Sohbetler"),
                        (Page::Archive, Outline::Archive, "Arşivlenenler"),
                        (Page::Contacts, Outline::People, "Kişiler ve istekler"),
                        (Page::Host, Outline::Host, "Host"),
                        (Page::Settings, Outline::Settings, "Ayarlar"),
                        (Page::Profile, Outline::Agent, "Profilim"),
                    ] {
                        if visual::rail_button(ui, icon, label, self.page == page).clicked() {
                            self.page = page;
                        }
                    }
                });
        }
        if !self.notice.is_empty() {
            egui::TopBottomPanel::bottom("status")
                .frame(Frame::new().fill(colors.raised).inner_margin(5))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("×")
                            .on_hover_text("Bildirimi kapat")
                            .clicked()
                        {
                            self.notice.clear();
                        }
                        ui.label(&self.notice);
                    });
                });
        }
    }

    fn id(&self) -> &str {
        self.identity.as_ref().map_or("", Identity::id)
    }

    fn choose(&mut self, selection: Selection) {
        if self.selected.as_ref() != Some(&selection) {
            self.save_preferences();
            let draft = self
                .preferences
                .drafts
                .get(&Self::draft_key(&selection))
                .cloned()
                .unwrap_or_default();
            self.selected = Some(selection);
            self.draft = draft.text;
            self.attachment = draft.attachment;
            self.message_details = None;
            self.reply = draft.reply;
            self.jump = None;
            self.invite = false;
            self.read_sent = None;
            self.confirm_close = false;
        }
    }

    fn preferences_root(&self) -> Option<PathBuf> {
        Some(
            self.profile_root()
                .join("profiles")
                .join(self.identity.as_ref()?.id()),
        )
    }

    fn draft_key(selection: &Selection) -> String {
        format!("{}/{}", selection.host, selection.room)
    }

    fn current_draft(&self) -> DraftState {
        DraftState {
            text: self.draft.clone(),
            reply: self.reply.clone(),
            attachment: self.attachment.clone(),
        }
    }

    fn save_preferences(&mut self) {
        if let Some(selection) = &self.selected {
            let key = Self::draft_key(selection);
            let draft = self.current_draft();
            if draft == DraftState::default() {
                self.preferences.drafts.remove(&key);
            } else if self.preferences.drafts.get(&key) != Some(&draft) {
                self.preferences.drafts.insert(key, draft);
            }
        }
        if self.preferences != self.saved_preferences
            && let Some(root) = self.preferences_root()
        {
            match self.preferences.save(&root) {
                Ok(()) => self.saved_preferences = self.preferences.clone(),
                Err(error) => self.notice = format!("Yerel ayarlar kaydedilemedi. {error:#}"),
            }
        }
    }

    fn stage_attachment(&mut self, path: &std::path::Path, limit: u64) {
        if let Some(root) = self.preferences_root() {
            match preferences::stage_file(&root, path, limit) {
                Ok(path) => self.attachment = Some(path),
                Err(error) => self.notice = format!("Dosya eklenemedi. {error:#}"),
            }
        }
    }

    fn workspace(&mut self, ctx: &UiContext) {
        match self.page {
            Page::Chats | Page::Archive => self.inbox(ctx),
            page => {
                egui::CentralPanel::default()
                    .frame(
                        Frame::new()
                            .inner_margin(22)
                            .fill(visual::palette(self.role).base),
                    )
                    .show(ctx, |ui| {
                        let width = ui.available_width().min(
                            if matches!(page, Page::Host | Page::Profile) {
                                690.
                            } else {
                                910.
                            },
                        );
                        ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.with_layout(Layout::top_down(Align::Center), |ui| {
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(width, 0.),
                                        Layout::top_down(Align::Min),
                                        |ui| {
                                            ui.set_width(width);
                                            ui.add_space(10.);
                                            match page {
                                                Page::Contacts => self.contacts(ui),
                                                Page::Host => self.host(ui),
                                                Page::Agent => self.agent(ui),
                                                Page::Settings => self.workspace_settings(ui),
                                                Page::Profile => self.workspace_profile(ui),
                                                _ => unreachable!(),
                                            }
                                        },
                                    );
                                });
                            });
                    });
            }
        }
    }

    fn workspace_settings(&mut self, ui: &mut Ui) {
        visual::page_heading(
            ui,
            Outline::Settings,
            "Ayarlar",
            "Görünüm ve paylaşım tercihleri",
        );
        ui.horizontal_wrapped(|ui| {
            for (index, label) in ["Görünüm", "Sohbetler", "Bağlantı", "Gizlilik"]
                .iter()
                .enumerate()
            {
                if ui
                    .selectable_label(self.settings_tab == index, *label)
                    .clicked()
                {
                    self.settings_tab = index;
                }
            }
        });
        ui.add_space(12.);
        match self.settings_tab {
            0 | 1 => self.wallpaper_settings(ui),
            2 => self.host(ui),
            _ => {
                if ui
                    .checkbox(
                        &mut self.preferences.local_discovery,
                        "Bu cihazdaki hesaplara öner",
                    )
                    .changed()
                {
                    self.command(Command::SetDiscovery(self.preferences.local_discovery));
                }
                if self.role == Role::Agent {
                    let mut enabled = self
                        .view
                        .as_ref()
                        .and_then(|view| view.local.share_device)
                        .unwrap_or(true);
                    if ui
                        .checkbox(
                            &mut enabled,
                            "Cihaz özetini kabul edilmiş arkadaşlarla paylaş",
                        )
                        .changed()
                    {
                        self.command(Command::SetDeviceSharing(enabled));
                    }
                    if enabled {
                        show_device(
                            ui,
                            self.device_preview
                                .get_or_insert_with(crate::session::device_summary),
                        );
                    }
                }
                if let Some(root) = self.preferences_root() {
                    ui.weak(format!("Veri klasörü · {}", root.display()));
                }
            }
        }
    }

    fn wallpaper_settings(&mut self, ui: &mut Ui) {
        if self.selected.is_some() {
            ui.checkbox(&mut self.wallpaper_for_room, "Yalnızca seçili sohbet");
        }
        let key = self
            .selected
            .as_ref()
            .filter(|_| self.wallpaper_for_room)
            .map(Self::draft_key);
        #[cfg(windows)]
        let root = self.preferences_root();
        let previous = self.preferences.wallpaper.clone();
        let mut wallpaper = key
            .as_ref()
            .and_then(|key| self.preferences.room_wallpapers.get(key))
            .cloned()
            .unwrap_or(previous);
        let before = wallpaper.clone();
        let mut reset = false;
        visual::sheet(ui, |ui| {
            ui.heading("Sohbet arka planı");
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut wallpaper.background, Background::Orbit, "Yörünge");
                ui.selectable_value(&mut wallpaper.background, Background::Plain, "Düz renk");
                #[cfg(windows)]
                if ui.button("Fotoğraf seç").clicked()
                    && let Some(root) = &root
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("JPEG / PNG", &["jpg", "jpeg", "png"])
                        .pick_file()
                {
                    match preferences::wallpaper_image(root, &path) {
                        Ok(path) => {
                            wallpaper.photo = Some(path);
                            wallpaper.background = Background::Photo;
                        }
                        Err(error) => self.notice = format!("Arka plan açılamadı. {error:#}"),
                    }
                }
            });
            if wallpaper.background == Background::Plain {
                ui.color_edit_button_srgb(&mut wallpaper.color);
            }
            ui.add(egui::Slider::new(&mut wallpaper.dim, 0..=85).text("Karartma"));
            reset = ui.button("Varsayılana dön").clicked();
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width().min(570.), 140.),
                egui::Sense::hover(),
            );
            visual::wallpaper(ui, rect, &wallpaper);
        });
        if let Some(key) = key {
            if reset {
                self.preferences.room_wallpapers.remove(&key);
            } else if wallpaper != before {
                self.preferences.room_wallpapers.insert(key, wallpaper);
            }
        } else if reset {
            self.preferences.wallpaper = Default::default();
        } else if wallpaper != before {
            self.preferences.wallpaper = wallpaper;
        }
    }

    fn workspace_profile(&mut self, ui: &mut Ui) {
        let Some(identity) = self.identity.clone() else {
            return;
        };
        visual::page_heading(
            ui,
            Outline::People,
            "Profilim",
            "Hesap görünümü ve kişi kartın",
        );
        self.profile_contents(ui, &identity.card);
        ui.add_space(12.);
        visual::profile_section(ui, Outline::People, "Kimlik", |ui| {
            ui.label(RichText::new("Görünen ad").color(MUTED).size(12.));
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 105.).clamp(90., 410.);
                ui.add_sized(
                    [width, 34.],
                    egui::TextEdit::singleline(&mut self.profile_edit)
                        .hint_text("Görünen adın")
                        .vertical_align(Align::Center)
                        .margin(egui::Margin::symmetric(9, 5)),
                );
                if ui.add(visual::primary(ui, "Adı kaydet")).clicked() {
                    self.command(Command::SetDisplayName(self.profile_edit.clone()));
                }
            });
            ui.horizontal_wrapped(|ui| {
                #[cfg(windows)]
                if visual::work_tab(ui, Outline::People, "Profil fotoğrafı seç", false).clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("JPEG / PNG", &["jpg", "jpeg", "png"])
                        .pick_file()
                {
                    self.command(Command::SetAvatar(Some(path)));
                }
                if self
                    .view
                    .as_ref()
                    .and_then(|view| view.local.avatar.as_ref())
                    .is_some_and(|avatar| avatar.data.is_some())
                    && visual::work_tab(ui, Outline::Cross, "Fotoğrafı kaldır", false).clicked()
                {
                    self.command(Command::SetAvatar(None));
                }
            });
        });
        ui.add_space(9.);
        visual::profile_section(ui, Outline::Link, "Paylaşım", |ui| {
            ui.horizontal_wrapped(|ui| {
                if visual::work_tab(ui, Outline::Link, "Bağlantımı paylaş", true).clicked() {
                    self.command(Command::ShareContact);
                }
                if visual::work_tab(ui, Outline::Copy, "Kişi kartını kopyala", false).clicked()
                    && let Ok(code) = pack("card", &identity.card)
                {
                    ui.ctx().copy_text(code);
                }
            });
        });
    }

    fn open_profile(&mut self, card: Card) {
        self.command(Command::LookupProfile(card.data.key.clone()));
        self.profile_card = Some(card);
    }

    fn profile_contents(&self, ui: &mut Ui, card: &Card) {
        let own = card.data.key == self.id();
        let local = self.view.as_ref().map(|view| &view.local);
        let remote = local.and_then(|local| local.profiles.get(&card.data.key));
        let name = if own {
            local.and_then(|local| local.display_name.as_deref())
        } else {
            remote.map(|record| record.profile.data.display_name.as_str())
        }
        .unwrap_or(&card.data.name);
        let photo = if own {
            local
                .and_then(|local| local.avatar.as_ref()?.data.as_ref())
                .map(|data| AvatarPhoto {
                    digest: AvatarPhoto::digest(data),
                    data: data.clone(),
                })
        } else {
            None
        };
        let photo = photo.as_ref().or_else(|| {
            local?
                .hosts
                .values()
                .find_map(|link| link.avatars.get(&card.data.key))
        });
        let agent = card.data.role == Role::Agent;
        let narrow = ui.available_width() < 340.;
        visual::profile_hero(ui, |ui| {
            ui.horizontal(|ui| {
                visual::profile_avatar(ui, card, photo, if narrow { 45. } else { 62. });
                ui.vertical(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(name)
                                .size(if narrow { 19. } else { 25. })
                                .strong(),
                        )
                        .truncate(),
                    )
                    .on_hover_text(name);
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("@{}", card.data.handle))
                                .size(12.)
                                .color(MUTED),
                        )
                        .truncate(),
                    );
                    visual::label_icon(
                        ui,
                        if agent {
                            Outline::Agent
                        } else {
                            Outline::People
                        },
                        RichText::new(if agent { "Ajan" } else { "İnsan" })
                            .size(12.)
                            .color(visual::palette(self.role).accent),
                    );
                });
            });
        });
        if agent {
            let profile = if own {
                self.identity.as_ref().and_then(Identity::agent_profile)
            } else {
                remote.and_then(|record| record.profile.data.agent.as_ref())
            };
            ui.add_space(8.);
            visual::profile_section(ui, Outline::Agent, "Ajan kimliği", |ui| {
                profile_value(
                    ui,
                    "Kayıt modeli",
                    profile.map_or("Belirtilmedi", |p| &p.model),
                );
                profile_value(ui, "Görev", profile.map_or("Belirtilmedi", |p| &p.duty));
                profile_value(
                    ui,
                    "Alt ajan",
                    profile.map_or("Belirtilmedi", |p| {
                        if p.is_sub_agent { "Evet" } else { "Hayır" }
                    }),
                );
                if let Some(parent) = profile.and_then(|p| p.parent_agent.as_deref()) {
                    profile_value(ui, "Üst ajan", parent);
                }
            });
            if local.is_some_and(|local| local.friend(self.id(), &card.data.key).is_some())
                && let Some(device) = remote.and_then(|r| r.device.as_ref())
            {
                ui.add_space(8.);
                visual::profile_section(ui, Outline::Host, "Cihaz özeti", |ui| {
                    profile_value(ui, "Sistem", &device.data.system);
                    profile_value(ui, "Mimari", &device.data.architecture);
                    profile_value(ui, "İşlemci", &device.data.processor);
                    profile_value(ui, "Bellek", &device.data.memory);
                });
            }
        }
        if !own {
            ui.add_space(7.);
            if visual::work_tab(ui, Outline::Copy, "Kişi kartını kopyala", false).clicked()
                && let Ok(code) = pack("card", card)
            {
                ui.ctx().copy_text(code);
            }
        }
    }

    fn inbox(&mut self, ctx: &UiContext) {
        let colors = visual::palette(self.role);
        let archive = self.page == Page::Archive;
        let rows = self
            .view
            .as_ref()
            .map(|view| {
                let mut rows: Vec<_> =
                    view.local
                        .hosts
                        .iter()
                        .flat_map(|(pin, host)| {
                            host.rooms
                                .iter()
                                .filter(|room| host.archived.contains(&room.id) == archive)
                                .map(|room| {
                                    let unread = host.unread(room.id, self.id());
                                    let seen = *host.seen.get(&room.id).unwrap_or(&0);
                                    let targeted =
                                        host.messages.range((seen + 1)..).any(|(_, message)| {
                                            message.room == room.id
                                                && message.author.data.key != self.id()
                                                && (message.reply.as_ref().is_some_and(|q| {
                                                    q.author.data.key == self.id()
                                                }) || self.identity.as_ref().is_some_and(|i| {
                                                    mentions(&message.text, &i.card.data.handle)
                                                }))
                                        });
                                    (
                                        Selection {
                                            host: pin.clone(),
                                            room: room.id,
                                        },
                                        room.name.clone(),
                                        unread,
                                        targeted,
                                        host.last(room.id).map(|m| {
                                            let body = m
                                                .attachment
                                                .as_ref()
                                                .filter(|_| m.text.is_empty())
                                                .map_or_else(
                                                    || m.text.clone(),
                                                    |file| format!("Dosya · {}", file.name),
                                                );
                                            let preview = if room.group {
                                                format!("{} · {body}", m.author.data.name)
                                            } else {
                                                body
                                            };
                                            (m.sent_at, preview)
                                        }),
                                    )
                                })
                        })
                        .collect();
                rows.sort_by_key(|(_, _, _, _, message)| {
                    std::cmp::Reverse(message.as_ref().map(|m| m.0).unwrap_or(0))
                });
                rows
            })
            .unwrap_or_default();
        if !rows
            .iter()
            .any(|(s, _, _, _, _)| self.selected.as_ref() == Some(s))
        {
            self.selected = None;
            if let Some((s, _, _, _, _)) = rows.first() {
                self.choose(s.clone());
            }
        }
        egui::SidePanel::left("inbox")
            .resizable(true)
            .default_width(267.)
            .width_range(221. ..=350.)
            .frame(Frame::new().fill(colors.panel).inner_margin(10))
            .show(ctx, |ui| {
                ui.add_space(6.);
                ui.horizontal(|ui| {
                    visual::label_icon(
                        ui,
                        if archive {
                            Outline::Archive
                        } else {
                            Outline::Chat
                        },
                        RichText::new(if archive {
                            "Arşivlenenler"
                        } else {
                            "Sohbetler"
                        })
                        .size(19.)
                        .strong(),
                    );
                    if !archive {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let toggle = if self.new_group_open {
                                ui.small_button("×").on_hover_text("Grup formunu kapat")
                            } else {
                                visual::work_tab(ui, Outline::Plus, "Yeni grup", false)
                            };
                            if toggle.clicked() {
                                self.new_group_open = !self.new_group_open;
                            }
                        });
                    }
                });
                ui.label(
                    RichText::new(if archive {
                        if rows.is_empty() {
                            "Arşivlenmiş sohbet yok".to_owned()
                        } else {
                            format!("{} arşivlenmiş sohbet", rows.len())
                        }
                    } else if rows.is_empty() {
                        "Henüz bir sohbet yok".to_owned()
                    } else {
                        format!("{} sohbet", rows.len())
                    })
                    .size(13.)
                    .color(MUTED),
                );
                if !archive && self.new_group_open {
                    visual::sheet(ui, |ui| {
                        field(ui, "Grup adı", &mut self.group_name, "ör. Proje ekibi");
                        if self.view.as_ref().is_some_and(|view| view.host.is_some()) {
                            if ui
                                .add_enabled(
                                    !self.group_name.trim().is_empty(),
                                    visual::primary(ui, "Grubu oluştur"),
                                )
                                .clicked()
                            {
                                let name = std::mem::take(&mut self.group_name);
                                self.command(Command::Group(name));
                                self.new_group_open = false;
                            }
                        } else {
                            ui.weak("Bu grubu barındırmak için bu cihazda hostu aç.");
                            if visual::work_tab(ui, Outline::Host, "Host ayarları", false).clicked()
                            {
                                self.page = Page::Host;
                            }
                        }
                    });
                }
                ui.add_space(8.);
                ui.separator();
                ScrollArea::vertical().show(ui, |ui| {
                    for (selection, name, unread, targeted, last) in rows {
                        let (time, preview) = last
                            .map(|(t, p)| (time(t), short(&p, 36)))
                            .unwrap_or_default();
                        let row = visual::chat_row(
                            ui,
                            &name,
                            &preview,
                            &time,
                            unread,
                            targeted,
                            self.selected.as_ref() == Some(&selection),
                        );
                        row.context_menu(|ui| {
                            if ui.button("Sohbeti aç").clicked() {
                                self.choose(selection.clone());
                                ui.close();
                            }
                            if ui
                                .button(if archive {
                                    "Arşivden çıkar"
                                } else {
                                    "Arşivle"
                                })
                                .clicked()
                            {
                                self.command(Command::Archive(selection.clone(), !archive));
                                ui.close();
                            }
                        });
                        if row.clicked() {
                            self.choose(selection);
                        }
                    }
                });
            });
        egui::CentralPanel::default().frame(Frame::new().fill(colors.base).inner_margin(egui::Margin::symmetric(12, 8))).show(ctx, |ui| {
            let selected = self.selected.clone();
            if let Some((selection, link, room)) = selected.and_then(|s| {
                let link = self.view.as_ref()?.local.hosts.get(&s.host)?;
                Some((
                    s.clone(),
                    link.clone(),
                    link.rooms.iter().find(|r| r.id == s.room)?.clone(),
                ))
            }) {
                self.conversation(ui, selection, link, room);
            } else {
                let (title, detail) = if self.view.is_none() {
                    ("Sohbetler açılıyor", "Yerel kayıtlar yükleniyor.")
                } else if archive {
                    ("Arşiv boş", "Arşivlediğin konuşmalar burada görünür.")
                } else {
                    (
                        "İlk sohbetini başlat",
                        "Bir kişinin veya ajanın kartını içe aktar ve arkadaşlık isteği gönder.",
                    )
                };
                ui.with_layout(Layout::top_down(Align::Center), |ui| {
                    ui.add_space(((ui.available_height() - 262.) * 0.45).max(10.));
                    visual::emblem(ui, if archive { Outline::Archive } else { Outline::Chat });
                    ui.heading(RichText::new(title).size(26.).strong());
                    ui.label(RichText::new(detail).size(14.).color(MUTED));
                    if self.view.is_some() {
                        ui.add_space(7.);
                        if archive {
                            if ui.button("Sohbetlere dön").clicked() { self.page = Page::Chats; }
                        } else if ui.add(visual::primary(ui, "Kişi ekle")).clicked() {
                            self.page = Page::Contacts;
                        }
                    }
                });
            }
        });
    }

    fn conversation(&mut self, ui: &mut Ui, s: Selection, link: Link, room: Room) {
        let messages: Vec<_> = link
            .messages
            .values()
            .filter(|message| message.room == room.id)
            .collect();
        let owner = room.owner == self.id();
        ui.horizontal_wrapped(|ui| {
            if !room.group
                && let Some(peer) = room
                    .members
                    .iter()
                    .find(|member| member.data.key != self.id())
            {
                if visual::participant_avatar(ui, peer, link.avatars.get(&peer.data.key)).clicked()
                {
                    self.open_profile(peer.clone());
                }
            } else {
                visual::avatar(ui, &room.name, 32.);
            }
            ui.vertical(|ui| {
                ui.heading(RichText::new(&room.name).size(17.).strong());
                if room.group {
                    let host_name = room
                        .members
                        .iter()
                        .find(|member| member.data.key == room.owner)
                        .or_else(|| self.view.as_ref()?.local.contacts.get(&room.owner))
                        .map_or("Grup kurucusu", |member| member.data.name.as_str());
                    visual::label_icon(
                        ui,
                        Outline::Host,
                        RichText::new(format!("Host · {host_name}")).color(MUTED),
                    );
                } else {
                    ui.weak("Bire bir sohbet");
                }
            });
            if !link.online {
                ui.label(RichText::new("Host çevrimdışı").color(Color32::LIGHT_YELLOW))
                    .on_hover_text(&link.error);
            }
            if room.closed {
                ui.weak("Grup kapalı");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button("Sohbet menüsü", |ui| {
                    if !room.group && !room.closed && ui.button("Arkadaşlıktan çıkar").clicked()
                    {
                        self.pending_removal = Some(s.clone());
                        ui.close();
                    }
                    let archived = link.archived.contains(&room.id);
                    if visual::work_tab(
                        ui,
                        Outline::Archive,
                        if archived {
                            "Arşivden çıkar"
                        } else {
                            "Arşivle"
                        },
                        false,
                    )
                    .clicked()
                    {
                        self.command(Command::Archive(s.clone(), !archived));
                        ui.close();
                    }
                    if let Some(first) = messages.first()
                        && ui.button("Önceki mesajlar").clicked()
                    {
                        self.command(Command::History(s.clone(), first.seq));
                        ui.close();
                    }
                });
                if room.group && room.active && !room.closed {
                    if owner && ui.button("Davet et").clicked() {
                        self.invite = !self.invite;
                    }
                    if !owner && ui.button("Gruptan ayrıl").clicked() {
                        self.command(Command::Leave(s.clone()));
                    }
                }
            });
        });
        if room.group {
            ui.collapsing(format!("{} üye", room.members.len()), |ui| {
                for member in &room.members {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}  @{}", member.data.name, member.data.handle));
                        if owner
                            && !room.closed
                            && member.data.key != self.id()
                            && ui.small_button("Çıkar").clicked()
                        {
                            self.command(Command::Remove(s.clone(), member.data.key.clone()));
                        }
                    });
                }
                if owner && !room.closed {
                    if self.confirm_close {
                        if ui.button("Grubu kapatmayı onayla").clicked() {
                            self.command(Command::Close(s.clone()));
                            self.confirm_close = false;
                        }
                        if ui.button("Vazgeç").clicked() {
                            self.confirm_close = false;
                        }
                    } else if ui.button("Grubu kapat").clicked() {
                        self.confirm_close = true;
                    }
                }
            });
        }
        if self.invite {
            let people = self
                .view
                .as_ref()
                .map(|v| {
                    v.local
                        .contacts
                        .values()
                        .filter(|card| {
                            v.local.friend(self.id(), &card.data.key).is_some()
                                && !room.members.iter().any(|m| m.data.key == card.data.key)
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for card in people {
                if ui
                    .button(format!("{} · Davet gönder", card.data.name))
                    .clicked()
                {
                    self.command(Command::Invite(s.clone(), card));
                    self.invite = false;
                }
            }
        }
        ui.separator();
        let through = messages.last().map_or(0, |m| m.seq);
        if through > *link.seen.get(&room.id).unwrap_or(&0)
            && ui.ctx().input(|input| input.focused)
            && self.read_sent.as_ref() != Some(&(s.clone(), through))
        {
            self.read_sent = Some((s.clone(), through));
            self.command(Command::Read(s.clone(), through));
        }
        let removal_pending = self
            .view
            .as_ref()
            .is_some_and(|view| view.local.removals.contains(&s));
        if room.active && !room.closed && !removal_pending {
            if self.current_draft() == DraftState::default()
                && let Some(item) = link.queue.iter().find(|item| {
                    item.draft.room == room.id
                        && item.error.is_some()
                        && !self.recovered_failures.contains(&item.draft.id)
                })
            {
                self.recovered_failures.insert(item.draft.id);
                self.restore_failed(&link, item);
            }
            let limit = link.max_file_bytes.max(if link.attachments {
                crate::protocol::MAX_ATTACHMENT
            } else {
                0
            });
            self.chat_composer(ui, &s, &room, limit);
        } else if !room.group {
            ui.weak(if removal_pending {
                "Kaldırma bekliyor"
            } else {
                "Arkadaşlık sona erdi · Geçmiş okunabilir"
            });
        }
        let wallpaper = self
            .preferences
            .room_wallpapers
            .get(&Self::draft_key(&s))
            .unwrap_or(&self.preferences.wallpaper);
        visual::wallpaper(ui, ui.available_rect_before_wrap(), wallpaper);
        let bottom = self.jump.is_none();
        ScrollArea::vertical()
            .id_salt((&s.host, s.room))
            .max_height(ui.available_height())
            .auto_shrink([false, false])
            .stick_to_bottom(bottom)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.;
                let mut day = None;
                let mut previous = None;
                for message in messages {
                    let message_day = local_time(message.sent_at).date();
                    let new_day = day != Some(message_day);
                    if new_day {
                        ui.with_layout(Layout::top_down(Align::Center), |ui| {
                            ui.label(
                                RichText::new(day_label(message.sent_at))
                                    .size(12.)
                                    .color(MUTED),
                            );
                        });
                        day = Some(message_day);
                    }
                    let first = new_day
                        || previous.is_none_or(|prior: &Message| {
                            prior.author.data.key != message.author.data.key
                                || message.sent_at.saturating_sub(prior.sent_at) > 300
                        });
                    let grouped = room.group && message.author.data.key != self.id();
                    ui.push_id(message.seq, |ui| {
                        if grouped {
                            ui.horizontal_top(|ui| {
                                if first {
                                    if visual::participant_avatar(
                                        ui,
                                        &message.author,
                                        link.avatars.get(&message.author.data.key),
                                    )
                                    .clicked()
                                    {
                                        self.open_profile(message.author.clone());
                                    }
                                } else {
                                    ui.add_space(28.);
                                }
                                self.message(ui, message, &s, &room, first, &link.receipts);
                            });
                        } else {
                            self.message(ui, message, &s, &room, false, &link.receipts);
                        }
                    });
                    previous = Some(message);
                }
                for item in link.queue.iter().filter(|p| p.draft.room == room.id) {
                    let stamp = if item.error.is_some() {
                        "Başarısız"
                    } else {
                        "Bekliyor"
                    };
                    let size = visual::measure_bubble(
                        ui,
                        &item.draft.text,
                        stamp,
                        if item.error.is_some() { 94. } else { 0. },
                        0,
                    );
                    visual::bubble_lane(ui, true, &size, false, |ui| {
                        if let Some(path) = &item.binary {
                            let name = path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("Dosya");
                            visual::label_icon(
                                ui,
                                Outline::Attachment,
                                RichText::new(short(name, 30)),
                            );
                            if let Some((offset, total)) = self
                                .session
                                .as_ref()
                                .and_then(|s| s.transfer_progress(item.draft.id))
                            {
                                ui.add(
                                    egui::ProgressBar::new(offset as f32 / total as f32)
                                        .show_percentage(),
                                );
                            }
                            if ui.small_button("İptal").clicked()
                                && let Some(session) = &self.session
                            {
                                session.cancel_transfer(s.clone(), item.draft.id);
                            }
                        }
                        if let Some(file) = &item.draft.attachment {
                            visual::label_icon(
                                ui,
                                Outline::Attachment,
                                RichText::new(short(&file.name, 30)),
                            );
                        }
                        visual::bubble_text(ui, &size, &item.draft.text, stamp, stamp, MUTED, 0);
                        if let Some(error) = &item.error
                            && ui.link("Yeniden dene").on_hover_text(error).clicked()
                        {
                            self.command(Command::Retry(s.clone(), item.draft.id));
                        }
                        if item.error.is_some() && ui.small_button("Taslağa al").clicked() {
                            self.restore_failed(&link, item);
                        }
                    });
                }
            });
    }

    fn restore_failed(&mut self, link: &Link, item: &crate::session::Pending) {
        self.draft = item.draft.text.clone();
        self.attachment = item.binary.clone();
        self.reply = item.draft.reply.and_then(|id| {
            let source = link.messages.get(&id)?;
            Some((
                id,
                source.author.data.name.clone(),
                short(&source.text, 100),
            ))
        });
    }

    fn chat_composer(&mut self, ui: &mut Ui, selection: &Selection, room: &Room, limit: usize) {
        let colors = visual::palette(self.role);
        let panel = egui::Id::new(("conversation-composer", selection.room));
        egui::TopBottomPanel::bottom(panel)
            .frame(
                Frame::new()
                    .fill(colors.panel)
                    .inner_margin(egui::Margin::symmetric(9, 7))
                    .corner_radius(12),
            )
            .show_separator_line(false)
            .show_inside(ui, |ui| {
                if let Some((sending, id, _)) = &self.submitted
                    && sending == selection
                    && let Some(session) = &self.session
                    && let Some((offset, total)) = session.transfer_progress(*id)
                {
                    ui.horizontal(|ui| {
                        ui.add(egui::ProgressBar::new(offset as f32/total as f32).show_percentage().desired_width(190.));
                        if ui.small_button("İptal").clicked() { session.cancel_transfer(selection.clone(), *id); }
                    });
                }
                if let Some(path) = &self.attachment {
                    let name = short(
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("Dosya"),
                        48,
                    );
                    ui.horizontal(|ui| {
                        if let Some(path) = &self.attachment {
                            visual::draft_image(ui, path);
                        }
                        visual::label_icon(ui, Outline::Attachment, RichText::new(name));
                        if ui
                            .small_button("×")
                            .on_hover_text("Dosyayı kaldır")
                            .clicked()
                        {
                            self.attachment = None;
                        }
                    });
                }
                if let Some((_, name, text)) = self.reply.clone() {
                    Frame::new()
                        .fill(colors.raised)
                        .corner_radius(7)
                        .inner_margin(6)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let close = ui.small_button("×").on_hover_text("Alıntıyı kaldır");
                                if close.clicked() {
                                    self.reply = None;
                                }
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(format!("{name} · {text}"))
                                            .color(colors.accent),
                                    )
                                    .truncate(),
                                );
                            });
                        });
                }
                let draft_id = panel.with("input");
                #[cfg(windows)]
                if limit > 0 {
                    let focused = ui.memory(|memory| memory.has_focus(draft_id));
                    let paste = focused && ui.input(|input| (self.paste_pressed || (input.key_pressed(egui::Key::V) && input.modifiers.command))
                        && !input.events.iter().any(|event| matches!(event, egui::Event::Paste(text) if !text.is_empty())));
                    if paste && let Some(root) = self.preferences_root() {
                        match preferences::clipboard_image(&root, limit as u64) {
                            Ok(Some(path)) => self.attachment = Some(path),
                            Ok(None) => {}
                            Err(error) => self.notice = format!("Pano resmi eklenemedi. {error:#}"),
                        }
                    }
                    if let Some(path) = ui.input(|input| input.raw.dropped_files.iter().find_map(|file| file.path.clone())) {
                        self.stage_attachment(&path, limit as u64);
                    }
                }
                if room.group
                    && let Some((offset, query)) = mention_query(&self.draft)
                    && ui.memory(|memory| memory.has_focus(draft_id))
                {
                    let query = query.to_ascii_lowercase();
                    ui.horizontal_wrapped(|ui| {
                        for member in room
                            .members
                            .iter()
                            .filter(|member| {
                                member.data.key != self.id()
                                    && member.data.handle.starts_with(&query)
                            })
                            .take(6)
                            .cloned()
                            .collect::<Vec<_>>()
                        {
                            if ui
                                .small_button(format!(
                                    "{} · @{}",
                                    member.data.name, member.data.handle
                                ))
                                .clicked()
                            {
                                self.draft.truncate(offset);
                                self.draft.push_str(&format!("@{} ", member.data.handle));
                                ui.memory_mut(|memory| memory.request_focus(draft_id));
                            }
                        }
                    });
                }
                let enter = ui.memory(|memory| memory.has_focus(draft_id))
                    && ui.input_mut(|input| {
                        !input.modifiers.shift
                            && input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });
                let (field, button) = ui
                    .horizontal(|ui| {
                        visual::composer_popup(ui, Outline::Attachment, "Dosya ekle", |ui| {
                            #[cfg(windows)]
                            if ui
                                .add_enabled(limit > 0, egui::Button::new("Cihazdan dosya seç"))
                                .clicked()
                            {
                                ui.close();
                                if let Some(path) = rfd::FileDialog::new().pick_file() {
                                    self.stage_attachment(&path, limit as u64);
                                }
                            }
                            if limit == 0 {
                                ui.weak("Bu host dosya eklerini desteklemiyor.");
                            } else {
                                #[cfg(windows)]
                                ui.weak(format!("Host sınırı · {} MiB", limit / 1048576));
                                #[cfg(not(windows))]
                                ui.weak("Dosya seçici bu sürümde Windows’ta kullanılabilir.");
                            }
                        });
                        visual::composer_popup(ui, Outline::Emoji, "Emoji ekle", |ui| {
                            ui.horizontal(|ui| {
                                for symbol in ["😀", "👍", "❤️", "🎉", "🙂", "👋"] {
                                    if ui.button(symbol).clicked() {
                                        self.draft.push_str(symbol);
                                        ui.close();
                                    }
                                }
                            });
                        });
                        let width = (ui.available_width() - 49.).max(74.);
                        let field = ui
                            .allocate_ui_with_layout(
                                egui::vec2(width, 0.),
                                Layout::top_down(Align::Min),
                                |ui| {
                                    ui.set_width(width);
                                    ScrollArea::vertical()
                                        .auto_shrink([false, true])
                                        .max_height(92.)
                                        .show(ui, |ui| {
                                            ui.add_sized(
                                                [width, 36.],
                                                egui::TextEdit::multiline(&mut self.draft)
                                                    .id(draft_id)
                                                    .hint_text(
                                                        RichText::new("Mesaj yaz…").color(MUTED),
                                                    )
                                                    .desired_rows(1)
                                                    .margin(egui::Margin::symmetric(10, 8)),
                                            )
                                        })
                                        .inner
                                },
                            )
                            .inner;
                        let send = visual::chat_icon_button(
                            ui,
                            true,
                            !self.draft.trim().is_empty() || self.attachment.is_some(),
                        )
                        .clicked();
                        (field, send)
                    })
                    .inner;
                if (enter || button) && (!self.draft.trim().is_empty() || self.attachment.is_some())
                {
                    let draft = self.current_draft();
                    if self.submitted.as_ref().is_none_or(|(_, _, previous)| previous != &draft) {
                        let id = uuid::Uuid::new_v4();
                        self.command(Command::SendDraft(selection.clone(), id, draft.text.clone(),
                            draft.reply.as_ref().map(|quote| quote.0), draft.attachment.clone()));
                        self.submitted = Some((selection.clone(), id, draft));
                        field.request_focus();
                    }
                }
            });
    }

    fn message(
        &mut self,
        ui: &mut Ui,
        message: &Message,
        selection: &Selection,
        room: &Room,
        show_author: bool,
        receipts: &[Receipt],
    ) {
        let colors = visual::palette(self.role);
        let mine = message.author.data.key == self.id();
        let author = show_author.then_some(message.author.data.name.as_str());
        let stamp = time(message.sent_at);
        let status = mine.then(|| delivery_status(message, receipts));
        let checks = status.map_or(0, DeliveryStatus::checks);
        let title = status.map_or_else(
            || calendar_date(message.sent_at),
            |status| format!("{} · {}", calendar_date(message.sent_at), status.label()),
        );
        let color = if status == Some(DeliveryStatus::Read) {
            Color32::from_rgb(124, 214, 242)
        } else {
            MUTED
        };
        let quote = message
            .reply
            .as_ref()
            .map(|quote| format!("{} · {}", quote.author.data.name, short(&quote.text, 88)));
        let extra = [
            author.map_or(0., |author| {
                ui.painter()
                    .layout_no_wrap(
                        author.to_owned(),
                        egui::FontId::proportional(12.),
                        colors.accent,
                    )
                    .size()
                    .x
            }),
            quote.as_ref().map_or(0., |quote| {
                ui.painter()
                    .layout_no_wrap(
                        quote.clone(),
                        egui::FontId::proportional(12.),
                        colors.accent,
                    )
                    .size()
                    .x
                    + 15.
            }),
            if let Some(file) = &message.attachment {
                file.preview
                    .as_ref()
                    .map_or(175., |preview| (preview.width as f32).max(175.))
            } else if message.invitation.is_some() && !mine {
                75.
            } else {
                0.
            },
        ]
        .into_iter()
        .fold(0_f32, f32::max);
        let size = visual::measure_bubble(ui, &message.text, &stamp, extra, checks);
        let (bubble, replied) =
            visual::bubble_lane(ui, mine, &size, message.invitation.is_none(), |ui| {
                if let Some(author) = author {
                    ui.add(
                        egui::Label::new(
                            RichText::new(author)
                                .size(12.)
                                .strong()
                                .color(visual::participant_color(&message.author.data.key)),
                        )
                        .truncate(),
                    );
                }
                if let (Some(source), Some(quote)) = (&message.reply, &quote) {
                    let button =
                        egui::Button::new(RichText::new(quote).size(12.).color(colors.accent))
                            .fill(colors.base)
                            .corner_radius(6.)
                            .wrap_mode(egui::TextWrapMode::Truncate);
                    if ui.add_sized([ui.available_width(), 25.], button).clicked() {
                        self.jump = Some(source.id);
                        let cached = self.view.as_ref().is_some_and(|v| {
                            v.local
                                .hosts
                                .get(&selection.host)
                                .is_some_and(|host| host.messages.contains_key(&source.id))
                        });
                        if !cached {
                            self.command(Command::History(selection.clone(), source.id + 1));
                        }
                    }
                }
                if let Some(file) = &message.attachment {
                    if let Some(preview) = &file.preview {
                        visual::attachment_preview(ui, preview, (&selection.host, message.seq));
                    }
                    let label = format!("{} · {}", short(&file.name, 26), file_size(file.bytes));
                    #[cfg(windows)]
                    if ui
                        .horizontal(|ui| {
                            visual::small_icon(ui, Outline::Attachment);
                            ui.add_sized(
                                [ui.available_width(), 27.],
                                egui::Button::new(&label).truncate(),
                            )
                        })
                        .inner
                        .on_hover_text(format!("{} dosyasını kaydet", file.name))
                        .clicked()
                        && let Some(path) =
                            rfd::FileDialog::new().set_file_name(&file.name).save_file()
                    {
                        self.command(Command::Download(selection.clone(), message.seq, path));
                    }
                    #[cfg(not(windows))]
                    visual::label_icon(ui, Outline::Attachment, RichText::new(label));
                }
                let (job, tags) = mention_body(&message.text, room, colors.accent);
                if tags.is_empty() {
                    visual::bubble_text(ui, &size, &message.text, &stamp, &title, color, checks);
                } else {
                    let (pos, galley, response) = egui::Label::new(job)
                        .wrap()
                        .sense(egui::Sense::click())
                        .layout_in_ui(ui);
                    ui.painter().galley(pos, galley.clone(), INK);
                    if let Some(pointer) = response.interact_pointer_pos()
                        && response.clicked()
                        && let Some((_, card)) = tags.iter().find(|(range, _)| {
                            range.contains(&galley.cursor_from_pos(pointer - pos).index)
                        })
                    {
                        self.open_profile((*card).clone());
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        visual::message_stamp(ui, &stamp, &title, color, checks);
                    });
                }
                if let Some(ticket) = &message.invitation
                    && !mine
                    && ui.link("Daveti aç").clicked()
                    && let Ok(code) = pack("invite", ticket)
                {
                    self.command(Command::Import(code));
                    self.page = Page::Contacts;
                }
            });
        let menu_click = ui
            .input(|input| {
                input
                    .pointer
                    .button_clicked(egui::PointerButton::Secondary)
                    .then(|| input.pointer.interact_pos())
                    .flatten()
            })
            .is_some_and(|point| {
                bubble.interact_rect.contains(point)
                    && ui.ctx().layer_id_at(point) == Some(bubble.layer_id)
            });
        egui::Popup::menu(&bubble)
            .at_pointer_fixed()
            .open_memory(menu_click.then_some(egui::SetOpenCommand::Bool(true)))
            .show(|ui| {
                if ui.button("Yanıtla").clicked() {
                    self.reply = Some((
                        message.seq,
                        message.author.data.name.clone(),
                        short(&message.text, 100),
                    ));
                    ui.close();
                }
                #[cfg(windows)]
                if let Some(file) = &message.attachment
                    && ui.button("Eki kaydet").clicked()
                    && let Some(path) = rfd::FileDialog::new().set_file_name(&file.name).save_file()
                {
                    self.command(Command::Download(selection.clone(), message.seq, path));
                    ui.close();
                }
                if ui.button("Mesaj bilgisi").clicked() {
                    self.message_details = Some((selection.clone(), message.seq));
                    ui.close();
                }
                if !message.text.is_empty() && ui.button("Metni kopyala").clicked() {
                    ui.ctx().copy_text(message.text.clone());
                    ui.close();
                }
            });
        if replied {
            self.reply = Some((
                message.seq,
                message.author.data.name.clone(),
                short(
                    message
                        .attachment
                        .as_ref()
                        .filter(|_| message.text.is_empty())
                        .map_or(message.text.as_str(), |file| file.name.as_str()),
                    100,
                ),
            ));
            ui.ctx().memory_mut(|memory| {
                memory.request_focus(
                    egui::Id::new(("conversation-composer", selection.room)).with("input"),
                )
            });
        }
        if self.jump == Some(message.seq) {
            bubble.scroll_to_me(Some(Align::Center));
            self.jump = None;
        }
    }

    fn contacts(&mut self, ui: &mut Ui) {
        visual::page_heading(
            ui,
            Outline::People,
            "Kişiler ve istekler",
            "Kişi kartı paylaş, bağlantı ekle ve gelen istekleri yönet.",
        );
        let local = self
            .view
            .as_ref()
            .map(|v| v.local.clone())
            .unwrap_or_default();
        let own_card = self.identity.as_ref().map(|identity| identity.card.clone());
        ui.horizontal_wrapped(|ui| {
            if ui.button("Bağlantımı paylaş").clicked() {
                self.command(Command::ShareContact);
            }
            if self.view.as_ref().is_some_and(|view| view.host.is_none())
                && ui.button("Hostu aç veya ağ seç").clicked()
            {
                self.page = Page::Host;
            }
        });
        if let Some(route) = self.contact_target.clone() {
            visual::sheet(ui, |ui| {
                ui.heading(&route.data.card.data.name);
                ui.weak(format!(
                    "@{} · {}",
                    route.data.card.data.handle,
                    if route.data.card.data.role == Role::Agent {
                        "Ajan"
                    } else {
                        "İnsan"
                    }
                ));
                if route.data.host.socket.ip().is_loopback() {
                    ui.weak("Bu cihazda kullanılır.");
                }
                if ui.button("Arkadaşlık isteği gönder").clicked() {
                    self.command(Command::OfferContact(route));
                    self.contact_target = None;
                }
                if ui.button("Vazgeç").clicked() {
                    self.contact_target = None;
                }
            });
        }
        let share = |ui: &mut Ui| {
            visual::sheet(ui, |ui| {
                visual::label_icon(ui, Outline::People, RichText::new("Kişi kartım").heading());
                if let Some(card) = &own_card {
                    ui.horizontal(|ui| {
                        visual::avatar(ui, &card.data.name, 38.);
                        ui.vertical(|ui| {
                            ui.strong(&card.data.name);
                            ui.weak(format!("@{}", card.data.handle));
                        });
                    });
                    ui.add_space(4.);
                    if visual::work_tab(ui, Outline::Copy, "Kişi kartımı kopyala", false).clicked()
                        && let Ok(code) = pack("card", card)
                    {
                        ui.ctx().copy_text(code);
                    }
                }
            })
        };
        let mut import = |ui: &mut Ui| {
            visual::sheet(ui, |ui| {
                visual::label_icon(ui, Outline::Link, RichText::new("Bağlantı ekle").heading());
                ui.strong("Kişi kartı, davet veya host bağlantısı");
                let input = ui.add_sized(
                    [ui.available_width(), 36.],
                    egui::TextEdit::singleline(&mut self.import)
                        .hint_text("ChatOid kodunu buraya yapıştır")
                        .vertical_align(Align::Center)
                        .margin(egui::Margin::symmetric(10, 5)),
                );
                let submit = ui
                    .horizontal_wrapped(|ui| {
                        if ui.button("Bağlantı yapıştır").clicked() {
                            input.request_focus();
                            ui.ctx()
                                .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                        }
                        ui.add_enabled(
                            !self.import.trim().is_empty(),
                            visual::primary(ui, "İçe aktar"),
                        )
                        .clicked()
                    })
                    .inner;
                if submit
                    || (!self.import.trim().is_empty()
                        && input.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    let code = std::mem::take(&mut self.import);
                    if code.trim().starts_with("chatoid:contact:") {
                        match crate::identity::unpack("contact", &code).and_then(|route| {
                            crate::protocol::ContactRoute::validate(&route)?;
                            Ok(route)
                        }) {
                            Ok(route) => self.contact_target = Some(route),
                            Err(error) => self.notice = format!("Bağlantı açılamadı. {error:#}"),
                        }
                    } else {
                        self.command(Command::Import(code));
                    }
                }
            })
        };
        if ui.available_width() >= 570. {
            ui.columns(2, |columns| {
                share(&mut columns[0]);
                import(&mut columns[1]);
            });
        } else {
            share(ui);
            import(ui);
        }
        ui.add_space(12.);
        self.local_people(ui, &local);
        visual::label_icon(ui, Outline::People, RichText::new("Kişiler").heading());
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width().min(410.), 0.),
            Layout::top_down(Align::Min),
            |ui| {
                field(ui, "Kullanıcı adıyla ara", &mut self.search, "ör. @poyraz");
            },
        );
        let query = self
            .search
            .trim()
            .trim_start_matches('@')
            .to_ascii_lowercase();
        let mut shown = 0;
        for card in local
            .contacts
            .values()
            .filter(|c| c.data.handle.contains(&query))
        {
            shown += 1;
            ui.push_id(&card.data.key, |ui| {
                visual::sheet(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let avatar = visual::participant_avatar(
                            ui,
                            card,
                            local
                                .hosts
                                .values()
                                .find_map(|link| link.avatars.get(&card.data.key)),
                        );
                        avatar.context_menu(|ui| self.person_actions(ui, card));
                        if avatar.clicked() {
                            self.open_profile(card.clone());
                        }
                        ui.vertical(|ui| {
                            ui.strong(&card.data.name);
                            ui.weak(format!(
                                "@{} · {} · {}",
                                card.data.handle,
                                if card.data.role == Role::Agent {
                                    "Ajan"
                                } else {
                                    "İnsan"
                                },
                                short(&card.data.key, 7)
                            ));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.menu_button("•••", |ui| self.person_actions(ui, card));
                            if !local.already_requested(self.id(), &card.data.key)
                                && ui.button("Arkadaşlık isteği gönder").clicked()
                            {
                                self.command(Command::Offer(card.clone()));
                            }
                        });
                    });
                });
            });
        }
        if shown == 0 {
            visual::sheet(ui, |ui| {
                ui.weak(if self.view.is_none() {
                    "Kişiler yükleniyor…"
                } else if local.contacts.is_empty() {
                    "Henüz kişi eklenmedi."
                } else {
                    "Bu kullanıcı adıyla eşleşen kişi yok."
                });
            });
        }
        ui.add_space(12.);
        visual::label_icon(ui, Outline::Chat, RichText::new("İstekler").heading());
        if !local.hosts.values().any(|link| !link.requests.is_empty()) {
            visual::sheet(ui, |ui| {
                ui.weak(if self.view.is_none() {
                    "İstekler yükleniyor…"
                } else {
                    "Henüz gelen veya gönderilen istek yok."
                });
            });
        }
        for (pin, link) in &local.hosts {
            for request in &link.requests {
                ui.push_id((pin, request.terms.data.id), |ui| {
                    visual::sheet(ui, |ui| self.request(ui, pin, request, &local))
                });
            }
        }
    }

    fn person_actions(&mut self, ui: &mut Ui, card: &Card) {
        if ui.button("Profili aç").clicked() {
            self.open_profile(card.clone());
            ui.close();
        }
        if ui.button("Kişi kartını kopyala").clicked()
            && let Ok(code) = pack("card", card)
        {
            ui.ctx().copy_text(code);
            ui.close();
        }
        let room = self.view.as_ref().and_then(|view| {
            view.local.hosts.iter().find_map(|(pin, link)| {
                link.rooms
                    .iter()
                    .find(|room| {
                        !room.group
                            && !room.closed
                            && room
                                .members
                                .iter()
                                .any(|member| member.data.key == card.data.key)
                    })
                    .map(|room| Selection {
                        host: pin.clone(),
                        room: room.id,
                    })
            })
        });
        if let Some(room) = room
            && ui.button("Arkadaşlıktan çıkar").clicked()
        {
            self.pending_removal = Some(room);
            ui.close();
        }
    }

    fn local_people(&mut self, ui: &mut Ui, local: &crate::session::Local) {
        let Some(dirs) = ProjectDirs::from("org", "ChatOid", "ChatOid") else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(dirs.data_local_dir().join("local-invitations")) else {
            return;
        };
        let cards: Vec<Card> = entries
            .flatten()
            .take(100)
            .filter_map(|entry| {
                let path = entry.path().join("public.card");
                if path.metadata().ok()?.len() > 16384 {
                    return None;
                }
                let code = std::fs::read_to_string(path).ok()?;
                let card: Card = crate::identity::unpack("card", &code).ok()?;
                card.check().ok()?;
                (card.data.key != self.id() && !local.already_requested(self.id(), &card.data.key))
                    .then_some(card)
            })
            .collect();
        if !cards.is_empty() {
            ui.heading("Bu cihazdaki hesaplar");
            for card in cards {
                if ui
                    .button(format!("{} · @{}", card.data.name, card.data.handle))
                    .clicked()
                {
                    self.command(Command::Offer(card));
                }
            }
        }
    }

    fn chat_dialogs(&mut self, ctx: &UiContext) {
        if let Some(card) = self.profile_card.clone() {
            let mut open = true;
            if ctx.content_rect().width() >= 950. {
                let colors = visual::palette(self.role);
                egui::SidePanel::right("participant-details")
                    .default_width(307.)
                    .min_width(252.)
                    .frame(Frame::new().fill(colors.base).inner_margin(12))
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            visual::label_icon(
                                ui,
                                Outline::People,
                                RichText::new("Katılımcı profili").size(15.).strong(),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if visual::rail_button(ui, Outline::Cross, "Kapat", false).clicked()
                                {
                                    open = false;
                                }
                            });
                        });
                        ui.add_space(6.);
                        ScrollArea::vertical().show(ui, |ui| self.profile_contents(ui, &card));
                    });
            } else {
                egui::Window::new("Katılımcı bilgisi")
                    .open(&mut open)
                    .resizable(false)
                    .show(ctx, |ui| self.profile_contents(ui, &card));
            }
            if !open {
                self.profile_card = None;
            }
        }
        if let Some(selection) = self.pending_removal.clone() {
            egui::Window::new("Arkadaşlıktan çıkar").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label("Bire bir sohbet iki taraf için salt okunur olur. Geçmiş ve ortak gruplar kalır.");
                ui.horizontal(|ui| {
                    if ui.button("Vazgeç").clicked() { self.pending_removal = None; }
                    if ui.button("Arkadaşlığı bitir").clicked() {
                        self.command(Command::EndFriendship(selection));
                        self.pending_removal = None;
                    }
                });
            });
        }
        if let Some((selection, seq)) = &self.message_details
            && let Some(link) = self
                .view
                .as_ref()
                .and_then(|view| view.local.hosts.get(&selection.host))
            && let Some(message) = link.messages.get(seq)
        {
            let mut open = true;
            egui::Window::new("Mesaj bilgisi")
                .open(&mut open)
                .resizable(false)
                .show(ctx, |ui| {
                    visual::label_icon(
                        ui,
                        Outline::People,
                        RichText::new(&message.author.data.name).strong(),
                    );
                    ui.weak(format!(
                        "{} · {}",
                        calendar_date(message.sent_at),
                        time(message.sent_at)
                    ));
                    if let Some(file) = &message.attachment {
                        visual::label_icon(
                            ui,
                            Outline::Attachment,
                            RichText::new(format!("{} · {}", file.name, file_size(file.bytes))),
                        );
                    }
                    if message.author.data.key == self.id()
                        && let Some(room) = link.rooms.iter().find(|room| room.id == message.room)
                    {
                        ui.separator();
                        let mut recipients = room
                            .members
                            .iter()
                            .filter_map(|card| {
                                let receipt = link.receipts.iter().find(|receipt| {
                                    receipt.room == room.id
                                        && receipt.actor == card.data.key
                                        && message.seq > receipt.joined_after
                                })?;
                                Some((card, receipt))
                            })
                            .peekable();
                        if recipients.peek().is_none() {
                            ui.weak("Alıcı durumu henüz yok.");
                        }
                        for (card, receipt) in recipients {
                            let status = if receipt.read >= message.seq {
                                "Okundu"
                            } else if receipt.delivered >= message.seq {
                                "Teslim edildi"
                            } else {
                                "Gönderildi"
                            };
                            ui.label(format!("{} · {status}", card.data.name));
                        }
                    }
                });
            if !open {
                self.message_details = None;
            }
        }
    }

    fn request(
        &mut self,
        ui: &mut Ui,
        pin: &str,
        request: &Request,
        local: &crate::session::Local,
    ) {
        let t = &request.terms.data;
        let outgoing = t.from == self.id();
        let person = if outgoing { &request.to } else { &request.from };
        let status = match request.status {
            Status::Pending if local.group_decision_waiting(t.id) => "Doğrulama bekliyor",
            Status::Pending if local.introduction_waiting(t.id) => "Teslim bekliyor",
            Status::Pending => "Bekliyor",
            Status::Accepted => "Kabul edildi",
            Status::Rejected => "Reddedildi",
            Status::Expired => "Süresi doldu",
        };
        ui.horizontal_wrapped(|ui| {
            visual::avatar(ui, &person.data.name, 30.);
            ui.strong(&person.data.name);
            ui.weak(request.room_name.as_deref().unwrap_or("Arkadaşlık"));
            visual::status(ui, status, request.status == Status::Accepted);
            if request.status == Status::Pending {
                if let Some(ticket) = local.tickets.get(&t.id) {
                    if outgoing
                        && ui.small_button("Davet bağlantısını kopyala").clicked()
                        && let Ok(code) = pack("invite", ticket)
                    {
                        ui.ctx().copy_text(code);
                    }
                    if !outgoing
                        && let Some(operator) = &local.operator
                        && ui.small_button("Operatör bağlantısını kopyala").clicked()
                    {
                        let mut ticket = ticket.clone();
                        if let Some(agent) = &self.identity {
                            match Grant::issue(agent, operator.clone(), t).and_then(|grant| {
                                ticket.grant = Some(grant);
                                pack("invite", &ticket)
                            }) {
                                Ok(code) => ui.ctx().copy_text(code),
                                Err(error) => self.notice = error.to_string(),
                            }
                        }
                    }
                }
                let manual = self.role == Role::Human
                    && (t.to == self.id()
                        || local.grants.iter().any(|g| {
                            g.agent.data.key == t.to
                                && g.proof.data.request == t.id
                                && g.proof.data.host == pin
                        }));
                if !outgoing && manual {
                    if visual::work_tab(ui, Outline::Check, "Kabul et", false).clicked() {
                        self.command(Command::Decide(pin.into(), Box::new(request.clone()), true));
                    }
                    if visual::work_tab(ui, Outline::Cross, "Reddet", false).clicked() {
                        self.command(Command::Decide(
                            pin.into(),
                            Box::new(request.clone()),
                            false,
                        ));
                    }
                }
            } else if request.status == Status::Accepted && self.role == Role::Human {
                let room = t.room.unwrap_or(t.id);
                if local
                    .hosts
                    .get(pin)
                    .is_some_and(|h| h.rooms.iter().any(|r| r.id == room))
                    && ui.small_button("Sohbeti aç").clicked()
                {
                    self.page = if local.hosts[pin].archived.contains(&room) {
                        Page::Archive
                    } else {
                        Page::Chats
                    };
                    self.choose(Selection {
                        host: pin.to_owned(),
                        room,
                    });
                }
            }
        });
    }

    fn host(&mut self, ui: &mut Ui) {
        visual::page_heading(
            ui,
            Outline::Host,
            "Host",
            "Bu cihazın bağlantısını ve kayıtlı hostları yönet.",
        );
        let address = self.view.as_ref().and_then(|v| v.host.clone());
        let connections = self
            .view
            .as_ref()
            .map(|v| {
                v.local
                    .hosts
                    .values()
                    .map(|link| (link.address.socket, link.online, link.error.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let hosting = address.is_some();
        let loading = self.view.is_none();
        let connected = connections.iter().filter(|(_, online, _)| *online).count();
        let mut setup = |ui: &mut Ui| {
            visual::sheet(ui, |ui| {
                ui.set_min_height(152.);
                ui.horizontal_wrapped(|ui| {
                    visual::label_icon(ui, Outline::Host, RichText::new("Bu cihaz").heading());
                    visual::status(
                        ui,
                        if address.is_some() {
                            "Çalışıyor"
                        } else {
                            "Kapalı"
                        },
                        address.is_some(),
                    );
                });
                ui.weak("Barındırdığın sohbetlerin geçmişi bu cihazda saklanır.");
                if let Some(address) = &address {
                    ui.add_space(3.);
                    ui.label(
                        RichText::new(address.socket.to_string())
                            .monospace()
                            .strong(),
                    );
                    if address.socket.ip().is_loopback() {
                        ui.weak("Bu adresle yalnızca bu cihazdan bağlanılabilir.");
                    }
                    ui.horizontal_wrapped(|ui| {
                        if visual::work_tab(ui, Outline::Copy, "Host bağlantısını kopyala", true)
                            .clicked()
                            && let Ok(code) = pack("host", address)
                        {
                            ui.ctx().copy_text(code);
                        }
                        if ui.button("Hostu durdur").clicked() {
                            self.command(Command::Stop);
                        }
                    });
                } else {
                    ui.horizontal_wrapped(|ui| {
                        egui::ComboBox::from_id_salt("host_interface")
                            .selected_text("LAN / VPN arayüzü seç")
                            .width(185.)
                            .show_ui(ui, |ui| {
                                let default_port = if self.role == Role::Human {
                                    47821
                                } else {
                                    47822
                                };
                                let selected = self.bind.trim().parse::<SocketAddr>().ok();
                                let port = selected.map_or(default_port, |addr| addr.port());
                                for (name, ip) in &self.interfaces {
                                    let label = if ip.is_loopback() {
                                        format!("Bu cihazda dene · {ip}")
                                    } else {
                                        format!("{name} · {ip}")
                                    };
                                    if ui
                                        .selectable_label(
                                            selected.is_some_and(|addr| addr.ip() == *ip),
                                            label,
                                        )
                                        .clicked()
                                    {
                                        self.bind = SocketAddr::new(*ip, port).to_string();
                                    }
                                }
                                if self.interfaces.is_empty() {
                                    ui.weak("Uygun arayüz bulunamadı.");
                                }
                            });
                        if ui.button("Yenile").clicked()
                            && let Err(error) = self.refresh_interfaces()
                        {
                            self.notice = format!("{error:#}");
                        }
                    });
                    field(
                        ui,
                        "IP adresi ve port",
                        &mut self.bind,
                        "192.168.1.20:47821",
                    );
                    if visual::work_tab(ui, Outline::Host, "Bu cihazı host yap", true).clicked() {
                        match self.bind.trim().parse() {
                            Ok(bind) => self.command(Command::Start(bind)),
                            Err(_) => {
                                self.notice = "IP adresi ve port gir, ör. 192.168.1.20:47821".into()
                            }
                        }
                    }
                }
            })
        };
        let map = |ui: &mut Ui| {
            visual::sheet(ui, |ui| {
                ui.set_min_height(152.);
                visual::label_icon(ui, Outline::Host, RichText::new("Ağ durumu").heading());
                visual::host_map(ui, hosting, connected);
                ui.weak(if loading {
                    "Bağlantılar yükleniyor…".into()
                } else if connected == 0 {
                    "Etkin bağlantı yok".into()
                } else {
                    format!("{connected} etkin bağlantı")
                });
            });
        };
        if ui.available_width() >= 610. {
            ui.columns(2, |columns| {
                setup(&mut columns[0]);
                map(&mut columns[1]);
            });
        } else {
            setup(ui);
            map(ui);
        }
        ui.add_space(14.);
        ui.horizontal_wrapped(|ui| {
            visual::label_icon(
                ui,
                Outline::Host,
                RichText::new("Kayıtlı hostlar").heading(),
            );
            if visual::work_tab(ui, Outline::Link, "Başka hosta bağlan", false).clicked() {
                self.page = Page::Contacts;
            }
        });
        if connections.is_empty() {
            visual::sheet(ui, |ui| {
                ui.weak(if self.view.is_none() {
                    "Bağlantılar yükleniyor…"
                } else {
                    "Henüz kayıtlı host yok."
                });
            });
        }
        for (socket, online, error) in connections {
            visual::sheet(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(socket.to_string()).monospace().strong());
                    visual::status(ui, if online { "Bağlı" } else { "Çevrimdışı" }, online);
                    if !online && !error.is_empty() {
                        ui.weak(&error);
                    }
                });
            });
        }
    }

    fn agent(&mut self, ui: &mut Ui) {
        let local = self
            .view
            .as_ref()
            .map(|v| v.local.clone())
            .unwrap_or_default();
        visual::label_icon(ui, Outline::Agent, RichText::new("Ajan kimliği").heading());
        if let Some(identity) = self.identity.as_ref() {
            ui.label(format!(
                "{}  @{}",
                identity.card.data.name, identity.card.data.handle
            ));
            if let Some(profile) = identity.agent_profile() {
                ui.weak(format!("Model · {}", profile.model));
                ui.label(&profile.duty);
                if let Some(parent) = &profile.parent_agent {
                    ui.weak(format!("Alt ajan · Üst ajan {parent}"));
                }
            }
        }
        if let Some(operator) = local.operator.as_ref() {
            ui.weak(format!("Operatör · {}", operator.data.name));
        }
        field(
            ui,
            "Operatörün insan kişi kartı",
            &mut self.pair,
            "ChatOid kişi kartını yapıştır",
        );
        if ui
            .add_enabled(
                !self.pair.trim().is_empty(),
                egui::Button::new("Operatörü eşleştir"),
            )
            .clicked()
        {
            let card = std::mem::take(&mut self.pair);
            self.command(Command::Pair(card));
        }
        ui.separator();
        visual::label_icon(
            ui,
            Outline::Link,
            RichText::new("Codex bağlantısı").heading(),
        );
        if let Some(view) = &self.view {
            ui.label(&view.codex);
        }
        self.settings_dirty |= field(
            ui,
            "Codex çalıştırılabilir dosyası",
            &mut self.settings.codex,
            "codex",
        );
        self.settings_dirty |= ui
            .checkbox(
                &mut self.settings.enabled,
                "Yeni sohbet mesajlarını değerlendir",
            )
            .changed();
        if visual::work_tab(ui, Outline::Lock, "OpenAI ile giriş yap", true).clicked() {
            self.command(Command::SignInCodex(self.settings.codex.clone()));
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("Deneme yanıtı al").clicked() {
                self.command(Command::Settings(self.settings.clone()));
                self.command(Command::CheckCodex);
            }
            if ui.button("Son hatayı yeniden dene").clicked() {
                self.command(Command::RetryCodex);
            }
        });
        ui.separator();
        visual::label_icon(
            ui,
            Outline::Archive,
            RichText::new("Çalışma klasörü").heading(),
        );
        self.settings_dirty |= field(
            ui,
            "Salt okunur klasör",
            &mut self.settings.workspace,
            "Klasörün tam yolu",
        );
        ui.weak("Grup incelemelerinin sonucu gruptaki üyelerle paylaşılır.");
        for (pin, link) in &local.hosts {
            for room in link.rooms.iter().filter(|room| room.active && !room.closed) {
                ui.collapsing(&room.name, |ui| {
                    for card in room.members.iter().filter(|c| c.data.role == Role::Human) {
                        let permit = FilePermit {
                            actor: card.data.key.clone(),
                            host: pin.clone(),
                            room: room.id,
                        };
                        let mut enabled = self.settings.permits.contains(&permit);
                        if ui
                            .checkbox(
                                &mut enabled,
                                format!("{} · @{}", card.data.name, card.data.handle),
                            )
                            .changed()
                        {
                            if enabled {
                                self.settings.permits.push(permit);
                            } else {
                                self.settings.permits.retain(|p| p != &permit);
                            }
                            self.settings_dirty = true;
                        }
                    }
                });
            }
        }
        ui.separator();
        visual::label_icon(
            ui,
            Outline::Chat,
            RichText::new("Otomatik istek kararları").heading(),
        );
        self.settings_dirty |= ui
            .checkbox(
                &mut self.settings.allow_friend,
                "Arkadaşlık isteklerinde Codex karar verebilir",
            )
            .changed();
        self.settings_dirty |= ui
            .checkbox(
                &mut self.settings.allow_group,
                "Grup davetlerinde Codex karar verebilir",
            )
            .changed();
        self.settings_dirty |= ui
            .add(
                egui::TextEdit::multiline(&mut self.settings.rules)
                    .hint_text("Karar kuralları")
                    .desired_width(f32::INFINITY),
            )
            .changed();
        if ui.button("Ayarları kaydet").clicked() {
            self.settings_dirty = false;
            self.command(Command::Settings(self.settings.clone()));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &UiContext, _frame: &mut eframe::Frame) {
        #[cfg(windows)]
        {
            if self.paste_hook.is_none() {
                self.paste_hook = paste::NativePaste::install(_frame);
            }
            self.paste_pressed = self
                .paste_hook
                .as_ref()
                .is_some_and(paste::NativePaste::take);
        }
        self.update_view(ctx);
        if ctx.input(|input| input.viewport().close_requested()) {
            self.save_preferences();
            return;
        }
        if self.identity.is_none() {
            self.login(ctx);
            #[cfg(windows)]
            taskbar::update(&mut self.taskbar_icon, _frame, self.role);
            return;
        }
        #[cfg(windows)]
        taskbar::update(&mut self.taskbar_icon, _frame, self.role);
        self.navigation(ctx);
        if self.identity.is_none() {
            ctx.request_repaint();
            return;
        }
        self.chat_dialogs(ctx);
        self.workspace(ctx);
        self.save_preferences();
        ctx.request_repaint_after(Duration::from_millis(180));
    }
}

fn profile_value(ui: &mut Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).size(11.).color(MUTED));
    ui.add(egui::Label::new(RichText::new(value).size(13.).color(INK)).wrap());
    ui.add_space(3.);
}

fn show_device(ui: &mut Ui, device: &crate::protocol::DeviceSummary) {
    ui.heading("Cihaz özeti");
    ui.label(format!("{} · {}", device.system, device.architecture));
    ui.label(format!("{} · {}", device.processor, device.memory));
}

fn auth_input(ui: &mut Ui, label: &str, placeholder: &str, value: &mut String) -> egui::Response {
    let icon = match label {
        ".key dosyası (isteğe bağlı)" => Outline::Lock,
        "Model adı" => Outline::Agent,
        "Görevi" => Outline::Chat,
        _ => Outline::People,
    };
    visual::label_icon(ui, icon, RichText::new(label).strong());
    ui.add_sized(
        [ui.available_width(), 41.],
        visual::auth_text(ui, value, placeholder),
    )
}

fn field(ui: &mut Ui, label: &str, value: &mut String, placeholder: &str) -> bool {
    let icon = match label {
        "Kullanıcı adıyla ara" => Outline::Search,
        "IP adresi ve port" => Outline::Host,
        "Grup adı" => Outline::People,
        _ => Outline::Link,
    };
    visual::label_icon(ui, icon, RichText::new(label).size(13.).strong());
    ui.add_sized(
        [ui.available_width(), 36.],
        egui::TextEdit::singleline(value)
            .hint_text(RichText::new(placeholder).color(MUTED))
            .vertical_align(Align::Center)
            .margin(egui::Margin::symmetric(10, 5)),
    )
    .changed()
}
fn nav(ui: &mut Ui, page: &mut Page, choice: Page, label: &str) {
    let icon = match choice {
        Page::Chats => Outline::Chat,
        Page::Archive => Outline::Archive,
        Page::Contacts => Outline::People,
        Page::Host => Outline::Host,
        Page::Agent => Outline::Agent,
        Page::Settings => Outline::Settings,
        Page::Profile => Outline::People,
    };
    if visual::work_tab(ui, icon, label, *page == choice).clicked() {
        *page = choice;
    }
}
fn short(text: &str, limit: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut preview: String = line.chars().take(limit).collect();
    if preview.len() < line.len() || line.len() < text.len() {
        preview.push('…');
    }
    preview
}
fn time(epoch: i64) -> String {
    let date = local_time(epoch);
    format!("{:02}:{:02}", date.hour(), date.minute())
}

fn local_time(epoch: i64) -> ::time::OffsetDateTime {
    let date = ::time::OffsetDateTime::from_unix_timestamp(epoch)
        .unwrap_or(::time::OffsetDateTime::UNIX_EPOCH);
    match ::time::UtcOffset::local_offset_at(date) {
        Ok(offset) => date.to_offset(offset),
        Err(_) => date,
    }
}

fn calendar_date(epoch: i64) -> String {
    let date = local_time(epoch);
    format!(
        "{:02}.{:02}.{:04}",
        date.day(),
        u8::from(date.month()),
        date.year()
    )
}

fn day_label(epoch: i64) -> String {
    match (local_time(crate::protocol::now()).date() - local_time(epoch).date()).whole_days() {
        0 => "Bugün".into(),
        1 => "Dün".into(),
        _ => calendar_date(epoch),
    }
}

fn file_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.)
    }
}

fn mention_query(text: &str) -> Option<(usize, &str)> {
    let start = text.rfind('@')?;
    let query = &text[start + 1..];
    if !text[..start]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
        && query.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        Some((start, query))
    } else {
        None
    }
}

fn mention_body<'a>(
    body: &str,
    room: &'a Room,
    accent: Color32,
) -> (
    egui::text::LayoutJob,
    Vec<(std::ops::Range<usize>, &'a Card)>,
) {
    let mut job = egui::text::LayoutJob::default();
    let mut tags = Vec::new();
    let plain = egui::TextFormat::simple(egui::FontId::proportional(14.), INK);
    let tagged = egui::TextFormat::simple(egui::FontId::proportional(14.), accent);
    let mut previous = 0;
    for range in mention_ranges(body) {
        let handle = &body[range.start + 1..range.end];
        if let Some(member) = room
            .members
            .iter()
            .find(|member| member.data.handle.eq_ignore_ascii_case(handle))
        {
            job.append(&body[previous..range.start], 0., plain.clone());
            let start = job.text.chars().count();
            job.append(&format!("@{}", member.data.name), 0., tagged.clone());
            tags.push((start..job.text.chars().count(), member));
            previous = range.end;
        }
    }
    job.append(&body[previous..], 0., plain);
    (job, tags)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeliveryStatus {
    Sent,
    Delivered,
    Read,
}

impl DeliveryStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Sent => "Gönderildi",
            Self::Delivered => "Teslim edildi",
            Self::Read => "Okundu",
        }
    }

    fn checks(self) -> u8 {
        if self == Self::Sent { 1 } else { 2 }
    }
}

fn delivery_status(message: &Message, receipts: &[Receipt]) -> DeliveryStatus {
    let mut matching = receipts
        .iter()
        .filter(|receipt| receipt.room == message.room && receipt.joined_after < message.seq)
        .peekable();
    if matching.peek().is_none() {
        return DeliveryStatus::Sent;
    }
    let mut all_read = true;
    for receipt in matching {
        if receipt.delivered < message.seq && receipt.read < message.seq {
            return DeliveryStatus::Sent;
        }
        all_read &= receipt.read >= message.seq;
    }
    if all_read {
        DeliveryStatus::Read
    } else {
        DeliveryStatus::Delivered
    }
}

#[cfg(test)]
mod visual_tests {
    use super::*;

    #[test]
    fn left_role_card_changes_form_and_profile_directory() {
        let ctx = UiContext::default();
        visual::theme(&ctx, Role::Human);
        let mut app = App::new(Role::Human, PathBuf::from("profiles-for-test"));
        app.handle = "deniz".into();
        app.password = "example".into();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1080., 720.));
        let mut render = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| app.login(ctx),
            )
        };
        let initial = render(vec![]);
        let agent_card = initial
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape
                    && text.galley.job.text == "Ajan"
                {
                    return Some(shape.shape.visual_bounding_rect().center());
                }
                None
            })
            .expect("the agent role card is visible");
        let click = |pressed| egui::Event::PointerButton {
            pos: agent_card,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        render(vec![egui::Event::PointerMoved(agent_card), click(true)]);
        let updated = render(vec![click(false)]);
        assert!(updated.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == "Ajan anahtarını aç")
        }));
        assert_eq!(app.role, Role::Agent);
        let agent_colors = visual::palette(Role::Agent);
        assert_eq!(ctx.style().visuals.panel_fill, agent_colors.base);
        assert_eq!(ctx.style().visuals.hyperlink_color, agent_colors.accent);
        assert_ne!(
            visual::app_icon(Role::Human).rgba,
            visual::app_icon(Role::Agent).rgba,
        );
        assert_eq!(
            app.profile_root(),
            PathBuf::from("profiles-for-test").join("agent")
        );
        assert!(app.password.is_empty());
        assert!(app.handle.is_empty());
    }

    #[test]
    fn enter_moves_from_handle_to_password() {
        let ctx = UiContext::default();
        visual::theme(&ctx, Role::Human);
        let mut app = App::new(Role::Human, PathBuf::new());
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1080., 720.));
        let mut draw = |events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| app.login(ctx),
            );
        };
        draw(vec![]);
        draw(vec![egui::Event::Text("deniz".into())]);
        draw(vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        draw(vec![egui::Event::Text("example".into())]);
        assert_eq!(app.handle, "deniz");
        assert_eq!(app.password, "example");
        assert!(
            app.auth_activity.is_none(),
            "typing must not trigger the role switch animation"
        );
    }

    #[test]
    fn eye_click_toggles_password_in_signup() {
        let ctx = UiContext::default();
        visual::theme(&ctx, Role::Human);
        let mut app = App::new(Role::Human, PathBuf::new());
        app.signing_up = true;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(720., 505.));
        let mut draw = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| app.login(ctx),
            )
        };
        let frame = draw(vec![]);
        let eye = frame
            .shapes
            .iter()
            .find_map(|clipped| {
                matches!(clipped.shape, egui::Shape::QuadraticBezier(_))
                    .then(|| clipped.shape.visual_bounding_rect().center())
            })
            .expect("password eye is drawn");
        let click = |pressed| egui::Event::PointerButton {
            pos: eye,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        draw(vec![egui::Event::PointerMoved(eye), click(true)]);
        draw(vec![click(false)]);
        let revealed = draw(vec![]);
        assert!(revealed.shapes.iter().any(|clipped| {
            matches!(clipped.shape, egui::Shape::LineSegment { points, .. } if points[0].distance(eye) < 14. && points[1].distance(eye) < 14.)
        }));
        draw(vec![click(true)]);
        draw(vec![click(false)]);
        assert!(!app.show_password);
    }

    #[test]
    fn agent_signup_shows_inline_fields_and_creates_key_in_one_step() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let ctx = UiContext::default();
        visual::theme(&ctx, Role::Agent);
        let mut app = App::new(Role::Agent, directory.path().into());
        app.signing_up = true;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1080., 720.));
        let draw = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| app.login(ctx),
            )
        };
        fn label(frame: &egui::FullOutput, name: &str) -> Option<egui::Pos2> {
            frame.shapes.iter().find_map(|shape| {
                let center = shape.shape.visual_bounding_rect().center();
                (matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == name)
                    && shape.clip_rect.contains(center))
                .then_some(center)
            })
        }
        let initial = draw(&mut app, vec![]);
        for name in ["Kullanıcı adı", "Model adı", "Görevi", "Bu bir alt ajan"] {
            assert!(
                label(&initial, name).is_some(),
                "{name} is visible when signup opens"
            );
        }
        assert!(label(&initial, "Üst ajanın adı").is_none());
        app.handle = "poyraz".into();
        app.name = "Poyraz".into();
        app.agent_profile = AgentProfile {
            model: "Qwen 2.7 27B".into(),
            duty: "Kurulumu doğrula".into(),
            is_sub_agent: true,
            parent_agent: Some("Kıvılcım".into()),
        };
        let filled = draw(&mut app, vec![]);
        assert!(label(&filled, "Üst ajanın adı").is_some());
        let pos = label(&filled, ".key dosyasını oluştur").expect("submit is visible");
        let click = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        draw(&mut app, vec![egui::Event::PointerMoved(pos), click(true)]);
        draw(&mut app, vec![click(false)]);
        let saved = Identity::load(&app.identity_path()?, Role::Agent, None)?;
        assert_eq!(saved.agent_profile(), Some(&app.agent_profile));
        Ok(())
    }

    #[test]
    fn invalid_handle_has_red_border_and_a_rule_popup() {
        let ctx = UiContext::default();
        visual::theme(&ctx, Role::Human);
        visual::install_auth_font(&ctx);
        let mut app = App::new(Role::Human, PathBuf::new());
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1080., 720.));
        let draw = |app: &mut App| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(rect),
                    ..Default::default()
                },
                |ctx| app.login(ctx),
            )
        };
        app.handle = "@".into();
        draw(&mut app);
        let invalid = draw(&mut app);
        assert!(invalid.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == HANDLE_RULE)));
        let has_border = |frame: &egui::FullOutput| {
            frame.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Rect(field) if field.stroke.color == visual::INVALID && field.rect.width() > 300.))
        };
        assert!(has_border(&invalid));
        app.handle = "deniz".into();
        assert!(!has_border(&draw(&mut app)));
    }

    #[test]
    fn short_messages_stay_compact_and_composer_stays_at_the_bottom() -> Result<()> {
        let me = Identity::generate(Role::Human, "deniz", "Deniz")?;
        let other = Identity::generate(Role::Agent, "asistan", "ChatOid Asistan")?;
        let room = Room {
            id: uuid::Uuid::new_v4(),
            name: "ChatOid Asistan".into(),
            owner: me.id().into(),
            group: false,
            active: true,
            closed: false,
            members: vec![me.card.clone(), other.card.clone()],
        };
        let make = |seq, author, text: String| Message {
            seq,
            room: room.id,
            client_id: uuid::Uuid::new_v4(),
            author,
            text,
            sent_at: 62_760,
            reply: None,
            root: None,
            invitation: None,
            attachment: None,
        };
        let messages = [
            make(1, me.card.clone(), "kendini tanıt".into()),
            make(2, other.card.clone(), "Merhaba!".into()),
            make(
                3,
                other.card.clone(),
                "Bu uzun mesaj ekranı yatay olarak genişletmeden satırlara bölünür. ".repeat(4),
            ),
        ];
        for [width, height] in [[720., 505.], [1080., 720.]] {
            let ctx = UiContext::default();
            visual::theme(&ctx, Role::Human);
            let mut app = App::new(Role::Human, PathBuf::new());
            app.identity = Some(me.clone());
            let selection = Selection {
                host: "test".into(),
                room: room.id,
            };
            let mut link: Link = serde_json::from_value(serde_json::json!({
                "address":{"socket":"127.0.0.1:47822","pin":"test"},"cursor":3,
                "rooms":[room],"requests":[],"messages":{"1":messages[0],"2":messages[1],"3":messages[2]},
                "queue":[],"archived":[],"seen":{}
            }))?;
            link.online = true;
            let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, height));
            let mut input = egui::RawInput {
                screen_rect: Some(bounds),
                ..Default::default()
            };
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .native_pixels_per_point = Some(1.5);
            let output = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    app.conversation(ui, selection.clone(), link.clone(), room.clone())
                });
            });
            let text = |label: &str| {
                output.shapes.iter().find_map(|shape| {
                    matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == label)
                        .then(||shape.shape.visual_bounding_rect().center())
                }).expect("visible text")
            };
            let sent = text("kendini tanıt");
            let received = text("Merhaba!");
            let colors = visual::palette(Role::Human);
            let bubble = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.fill == colors.tint && rect.rect.contains(sent) =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .expect("the outgoing bubble owns the text");
            assert!(
                bubble.width() < 175.,
                "short bubble width was {}",
                bubble.width()
            );
            assert!(
                bubble.right() > width - 45. && bubble.right() <= width,
                "outgoing bubble must be right aligned: {bubble:?}"
            );
            assert!(
                received.x < 170.,
                "incoming message starts near the left edge"
            );
            assert!(
                text("Mesaj yaz…").y > height - 66.,
                "composer should be docked near the bottom"
            );
            let long = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.job.text == messages[2].text => {
                        Some(text.galley.rows.len())
                    }
                    _ => None,
                })
                .unwrap();
            assert!(long > 1);
            let draw = |ctx: &UiContext| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    app.conversation(ui, selection.clone(), link.clone(), room.clone())
                });
            };
            let mut draw = draw;
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(bounds),
                    events: vec![
                        egui::Event::PointerMoved(bubble.center()),
                        egui::Event::PointerButton {
                            pos: bubble.center(),
                            button: egui::PointerButton::Secondary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                        egui::Event::PointerButton {
                            pos: bubble.center(),
                            button: egui::PointerButton::Secondary,
                            pressed: false,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                &mut draw,
            );
            let menu = ctx.run(
                egui::RawInput {
                    screen_rect: Some(bounds),
                    ..Default::default()
                },
                draw,
            );
            assert!(menu.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.job.text == "Mesaj bilgisi")));
        }
        Ok(())
    }

    #[test]
    fn signed_in_screens_show_their_primary_controls_at_supported_widths() -> Result<()> {
        let cases = [
            (Page::Chats, "İlk sohbetini başlat", "Kişi ekle"),
            (Page::Archive, "Arşiv boş", "Sohbetlere dön"),
            (Page::Contacts, "Kişi kartım", "İçe aktar"),
            (Page::Host, "Ağ durumu", "Bu cihazı host yap"),
            (Page::Settings, "Sohbet arka planı", "Varsayılana dön"),
            (Page::Profile, "Profilim", "Adı kaydet"),
        ];
        for width in [720., 1080.] {
            for (page, title, action) in cases {
                let ctx = UiContext::default();
                visual::theme(&ctx, Role::Human);
                let mut app = App::new(Role::Human, PathBuf::new());
                app.identity = Some(Identity::generate(Role::Human, "deniz", "Deniz")?);
                app.view = Some(View {
                    local: Default::default(),
                    host: None,
                    notice: None,
                    copy: None,
                    codex: String::new(),
                });
                app.page = page;
                let bounds = egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, if width == 720. { 505. } else { 720. }),
                );
                let frame = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(bounds),
                        ..Default::default()
                    },
                    |ctx| {
                        app.navigation(ctx);
                        app.workspace(ctx);
                    },
                );
                for label in [title, action] {
                    assert!(frame.shapes.iter().any(|shape| {
                        let center = shape.shape.visual_bounding_rect().center();
                        matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == label)
                            && bounds.contains(center) && shape.clip_rect.contains(center)
                    }), "{page:?} {width}: {label} must be visible");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn authentication_actions_fit_supported_windows() {
        let cases = [
            (Role::Human, [720., 505.], false, "Giriş yap"),
            (Role::Human, [720., 505.], true, "Hesap oluştur"),
            (Role::Human, [1080., 720.], true, "Hesap oluştur"),
            (Role::Agent, [440., 460.], false, "Anahtarı aç"),
            (Role::Agent, [440., 460.], true, "Ajan oluştur"),
        ];
        for (role, [width, height], sign_up, expected) in cases {
            let ctx = UiContext::default();
            visual::theme(&ctx, role);
            let mut app = App::new(role, PathBuf::new());
            app.signing_up = sign_up;
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, height));
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(rect),
                    ..Default::default()
                },
                |ctx| app.login(ctx),
            );
            let actions = output
                .shapes
                .iter()
                .filter(|clipped| {
                    let egui::Shape::Text(text) = &clipped.shape else {
                        return false;
                    };
                    text.galley.job.text == expected
                        && clipped
                            .clip_rect
                            .contains(clipped.shape.visual_bounding_rect().center())
                        && rect.contains(clipped.shape.visual_bounding_rect().center())
                })
                .count();
            let expected_occurrences = if role == Role::Agent { 1 } else { 2 };
            assert!(
                actions >= expected_occurrences,
                "{role:?} {width}x{height}: {expected} must be visible"
            );
            if role == Role::Human {
                assert!(output.shapes.iter().any(|shape| {
                    matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == "Beni hatırla")
                        && shape.clip_rect.contains(shape.shape.visual_bounding_rect().center())
                }));
            }
        }
    }
}
