use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use eframe::egui::{
    self, Align, Color32, Context as UiContext, Frame, Layout, RichText, ScrollArea, Ui,
};
use zeroize::Zeroize;

#[path = "desktop/image_viewer.rs"]
mod image_viewer;
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
    agent::{FilePermit, Settings, TaskAccess},
    host::{mention_ranges, mentions},
    identity::{
        AgentProfile, Card, HANDLE_RULE, Identity, NAME_RULE, PASSWORD_MIN, Role, pack,
        password_rule, validate_handle, validate_name, validate_password,
    },
    protocol::{
        AvatarPhoto, Grant, Message, MessageMode, Receipt, Request, Room, Status, allowed_ip,
    },
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProfilePane {
    Name,
    Photo,
}

impl ProfilePane {
    fn title(self) -> &'static str {
        match self {
            Self::Name => "Görünen ad",
            Self::Photo => "Profil fotoğrafı",
        }
    }

    fn icon(self) -> Outline {
        match self {
            Self::Name => Outline::Edit,
            Self::Photo => Outline::People,
        }
    }
}

#[derive(Default)]
struct McpForm {
    name: String,
    remote: bool,
    url: String,
    command: String,
    args: String,
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
    draft_mode: Option<MessageMode>,
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
    profile_pane: Option<ProfilePane>,
    profile_focus_editor: bool,
    profile_return_focus: Option<egui::Id>,
    workspace_jobs_opener: Option<egui::Id>,
    contact_target: Option<crate::identity::Signed<crate::protocol::ContactRoute>>,
    pending_removal: Option<Selection>,
    pending_clear: Option<(Selection, String)>,
    device_preview: Option<crate::protocol::DeviceSummary>,
    attachment: Option<PathBuf>,
    viewer: image_viewer::Viewer,
    profile_card: Option<Card>,
    message_details: Option<(Selection, i64)>,
    reply: Option<(i64, String, String)>,
    jump: Option<i64>,
    read_sent: Option<(Selection, i64)>,
    invite: bool,
    confirm_close: bool,
    settings: Settings,
    settings_dirty: bool,
    mcp_form: McpForm,
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
            draft_mode: None,
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
            profile_pane: None,
            profile_focus_editor: false,
            profile_return_focus: None,
            workspace_jobs_opener: None,
            contact_target: None,
            pending_removal: None,
            pending_clear: None,
            device_preview: None,
            attachment: None,
            viewer: image_viewer::Viewer::default(),
            profile_card: None,
            message_details: None,
            reply: None,
            jump: None,
            read_sent: None,
            invite: false,
            confirm_close: false,
            settings: Settings::default(),
            settings_dirty: false,
            mcp_form: McpForm::default(),
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
        self.pending_clear = None;
        self.viewer.close();
        image_viewer::reset_cache_scope();
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
        let cleanup_error = if let Some(identity) = &self.identity
            && self.role == Role::Human
        {
            remember::forget_account(&self.profile_root(), &identity.card.data.handle)?
        } else {
            None
        };
        let mut login = Self::new(self.role, self.data.clone());
        #[cfg(windows)]
        {
            login.taskbar_icon = self.taskbar_icon.take();
        }
        if let Some(error) = cleanup_error {
            login.notice = format!(
                "Oturum kapatıldı. Anahtar kasası temizliği daha sonra yeniden denenecek. {error:#}"
            );
        }
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
                            && self.draft_mode == draft.mode
                        {
                            self.draft.clear();
                            self.draft_mode = None;
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
                            if cfg!(target_os = "linux") {
                                "Oturum anahtarı Linux anahtar kasasında (Secret Service) saklanır. Anahtar kasası kullanılamıyorsa oturum kaydedilmez."
                            } else {
                                "Bu cihazdaki sonraki açılışta hesabına otomatik giriş yap."
                            },
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
            #[cfg(any(windows, target_os = "linux"))]
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
                            #[cfg(any(windows, target_os = "linux"))]
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
            self.reply = draft.reply.filter(|(seq, _, _)| {
                self.view
                    .as_ref()
                    .and_then(|view| view.local.hosts.get(&selection.host))
                    .is_none_or(|link| link.is_visible(selection.room, *seq))
            });
            self.selected = Some(selection);
            self.draft = draft.text;
            self.draft_mode = draft.mode;
            self.attachment = draft.attachment;
            self.message_details = None;
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
            mode: self.draft_mode.clone(),
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
                if page == Page::Profile {
                    self.profile_side_panel(ctx);
                }
                if page == Page::Agent {
                    self.workspace_jobs_panel(ctx);
                }
                egui::CentralPanel::default()
                    .frame(
                        Frame::new()
                            .inner_margin(22)
                            .fill(visual::palette(self.role).base),
                    )
                    .show(ctx, |ui| {
                        let width = ui.available_width().min(match page {
                            Page::Profile => 420.,
                            Page::Host => 690.,
                            _ => 910.,
                        });
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
        #[cfg(any(windows, target_os = "linux"))]
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
                #[cfg(any(windows, target_os = "linux"))]
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
        visual::page_heading(ui, Outline::People, "Profilim", "");
        if self.profile_contents(ui, &identity.card) {
            self.toggle_profile_pane(ui.ctx(), ProfilePane::Photo, None);
        }
        let name = self
            .view
            .as_ref()
            .and_then(|view| view.local.display_name.clone())
            .unwrap_or_else(|| identity.card.data.name.clone());
        ui.add_space(10.);
        visual::profile_section(ui, Outline::People, "Kimlik", |ui| {
            let edit = visual::profile_action(
                ui,
                Outline::Edit,
                "Görünen adı düzenle",
                &name,
                self.profile_pane == Some(ProfilePane::Name),
            );
            if edit.clicked() {
                self.profile_edit.clone_from(&name);
                self.toggle_profile_pane(ui.ctx(), ProfilePane::Name, Some(edit.id));
            }
            if visual::profile_action(
                ui,
                Outline::Copy,
                "Kullanıcı adımı kopyala",
                &format!("@{}", identity.card.data.handle),
                false,
            )
            .clicked()
            {
                ui.ctx()
                    .copy_text(format!("@{}", identity.card.data.handle));
                self.notice = "Kullanıcı adı kopyalandı.".into();
            }
        });
        ui.add_space(10.);
        visual::profile_section(ui, Outline::Link, "Paylaşım", |ui| {
            if visual::profile_action(
                ui,
                Outline::Link,
                "Bağlantımı paylaş",
                "Arkadaşlık istekleri için",
                false,
            )
            .clicked()
            {
                self.command(Command::ShareContact);
            }
            if visual::profile_action(
                ui,
                Outline::Copy,
                "Kişi kartını kopyala",
                "İmzalı ChatOid kartı",
                false,
            )
            .clicked()
                && let Ok(code) = pack("card", &identity.card)
            {
                ui.ctx().copy_text(code);
                self.notice = "Kişi kartı kopyalandı.".into();
            }
        });
    }

    fn toggle_profile_pane(
        &mut self,
        ctx: &UiContext,
        pane: ProfilePane,
        return_focus: Option<egui::Id>,
    ) {
        if self.profile_pane == Some(pane) {
            self.close_profile_pane(ctx);
            return;
        }
        self.profile_pane = Some(pane);
        self.profile_return_focus = return_focus;
        self.profile_focus_editor = pane == ProfilePane::Name;
        ctx.request_repaint();
    }

    fn close_profile_pane(&mut self, ctx: &UiContext) {
        self.profile_pane = None;
        self.profile_focus_editor = false;
        if let Some(id) = self.profile_return_focus.take() {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
        ctx.request_repaint();
    }

    fn profile_side_panel(&mut self, ctx: &UiContext) {
        let Some(pane) = self.profile_pane else {
            return;
        };
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.close_profile_pane(ctx);
            return;
        }
        if right_drawer(
            ctx,
            self.role,
            "profile-editor-dock",
            pane.icon(),
            pane.title(),
            |ui| {
                self.profile_pane_contents(ui, pane);
            },
        ) && self.profile_pane.is_some()
        {
            self.close_profile_pane(ctx);
        }
    }

    fn profile_pane_contents(&mut self, ui: &mut Ui, pane: ProfilePane) {
        match pane {
            ProfilePane::Name => {
                let Some(identity) = self.identity.as_ref() else {
                    return;
                };
                let name = self
                    .view
                    .as_ref()
                    .and_then(|view| view.local.display_name.clone())
                    .unwrap_or_else(|| identity.card.data.name.clone());
                self.edit_profile_name(ui, &name);
            }
            ProfilePane::Photo => {
                #[cfg(any(windows, target_os = "linux"))]
                if visual::profile_action(ui, Outline::Plus, "Fotoğraf seç", "JPEG veya PNG", false)
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("JPEG / PNG", &["jpg", "jpeg", "png"])
                        .pick_file()
                {
                    self.command(Command::SetAvatar(Some(path)));
                    self.close_profile_pane(ui.ctx());
                }
                if self
                    .view
                    .as_ref()
                    .and_then(|view| view.local.avatar.as_ref())
                    .is_some_and(|avatar| avatar.data.is_some())
                    && visual::profile_action(
                        ui,
                        Outline::Cross,
                        "Fotoğrafı kaldır",
                        "Baş harflerimi göster",
                        false,
                    )
                    .clicked()
                {
                    self.command(Command::SetAvatar(None));
                    self.close_profile_pane(ui.ctx());
                }
            }
        }
    }

    fn edit_profile_name(&mut self, ui: &mut Ui, current: &str) {
        let edit_id = egui::Id::new("profile-display-name");
        if std::mem::take(&mut self.profile_focus_editor) {
            ui.memory_mut(|memory| memory.request_focus(edit_id));
        }
        let focused = ui.memory(|memory| memory.has_focus(edit_id));
        let enter = focused && ui.input(|input| input.key_pressed(egui::Key::Enter));
        let field = ui.add_sized(
            [ui.available_width(), 36.],
            egui::TextEdit::singleline(&mut self.profile_edit)
                .id(edit_id)
                .hint_text("Görünen ad")
                .vertical_align(Align::Center)
                .margin(egui::Margin::symmetric(10, 5)),
        );
        let candidate = self.profile_edit.trim();
        let valid = validate_name(candidate).is_ok();
        visual::field_rules(
            ui,
            &field,
            !self.profile_edit.is_empty() && !valid,
            NAME_RULE,
        );
        let changed = valid && candidate != current;
        let (save, cancel) = ui
            .horizontal_wrapped(|ui| {
                (
                    ui.add_enabled(changed, visual::primary(ui, "Adı kaydet"))
                        .clicked(),
                    ui.button("Vazgeç").clicked(),
                )
            })
            .inner;
        if (save || enter) && changed {
            self.command(Command::SetDisplayName(candidate.to_owned()));
        }
        if ((save || enter) && changed) || cancel {
            self.close_profile_pane(ui.ctx());
        }
    }

    fn open_profile(&mut self, card: Card) {
        self.command(Command::LookupProfile(card.data.key.clone()));
        self.profile_card = Some(card);
    }

    fn profile_contents(&self, ui: &mut Ui, card: &Card) -> bool {
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
        let mut photo_clicked = false;
        visual::profile_hero(ui, |ui| {
            if own {
                ui.vertical_centered(|ui| {
                    photo_clicked = visual::profile_avatar(ui, card, photo, 79.)
                        .on_hover_text("Fotoğraf seçeneklerini aç")
                        .clicked();
                    ui.add_space(3.);
                    ui.add(egui::Label::new(RichText::new(name).size(25.).strong()).truncate())
                        .on_hover_text(name);
                    ui.weak(format!(
                        "@{} · {}",
                        card.data.handle,
                        if agent { "Ajan" } else { "İnsan" }
                    ));
                    photo_clicked |= ui
                        .link(if photo.is_some() {
                            "Fotoğrafı değiştir"
                        } else {
                            "Fotoğraf ekle"
                        })
                        .clicked();
                });
            } else {
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
            }
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
        photo_clicked
    }

    fn inbox(&mut self, ctx: &UiContext) {
        let colors = visual::palette(self.role);
        let archive = self.page == Page::Archive;
        let rows = self
            .view
            .as_ref()
            .map(|view| {
                let mut rows: Vec<_> = view
                    .local
                    .hosts
                    .iter()
                    .flat_map(|(pin, host)| {
                        host.rooms
                            .iter()
                            .filter(|room| host.archived.contains(&room.id) == archive)
                            .map(|room| {
                                let unread = host.unread(room.id, self.id());
                                let targeted =
                                    host.unread_messages(room.id, self.id()).any(|message| {
                                        message
                                            .reply
                                            .as_ref()
                                            .is_some_and(|q| q.author.data.key == self.id())
                                            || self.identity.as_ref().is_some_and(|i| {
                                                mentions(&message.text, &i.card.data.handle)
                                            })
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
                        let has_history = last.is_some();
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
                                .add_enabled(has_history, egui::Button::new("Sohbeti temizle"))
                                .clicked()
                            {
                                self.pending_clear = Some((selection.clone(), name.clone()));
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
        let messages: Vec<_> = link.visible_messages(room.id).collect();
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
                        && first.seq > link.cleared_through(room.id) + 1
                        && ui.button("Önceki mesajlar").clicked()
                    {
                        self.command(Command::History(s.clone(), first.seq));
                        ui.close();
                    }
                    if ui
                        .add_enabled(!messages.is_empty(), egui::Button::new("Sohbeti temizle"))
                        .clicked()
                    {
                        self.pending_clear = Some((s.clone(), room.name.clone()));
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
                if messages.is_empty()
                    && link.cleared_through(room.id) > 0
                    && !link.queue.iter().any(|item| item.draft.room == room.id)
                {
                    ui.vertical_centered(|ui| {
                        ui.add_space(16.);
                        ui.weak("Sohbet temizlendi. Yeni mesajlar burada görünecek.");
                    });
                }
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
                                self.message(ui, message, &s, &room, first, &link);
                            });
                        } else {
                            self.message(ui, message, &s, &room, false, &link);
                        }
                        if link.online
                            && room.active
                            && !room.closed
                            && message.author.data.key == self.id()
                            && let Some(activity) = link.task_activity.iter().find(|activity| {
                                activity.room == room.id && activity.source == message.seq
                            })
                        {
                            let status = match activity.phase {
                                crate::protocol::TaskPhase::Pending => "Ajan onayı bekleniyor",
                                crate::protocol::TaskPhase::Running => "Ajan çalışıyor",
                                crate::protocol::TaskPhase::WaitingApproval => {
                                    "Ajan yerel eylem onayı bekliyor"
                                }
                                crate::protocol::TaskPhase::WaitingAnswer => {
                                    "Ajan yanıtını bekliyor"
                                }
                                crate::protocol::TaskPhase::Stopping => "Görev durduruluyor",
                                crate::protocol::TaskPhase::Complete => "Görev tamamlandı",
                                crate::protocol::TaskPhase::Rejected => "Görev reddedildi",
                                crate::protocol::TaskPhase::Cancelled => "Görev durduruldu",
                                crate::protocol::TaskPhase::Failed => "Görev tamamlanamadı",
                            };
                            let text = activity.plan.map_or_else(
                                || status.to_owned(),
                                |[done, total]| format!("{status} · {done}/{total} adım"),
                            );
                            ui.with_layout(Layout::top_down(Align::Max), |ui| {
                                ui.label(RichText::new(text).size(12.).color(MUTED));
                            });
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
                    let (pending_response, _) = visual::bubble_lane(ui, true, &size, false, |ui| {
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
                    pending_response.context_menu(|ui| {
                        if ui.button("Sil · gönderimi geri al").clicked() {
                            self.command(Command::CancelSend(s.clone(), item.draft.id));
                            ui.close();
                        }
                    });
                }
            });
    }

    fn restore_failed(&mut self, link: &Link, item: &crate::session::Pending) {
        self.draft = item.draft.text.clone();
        self.attachment = item.binary.clone();
        self.draft_mode = item
            .draft
            .mode
            .clone()
            .filter(|mode| mode != &MessageMode::Plain);
        self.reply = item.draft.reply.and_then(|id| {
            if !link.is_visible(item.draft.room, id) {
                return None;
            }
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
                }
                #[cfg(target_os = "linux")]
                if limit > 0 {
                    let focused = ui.memory(|memory| memory.has_focus(draft_id));
                    let paste = focused
                        && ui.input(|input| {
                            input.key_pressed(egui::Key::V)
                                && input.modifiers.command
                                && !input.events.iter().any(|event| {
                                    matches!(event, egui::Event::Paste(text) if !text.is_empty())
                                })
                        });
                    if paste && let Some(root) = self.preferences_root() {
                        match preferences::clipboard_image(&root, limit as u64) {
                            Ok(Some(path)) => self.attachment = Some(path),
                            Ok(None) => {}
                            Err(error) => self.notice = format!("Pano resmi eklenemedi. {error:#}"),
                        }
                    }
                }
                if limit > 0
                    && let Some(path) = ui.input(|input| {
                        input
                            .raw
                            .dropped_files
                            .iter()
                            .find_map(|file| file.path.clone())
                    })
                {
                    self.stage_attachment(&path, limit as u64);
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
                let focused = ui.memory(|memory| memory.has_focus(draft_id));
                if focused {
                    ui.input_mut(|input| {
                        input.events.retain(|event| !matches!(event, egui::Event::Key {
                            key: egui::Key::Enter, repeat: true, modifiers, ..
                        } if !modifiers.shift));
                    });
                }
                let cursor_char = egui::TextEdit::load_state(ui.ctx(), draft_id)
                    .and_then(|state| state.cursor.char_range())
                    .map(|range| range.primary.index)
                    .unwrap_or_else(|| self.draft.chars().count());
                let slash_token = slash_command_token(&self.draft, cursor_char);
                let suggestions = slash_token
                    .as_ref()
                    .map(|(_, _, query)| slash_command_suggestions(query))
                    .unwrap_or_default();
                let selection_id = draft_id.with("slash-selection");
                let dismissed_id = draft_id.with("slash-dismissed");
                let dismissed = ui.ctx().data(|data| {
                    data.get_temp::<String>(dismissed_id)
                        .is_some_and(|draft| draft == self.draft)
                });
                let suggestions_open = focused && !suggestions.is_empty() && !dismissed;
                let mut selected = ui
                    .ctx()
                    .data(|data| data.get_temp::<usize>(selection_id).unwrap_or(0))
                    .min(suggestions.len().saturating_sub(1));
                let mut completion_enter = false;
                let management = is_automation_management(&self.draft);
                let activation = (!management).then(|| leading_slash_mode(&self.draft).or_else(|| {
                    slash_token.as_ref().and_then(|(start, end, query)| {
                        slash_mode(query).or_else(|| {
                            suggestions_open.then(|| suggestions[selected].command).and_then(slash_mode)
                        }).map(|mode| (*start, *end, mode))
                    })
                })).flatten();
                let mode_enter=focused && activation.is_some() && !ui.input(|input|input.modifiers.shift)
                    && ui.input_mut(|input|input.consume_key(egui::Modifiers::NONE,egui::Key::Enter));
                if mode_enter && let Some((start,end,mode))=activation {
                    let (draft,caret)=consume_mode_slash_token(&self.draft,start,end);
                    self.draft=draft;self.draft_mode=Some(mode);set_composer_cursor(ui.ctx(),draft_id,caret);
                }
                if suggestions_open {
                    if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                        selected = (selected + 1) % suggestions.len();
                    }
                    if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                        selected = selected.checked_sub(1).unwrap_or(suggestions.len() - 1);
                    }
                    completion_enter = !management && !mode_enter && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });
                    if completion_enter {
                        if let Some((start, end, _)) = slash_token {
                            let (draft, caret) = replace_slash_token(
                                &self.draft,
                                start,
                                end,
                                suggestions[selected].command,
                            );
                            self.draft = draft;
                            set_composer_cursor(ui.ctx(), draft_id, caret);
                        }
                        ui.memory_mut(|memory| memory.request_focus(draft_id));
                    } else if ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                    }) {
                        ui.ctx().data_mut(|data| data.insert_temp(dismissed_id, self.draft.clone()));
                    }
                }
                ui.ctx().data_mut(|data| data.insert_temp(selection_id, selected));
                let enter = !mode_enter && !completion_enter && ui.memory(|memory| memory.has_focus(draft_id))
                    && ui.input_mut(|input| {
                        !input.modifiers.shift
                            && input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });
                let (field, button) = ui
                    .horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.;
                        visual::composer_popup(ui, Outline::Attachment, "Dosya ekle", |ui| {
                            #[cfg(any(windows, target_os = "linux"))]
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
                                #[cfg(any(windows, target_os = "linux"))]
                                ui.weak(format!("Host sınırı · {} MiB", limit / 1048576));
                                #[cfg(not(any(windows, target_os = "linux")))]
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
                        ui.menu_button("Komutlar", |ui| {
                            ui.set_min_width(210.);
                            for command in ["/loop", "/goal"] {
                                if ui.button(format!("{command} · komutu ekle")).clicked() {
                                    let (draft, caret) = slash_token
                                        .as_ref()
                                        .map(|(start, end, _)| {
                                            replace_slash_token(&self.draft, *start, *end, command)
                                        })
                                        .unwrap_or_else(|| {
                                            let mut draft = self.draft.clone();
                                            if !draft.is_empty() && !draft.ends_with(char::is_whitespace) {
                                                draft.push(' ');
                                            }
                                            draft.push_str(command);
                                            let caret = draft.chars().count();
                                            (draft, caret)
                                        });
                                    self.draft = draft;
                                    let caret=caret.saturating_sub(usize::from(self.draft.chars().nth(caret.saturating_sub(1))==Some(' ')));
                                    set_composer_cursor(ui.ctx(), draft_id, caret);
                                    ui.close();
                                }
                            }
                        });
                        let width = (ui.available_width() - 42.).max(40.);
                        let field = ui
                            .allocate_ui_with_layout(
                                egui::vec2(width, 36.),
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
                                                    .hint_text(RichText::new("Mesaj yaz…").color(MUTED))
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
                if completion_enter || mode_enter {
                    field.request_focus();
                }
                let mut clicked_suggestion = false;
                if suggestions_open && !completion_enter {
                    egui::Area::new(draft_id.with("slash-popup"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(egui::pos2(field.rect.left(), field.rect.top() - 108.))
                        .show(ui.ctx(), |ui| {
                            Frame::popup(ui.style())
                                .fill(colors.panel)
                                .stroke(egui::Stroke::new(1_f32, colors.border))
                                .corner_radius(10)
                                .inner_margin(5.)
                                .show(ui, |ui| {
                                    ui.set_width(280.);
                                    for (index, suggestion) in suggestions.iter().enumerate() {
                                        if visual::slash_command_suggestion(
                                            ui,
                                            suggestion.icon,
                                            suggestion.command,
                                            suggestion.description,
                                            index == selected,
                                        ).clicked() {
                                            if let Some((start, end, _)) = slash_token {
                                                let (draft, caret) = replace_slash_token(
                                                    &self.draft,
                                                    start,
                                                    end,
                                                    suggestion.command,
                                                );
                                                self.draft = draft;
                                                set_composer_cursor(ui.ctx(), draft_id, caret);
                                            }
                                            clicked_suggestion = true;
                                        }
                                    }
                                });
                        });
                }
                if clicked_suggestion {
                    field.request_focus();
                }
                ui.horizontal_wrapped(|ui| {
                    if let Some(mode) = self.draft_mode.clone() {
                        let (icon, label) = match mode {
                            MessageMode::Goal => (Outline::Goal, "Hedef"),
                            MessageMode::Loop { .. } => (Outline::Loop, "Döngü"),
                            MessageMode::Plain => (Outline::Chat, "Düz metin"),
                        };
                        visual::label_icon(ui, icon, RichText::new(label).color(colors.accent));
                        if let MessageMode::Loop { interval_seconds } = mode {
                            egui::ComboBox::from_id_salt(draft_id.with("loop-interval"))
                                .selected_text(loop_interval_label(interval_seconds))
                                .show_ui(ui, |ui| {
                                    for (seconds, option) in [
                                        (10, "10 sn"),
                                        (30 * 60, "30 dk"),
                                        (60 * 60, "1 saat"),
                                        (24 * 60 * 60, "1 gün"),
                                    ] {
                                        if ui.selectable_label(interval_seconds == seconds, option).clicked() {
                                            self.draft_mode = Some(MessageMode::Loop { interval_seconds: seconds });
                                            ui.close();
                                        }
                                    }
                                });
                        }
                        if visual::composer_button(ui, Outline::Cross, "Modu kaldır", false).clicked() {
                            self.draft_mode = None;
                        }
                    }
                });
                if (enter || button) && (!self.draft.trim().is_empty() || self.attachment.is_some())
                {
                    let draft = self.current_draft();
                    if self.submitted.as_ref().is_none_or(|(_, _, previous)| previous != &draft) {
                        let id = uuid::Uuid::new_v4();
                        let wire_mode = draft.mode.clone().or_else(|| {
                            (crate::session::automation::contains_command(&draft.text) && !management).then_some(MessageMode::Plain)
                        });
                        self.command(Command::SendModeDraft(selection.clone(), id, draft.text.clone(),
                            draft.reply.as_ref().map(|quote| quote.0), draft.attachment.clone(), wire_mode));
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
        link: &Link,
    ) {
        if message.withdrawn {
            ui.label(RichText::new("Bu mesaj geri alındı").italics().color(MUTED));
            return;
        }
        let colors = visual::palette(self.role);
        let mine = message.author.data.key == self.id();
        let author = show_author.then_some(message.author.data.name.as_str());
        let stamp = time(message.sent_at);
        let status = mine.then(|| delivery_status(message, &link.receipts));
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
            .filter(|quote| link.is_visible(room.id, quote.id))
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
                    let mut open = false;
                    if let Some(preview) = &file.preview {
                        open |=
                            visual::attachment_preview(ui, preview, (&selection.host, message.seq))
                                .is_some_and(|response| response.clicked());
                    }
                    let is_image = file.preview.is_some()
                        || [".jpg", ".jpeg", ".png"]
                            .iter()
                            .any(|suffix| file.name.to_lowercase().ends_with(suffix));
                    let label = format!("{} · {}", short(&file.name, 26), file_size(file.bytes));
                    if ui
                        .button(label)
                        .on_hover_text(if is_image {
                            "Fotoğrafı aç"
                        } else {
                            "Dosyayı kaydet"
                        })
                        .clicked()
                    {
                        if is_image {
                            open = true;
                        } else {
                            #[cfg(any(windows, target_os = "linux"))]
                            if let Some(path) =
                                rfd::FileDialog::new().set_file_name(&file.name).save_file()
                            {
                                self.command(Command::Download(
                                    selection.clone(),
                                    message.seq,
                                    path,
                                ));
                            }
                        }
                    }
                    if open && let Some(root) = self.preferences_root() {
                        let path = image_viewer::cache_path(&root, &selection.host, message.seq);
                        let prepared = path.parent().map(std::fs::create_dir_all).transpose();
                        match prepared {
                            Ok(_) => {
                                if !path.is_file() {
                                    self.command(Command::Download(
                                        selection.clone(),
                                        message.seq,
                                        path.clone(),
                                    ));
                                }
                                self.viewer.open(file.name.clone(), path);
                            }
                            Err(error) => self.notice = format!("Fotoğraf açılamadı: {error}"),
                        }
                    }
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
                if message.author.data.role == Role::Agent {
                    for url in message_web_links(&message.text) {
                        let label = format!(
                            "{}{}",
                            url.host_str().unwrap_or_default(),
                            url.path().trim_end_matches('/')
                        );
                        ui.hyperlink_to(short(&label, 55), url.as_str())
                            .on_hover_text(url.as_str());
                    }
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
                if mine && message.undo_until_ms > 0 {
                    let allowed = crate::protocol::now_ms() <= message.undo_until_ms;
                    if ui
                        .add_enabled(allowed, egui::Button::new("Sil · geri al"))
                        .on_hover_text("İlk 0,5 saniye içinde herkes için geri al")
                        .clicked()
                    {
                        self.command(Command::WithdrawMessage(selection.clone(), message.seq));
                        ui.close();
                    }
                }
                let goal_start = mine
                    && !room.group
                    && room.active
                    && !room.closed
                    && room
                        .members
                        .iter()
                        .any(|member| member.data.role == Role::Agent)
                    && (message.mode == Some(MessageMode::Goal)
                        || message
                            .text
                            .trim()
                            .split_once(char::is_whitespace)
                            .is_some_and(|(verb, remainder)| {
                                verb == "/goal" && !remainder.trim().is_empty()
                            }));
                let workspace_start = goal_start
                    || (mine
                        && !room.group
                        && room.active
                        && !room.closed
                        && room
                            .members
                            .iter()
                            .any(|member| member.data.role == Role::Agent)
                        && self
                            .view
                            .as_ref()
                            .and_then(|view| view.local.hosts.get(&selection.host))
                            .is_some_and(|link| {
                                link.task_activity.iter().any(|activity| {
                                    activity.room == message.room && activity.source == message.seq
                                })
                            }));
                if workspace_start && ui.button("Görevi yönlendir").clicked() {
                    self.reply = Some((
                        message.seq,
                        message.author.data.name.clone(),
                        short(&message.text, 100),
                    ));
                    let id = egui::Id::new(("conversation-composer", selection.room)).with("input");
                    self.prefill_workspace_command(ui.ctx(), id, "/yönlendir ", "/yonlendir ");
                    ui.close();
                }
                if workspace_start {
                    let commands = if goal_start {
                        [
                            ("Görev durumu", "/goal status"),
                            ("Görevi durdur", "/goal stop"),
                        ]
                    } else {
                        [("Görev durumu", "/durum"), ("Görevi durdur", "/durdur")]
                    };
                    for (label, verb) in commands {
                        if ui.button(label).clicked() {
                            self.command(Command::SendDraft(
                                selection.clone(),
                                uuid::Uuid::new_v4(),
                                verb.into(),
                                Some(message.seq),
                                None,
                            ));
                            ui.close();
                        }
                    }
                }
                if ui.button("Yanıtla").clicked() {
                    self.reply = Some((
                        message.seq,
                        message.author.data.name.clone(),
                        short(&message.text, 100),
                    ));
                    ui.close();
                }
                #[cfg(any(windows, target_os = "linux"))]
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

    fn prefill_workspace_command(
        &mut self,
        ctx: &UiContext,
        id: egui::Id,
        command: &str,
        alias: &str,
    ) {
        if !self.draft.starts_with(command) && !self.draft.starts_with(alias) {
            self.draft.insert_str(0, command);
        }
        let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(self.draft.chars().count()),
            )));
        state.store(ctx, id);
        ctx.memory_mut(|memory| memory.request_focus(id));
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
        if let Some((selection, name)) = self.pending_clear.clone() {
            let modal = egui::Modal::new(egui::Id::new("clear-chat")).show(ctx, |ui| {
                ui.set_max_width(370.);
                ui.heading("Sohbeti temizle");
                ui.label(format!(
                    "{} sohbetindeki mevcut mesajlar yalnızca bu hesabın görünümünden kalkacak.",
                    name
                ));
                ui.add_space(8.);
                ui.horizontal(|ui| {
                    if ui.button("Vazgeç").clicked() {
                        self.pending_clear = None;
                    }
                    if ui.add(visual::primary(ui, "Temizle")).clicked() {
                        if self.selected.as_ref() == Some(&selection) {
                            self.message_details = None;
                            self.reply = None;
                            self.jump = None;
                            self.viewer.close();
                            self.save_preferences();
                        }
                        self.command(Command::ClearChat(selection));
                        self.pending_clear = None;
                    }
                });
            });
            if modal.should_close() {
                self.pending_clear = None;
            }
        }
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
        if local.operator.is_none() || !self.settings.workspace_jobs {
            visual::profile_section(ui, Outline::Agent, "Codex görevlerini hazırla", |ui| {
                ui.label("Operatör eşleştikten sonra bire bir mesajlar yerel Codex'te çalışır. İsteğe bağlı olarak ayrı bir klasör ve her istek için onay ayarlayabilirsin.");
                visual::status(
                    ui,
                    if local.operator.is_some() {
                        "Operatör eşleşti"
                    } else {
                        "Operatör seçilmedi"
                    },
                    local.operator.is_some(),
                );
                if local.operator.is_none() {
                    let me = self.id().to_owned();
                    for card in local.contacts.values().filter(|card| {
                        card.data.role == Role::Human && local.friend(&me, &card.data.key).is_some()
                    }) {
                        if ui
                            .button(format!("{} · Operatör olarak eşleştir", card.data.name))
                            .clicked()
                            && let Ok(code) = pack("card", card)
                        {
                            self.command(Command::Pair(code));
                        }
                    }
                }
                visual::status(
                    ui,
                    if self.settings.workspace.is_empty() {
                        "Varsayılan özel çalışma alanı kullanılacak"
                    } else {
                        "Çalışma klasörü seçildi"
                    },
                    true,
                );
                #[cfg(any(windows, target_os = "linux"))]
                if ui.button("Çalışma klasörü seç").clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_folder()
                {
                    self.settings.workspace = path.to_string_lossy().into_owned();
                    self.settings_dirty = true;
                }
                if ui.button("İsteğe bağlı ayrı görev hesabı ekle").clicked() {
                    self.command(Command::SignInTaskCodex(self.settings.codex.clone()));
                }
                if ui
                    .add_enabled(
                        local.operator.is_some(),
                        egui::Button::new("Görev ayarlarını kaydet ve etkinleştir"),
                    )
                    .clicked()
                {
                    self.settings.workspace_jobs = true;
                    self.command(Command::Settings(self.settings.clone()));
                    self.settings_dirty = false;
                }
                ui.weak("Eşleşmiş operatörün bire bir istekleri Codex'te çalışır. Ayrı onay seçeneği ve çalışma klasörü aşağıdan ayarlanabilir.");
            });
            ui.add_space(12.);
        }
        ui.horizontal_wrapped(|ui| {
            visual::label_icon(ui, Outline::Agent, RichText::new("Ajan kimliği").heading());
            let pending = local
                .jobs
                .iter()
                .filter(|job| matches!(job.status, crate::session::WorkspaceJobStatus::Pending))
                .count();
            let label = if let Some(approval) = &local.task_approval {
                if approval.file || approval.title == "Masaüstü metin dosyası" {
                    "Dosya onayı bekliyor"
                } else {
                    "Pencere onayı bekliyor"
                }
                .into()
            } else if pending > 0 {
                format!("Codex görevleri ({pending})")
            } else {
                "Codex görevleri".into()
            };
            let open = self.workspace_jobs_opener.is_some();
            let button = visual::work_tab(ui, Outline::Chat, &label, open);
            if button.clicked() {
                self.workspace_jobs_opener = (!open).then_some(button.id);
                ui.ctx().request_repaint();
            }
        });
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
        if visual::work_tab(
            ui,
            Outline::Lock,
            "İsteğe bağlı ayrı görev hesabı ekle",
            false,
        )
        .clicked()
        {
            self.command(Command::SignInTaskCodex(self.settings.codex.clone()));
        }
        self.settings_dirty |= field(
            ui,
            "Çalışma klasörü yolu",
            &mut self.settings.workspace,
            "Klasörün tam yolu",
        );
        #[cfg(any(windows, target_os = "linux"))]
        if visual::work_tab(ui, Outline::Archive, "Çalışma klasörü seç", false).clicked()
            && let Some(path) = rfd::FileDialog::new().pick_folder()
        {
            self.settings.workspace = path.to_string_lossy().into_owned();
            self.settings_dirty = true;
        }
        self.settings_dirty |= ui
            .checkbox(
                &mut self.settings.workspace_jobs,
                "Onaylı Codex çalışma görevlerine izin ver",
            )
            .on_hover_text("Eşleşmiş operatörün bire bir istekleri yerel Codex'te çalışır.")
            .changed();
        self.settings_dirty |= ui.checkbox(
            &mut self.settings.manual_task_approval,
            "Her istek için ayrıca çalıştırma onayı iste",
        ).on_hover_text("Kapalıyken eşleşmiş operatörün normal istekleri otomatik başlar. Açıkken her istek için yerel onay gerekir.").changed();
        let access = self.settings.task_access;
        task_access_picker(ui, &mut self.settings.task_access);
        self.settings_dirty |= access != self.settings.task_access;
        if self.settings.task_access == TaskAccess::Full {
            ui.weak("Tam erişimde her görev ajan cihazında çalıştırma onayı bekler.");
        }
        ui.push_id("agent-default-task-model", |ui| {
            let previous = self.settings.task_model.clone();
            task_model_picker(
                ui,
                &local.task_models,
                "Codex varsayılanı",
                &mut self.settings.task_model,
            );
            self.settings_dirty |= previous != self.settings.task_model;
            if ui
                .button(if local.task_models.is_empty() {
                    "Codex modellerini yükle"
                } else {
                    "Model listesini yenile"
                })
                .clicked()
            {
                self.command(Command::LoadTaskModels);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Görev süre sınırı");
            self.settings_dirty |= ui
                .add(
                    egui::DragValue::new(&mut self.settings.task_timeout_minutes)
                        .range(1..=1440)
                        .suffix(" dakika"),
                )
                .on_hover_text(
                    "Yeni başlatılan görevlere uygulanır. Durdur düğmesi her zaman kullanılabilir.",
                )
                .changed();
        });
        ui.weak("Grup incelemelerinin sonucu gruptaki üyelerle paylaşılır.");
        ui.collapsing("Ek skill klasörleri", |ui| {
            ui.weak("Kurulu Codex becerileri otomatik bulunur. Başka bir klasör ekleyebilir, görev panelinden listeyi yenileyebilirsin.");
            let mut remove = None;
            for (index, path) in self.settings.skill_roots.iter().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(path.display().to_string());
                    if ui.small_button("Kaldır").clicked() { remove = Some(index); }
                });
            }
            if let Some(index) = remove {
                self.settings.skill_roots.remove(index);
                self.settings_dirty = true;
            }
            #[cfg(any(windows, target_os = "linux"))]
            if ui.add_enabled(self.settings.skill_roots.len() < 8, egui::Button::new("Skill klasörü ekle")).clicked()
                && let Some(path) = rfd::FileDialog::new().set_title("Skill klasörünü veya skill'leri içeren klasörü seç").pick_folder()
                && !self.settings.skill_roots.contains(&path)
            {
                self.settings.skill_roots.push(path);
                self.settings_dirty = true;
            }
        });
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

    fn workspace_jobs_panel(&mut self, ctx: &UiContext) {
        let Some(opener) = self.workspace_jobs_opener else {
            return;
        };
        let escape =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if escape
            || right_drawer(
                ctx,
                self.role,
                "workspace-jobs-dock",
                Outline::Agent,
                "Codex görevleri",
                |ui| {
                    let local = self
                        .view
                        .as_ref()
                        .map(|view| view.local.clone())
                        .unwrap_or_default();
                    self.workspace_jobs(ui, &local);
                },
            )
        {
            self.workspace_jobs_opener = None;
            ctx.memory_mut(|memory| memory.request_focus(opener));
            ctx.request_repaint();
        }
    }

    fn workspace_jobs(&mut self, ui: &mut Ui, local: &crate::session::Local) {
        use crate::session::{WorkspaceDecision, WorkspaceJobStatus};
        if !local.settings.workspace_jobs && local.jobs.is_empty() {
            ui.weak("Ajan ayarlarından Codex görevlerini etkinleştir.");
            return;
        }
        if local.jobs.is_empty() {
            ui.weak("Eşleşmiş operatör bire bir isteklerini gönderebilir; /goal kalıcı hedefleri izler.");
        }
        egui::CollapsingHeader::new("MCP bağlantıları").show(ui, |ui| {
            if ui
                .button("Bağlantıları ve araçları listele")
                .on_hover_text("Ajanın Codex görev profiline tanımlı MCP sunucularına bağlanır.")
                .clicked()
            {
                self.command(Command::LoadTaskMcp);
            }
            ui.weak("Bu liste ajan profilindeki Codex bağlantılarını gösterir.");
            self.mcp_setup(ui);
            if let Some(connections) = &local.task_mcp {
                if connections.is_empty() {
                    ui.weak("Bu görev profilinde MCP bağlantısı bulunamadı.");
                }
                for connection in connections {
                    egui::CollapsingHeader::new(format!(
                        "{} · {} araç",
                        connection.name,
                        connection.tools.len()
                    ))
                    .id_salt(&connection.name)
                    .show(ui, |ui| {
                        ui.weak(format!("Kimlik doğrulama · {}", connection.auth_status));
                        if connection.auth_status != "unsupported"
                            && ui.button("Tarayıcıda giriş yap").clicked()
                        {
                            self.command(Command::SignInMcp(connection.name.clone()));
                        }
                        for tool in connection.tools.values() {
                            ui.label(&tool.name)
                                .on_hover_text(tool.description.as_deref().unwrap_or_default());
                        }
                    });
                }
            }
        });
        if local
            .jobs
            .iter()
            .any(|job| job.status == WorkspaceJobStatus::Pending)
            && ui
                .button(if local.task_models.is_empty() {
                    "Codex modellerini yükle"
                } else {
                    "Codex modellerini yenile"
                })
                .clicked()
        {
            self.command(Command::LoadTaskModels);
        }
        if let Some(approval) = &local.task_approval {
            let (icon, title, action) = if approval.file {
                (Outline::Attachment, "Dosya paylaşımı", "Dosyayı onayla")
            } else if approval.title == "Masaüstü metin dosyası" {
                (Outline::Attachment, "Masaüstü dosyası", "Dosyayı onayla")
            } else if approval.action.starts_with("MCP araç çağrısı") {
                (Outline::Agent, "MCP araç çağrısı", "Çağrıyı onayla")
            } else if approval
                .action
                .to_lowercase()
                .contains("masaüstü görüntüsünü")
            {
                (Outline::Agent, "Ekran görüntüsü", "Görüntüyü onayla")
            } else {
                (Outline::Agent, "Pencere eylemi", "Eylemi onayla")
            };
            visual::profile_section(ui, icon, title, |ui| {
                ui.label(RichText::new(&approval.title).strong());
                ui.label(&approval.action);
                ui.horizontal_wrapped(|ui| {
                    if ui.button(action).clicked() {
                        self.command(Command::DecideTaskAction(approval.id, true));
                    }
                    if ui.button("Reddet").clicked() {
                        self.command(Command::DecideTaskAction(approval.id, false));
                    }
                });
            });
        } else {
            #[cfg(any(windows, target_os = "linux"))]
            self.desktop_task_controls(ui, local);
            #[cfg(target_os = "linux")]
            if local
                .jobs
                .iter()
                .any(|job| job.status == WorkspaceJobStatus::Running)
            {
                ui.weak("Linux ekran görüntüleri masaüstü izin penceresi üzerinden alınır.");
            }
        }
        for job in local.jobs.iter().rev().take(12) {
            ui.push_id((job.id, job.approval_round), |ui| {
                if job.continuation.is_some() {
                    ui.weak(if job.forked { "Yeni Codex görev kolu" } else { "Codex görevinin devamı" });
                }
                ui.label(&job.task);
                ui.weak(job.directory.display().to_string());
                if let Some(name) = &job.attachment {
                    visual::label_icon(ui, Outline::Attachment, RichText::new(name).strong());
                    ui.weak("Görev onaylanınca ek Codex tarafından incelenebilir.");
                }
                if let Some(model) = &job.model {
                    ui.weak(format!(
                        "{} · {}",
                        model.model,
                        reasoning_label(&model.effort)
                    ));
                }
                if job.plan_only {
                    ui.weak("Salt okunur planlama");
                }
                if job.review.is_some() {
                    ui.weak("Salt okunur Git incelemesi");
                }
                if job.network {
                    ui.weak("Çalışma komutlarında ağ erişimi açık.");
                }
                if !job.skill_names.is_empty() {
                    ui.weak(format!("Beceriler · {}", job.skill_names.join(", ")));
                }
                let running = job.status == WorkspaceJobStatus::Running;
                let report = if running {
                    local.workspace_progress.as_ref()
                } else {
                    local.workspace_reports.get(&job.id).map(AsRef::as_ref)
                };
                match &job.status {
                    WorkspaceJobStatus::Pending => {
                        if job.recovery.is_some() {
                            ui.weak("Kesilen görevin geçmişi korunuyor. İzinleri yeniden seçerek devam edebilirsin.");
                        }
                        use crate::agent::app_server::ModelChoice;
                        let model_id = ui.id().with("model-choice");
                        let mut model = ui.data(|data| data.get_temp::<Option<ModelChoice>>(model_id))
                            .unwrap_or_else(|| local.settings.task_model.clone());
                        let default_label = if job.continuation.is_some() {
                            "Önceki görev oturumunun ayarı"
                        } else {
                            "Codex varsayılanı"
                        };
                        if task_model_picker(ui, &local.task_models, default_label, &mut model) {
                            ui.data_mut(|data| data.insert_temp(model_id, model.clone()));
                        }
                        let skills = self.task_skill_picker(ui, local.task_skills.as_deref());
                        let mcp_id = ui.id().with("task-mcp");
                        let mut mcp = ui.data(|data| data.get_temp::<bool>(mcp_id).unwrap_or_default());
                        if local.task_mcp.as_ref().is_some_and(|connections| !connections.is_empty()) {
                            if ui.checkbox(&mut mcp, "Bu görevde listedeki MCP araçlarını kullan")
                                .on_hover_text("Çalıştırma modunda her çağrı için sunucu, araç ve parametreler ayrıca onayına sunulur.")
                                .changed() {
                                ui.data_mut(|data| data.insert_temp(mcp_id, mcp));
                            }
                        } else { mcp = false; }
                        let fork_id = ui.id().with("fork-task");
                        let mut fork = false;
                        if job.can_fork() {
                            fork = ui.data(|data| data.get_temp::<bool>(fork_id).unwrap_or_default());
                            if ui.checkbox(&mut fork, "Ayrı bir kolda devam et")
                                .on_hover_text("Yeni kol alıntılanan yanıta kadarki Codex geçmişini kullanır.")
                                .changed()
                            {
                                ui.data_mut(|data| data.insert_temp(fork_id, fork));
                            }
                        }
                        let compact_id = ui.id().with("compact-before");
                        let mut compact_before = false;
                        if job.continuation.is_some() {
                            compact_before = ui.data(|data| data.get_temp::<bool>(compact_id).unwrap_or_default());
                            if ui.checkbox(&mut compact_before, "Önce Codex bağlamını sıkıştır")
                                .on_hover_text("Codex, aynı görev oturumuna devam etmeden önce önceki bağlamı özetler.")
                                .changed()
                            {
                                ui.data_mut(|data| data.insert_temp(compact_id, compact_before));
                            }
                        }
                        let search_id = ui.id().with("job-web-research");
                        let delegate_id = ui.id().with("job-delegate");
                        let mut delegate = ui.data(|data| data.get_temp::<bool>(delegate_id).unwrap_or_default());
                        if ui.checkbox(&mut delegate, "Bu görev için alt ajanlara iş devret")
                            .on_hover_text("Codex bağımsız alt işleri yerel alt ajanlara devredebilir. Aynı anda en fazla üç alt ajan çalışır. Pencere, dosya paylaşımı ve MCP işlemlerini ana ajan yürütür.")
                            .changed() { ui.data_mut(|data| data.insert_temp(delegate_id, delegate)); }
                        let network_id = ui.id().with("job-network");
                        let mut network = ui.data(|data| data.get_temp::<bool>(network_id).unwrap_or_default());
                        ui.weak(format!("Çalıştırma erişimi · {}", local.settings.task_access.label()));
                        if local.settings.task_access == TaskAccess::Workspace
                            && ui.checkbox(&mut network, "Bu görevde ağ erişimi kullan")
                            .on_hover_text("Bu görev için Codex ağ iznini açar. Ağ kısıtlamasını cihazdaki Codex sandbox'ı uygular.")
                            .changed() {
                            ui.data_mut(|data| data.insert_temp(network_id, network));
                        }
                        #[cfg(windows)]
                        if local.settings.task_access == TaskAccess::Workspace && !network {
                            ui.small("Windows'ta yerel bağlantılar erişilebilir kalabilir.");
                        }
                        let mut web_research =
                            ui.data(|data| data.get_temp::<bool>(search_id).unwrap_or(false));
                        if ui
                            .checkbox(&mut web_research, "Bu görevde web araştırmasına izin ver")
                            .on_hover_text(
                                "Arama sorguları OpenAI’nin web arama aracı tarafından işlenir.",
                            )
                            .changed()
                        {
                            ui.data_mut(|data| data.insert_temp(search_id, web_research));
                        }
                        ui.horizontal_wrapped(|ui| {
                            for (label, plan_only) in
                                [("Planla", true), (if local.settings.task_access == TaskAccess::Full {
                                    "Tam erişimle çalıştır"
                                } else { "Bu klasörde çalıştır" }, false)]
                            {
                                if ui
                                    .button(label)
                                    .on_hover_text(if plan_only {
                                        "Klasörü salt okunur incele ve plan hazırla."
                                    } else {
                                        local.settings.task_access.label()
                                    })
                                    .clicked()
                                {
                                    self.command(Command::DecideWorkspaceJob(
                                        job.id,
                                        WorkspaceDecision::Approve {
                                            web_research,
                                            network: network && !plan_only,
                                            delegate: delegate && !plan_only,
                                            model: model.clone(),
                                            plan_only,
                                            skills: skills.clone(),
                                            compact_before,
                                            fork,
                                            mcp: mcp && !plan_only,
                                        },
                                    ));
                                }
                            }
                            if ui.button("Reddet").clicked() {
                                self.command(Command::DecideWorkspaceJob(
                                    job.id,
                                    WorkspaceDecision::Reject,
                                ));
                            }
                        });
                        if let Some(target) = task_review_picker(ui) {
                            self.command(Command::DecideWorkspaceJob(
                                job.id,
                                WorkspaceDecision::Review { model, target },
                            ));
                        }
                    }
                    WorkspaceJobStatus::Running => {
                        if job.web_research {
                            ui.weak("Web araştırmasına izin verildi.");
                        }
                        if let Some(progress) = report {
                            use crate::agent::app_server::StepState;
                            if !progress.message.is_empty() {
                                ui.weak(&progress.message);
                            }
                            for query in &progress.web_queries {
                                ui.weak(format!("Web araması · {query}"));
                            }
                            for step in &progress.steps {
                                let text = match step.state {
                                    StepState::Pending => "Bekliyor",
                                    StepState::Running => "Sürüyor",
                                    StepState::Complete => "Tamamlandı",
                                };
                                ui.label(format!("{text} · {}", step.title));
                            }
                        }
                        if ui.button("Görevi durdur").clicked() {
                            self.command(Command::CancelWorkspaceJob(job.id));
                        }
                    }
                    WorkspaceJobStatus::Complete => {
                        let pending = local.hosts.get(&job.selection.host).and_then(|link| {
                            link.queue.iter().find(|item| item.draft.id == job.id)
                        });
                        if let Some(pending) = pending {
                            ui.weak("Sonuç gönderim kuyruğunda.");
                            if let Some(error) = &pending.error {
                                ui.label(RichText::new(error).color(visual::INVALID));
                                if ui.button("Yeniden dene").clicked() {
                                    self.command(Command::Retry(job.selection.clone(), job.id));
                                }
                            }
                            if pending.binary.is_some()
                                && let Some(session) = &self.session
                            {
                                if let Some((offset, total)) = session.transfer_progress(job.id) {
                                    ui.add(
                                        egui::ProgressBar::new(offset as f32 / total as f32)
                                            .show_percentage(),
                                    );
                                }
                                if ui.button("Dosya gönderimini iptal et").clicked() {
                                    session.cancel_transfer(job.selection.clone(), job.id);
                                }
                            }
                        } else if local.hosts.get(&job.selection.host).is_some_and(|link| {
                            link.messages
                                .values()
                                .any(|message| message.client_id == job.id)
                        }) {
                            ui.weak("Sonuç gönderildi.");
                        } else {
                            ui.weak("Gönderim sona erdi.");
                        }
                    }
                    WorkspaceJobStatus::Rejected => {
                        ui.weak("Reddedildi.");
                    }
                    WorkspaceJobStatus::Cancelled => {
                        ui.weak("Durduruldu.");
                        if ui.button(if job.recovery.is_some() { "Devamı onaya al" } else { "Yeniden onaya al" }).clicked() {
                            self.command(Command::RetryWorkspaceJob(job.id));
                        }
                    }
                    WorkspaceJobStatus::Failed(error) => {
                        ui.label(RichText::new(error).color(visual::INVALID));
                        if ui.button(if job.recovery.is_some() { "Devamı onaya al" } else { "Yeniden onaya al" }).clicked() {
                            self.command(Command::RetryWorkspaceJob(job.id));
                        }
                    }
                }
                if let Some(report) = report.filter(|report| !report.agents.is_empty()) {
                    egui::CollapsingHeader::new("Alt ajanlar").default_open(true).show(ui, |ui| {
                        for agent in &report.agents {
                            let state = match agent.state.as_str() {
                                "started" | "resumed" => if running { "Çalışıyor" } else { "Son durum alınamadı" },
                                "completed" => "Tamamlandı",
                                "interrupted" => "Kesildi",
                                "closed" => "Kapatıldı",
                                _ => "Durum güncellendi",
                            };
                            visual::label_icon(ui, Outline::Agent, RichText::new(format!("{} · {state}", agent.name)).strong());
                            if let Some(model) = &agent.model {
                                ui.weak(format!("{} · {}", model, agent.effort.as_deref().map(reasoning_label).unwrap_or("Varsayılan")));
                            }
                        }
                    });
                }
                if let Some(report) = report.filter(|report| !report.commands.is_empty()) {
                    use crate::agent::app_server::CommandState;
                    egui::CollapsingHeader::new("Komut etkinliği").show(ui, |ui| {
                        for command in &report.commands {
                            let status = match (command.state, command.exit) {
                                (CommandState::Running, _) if running => "Çalışıyor",
                                (CommandState::Running, _) => "Sonuç alınamadı",
                                (CommandState::Complete, Some(exit)) if exit != 0 => "Başarısız",
                                (CommandState::Complete, _) => "Tamamlandı",
                                (CommandState::Failed, _) => "Başarısız",
                                (CommandState::Declined, _) => "Reddedildi",
                            };
                            let title = match command.exit {
                                Some(code) => format!("{status} · Çıkış kodu {code}"),
                                None => status.into(),
                            };
                            egui::CollapsingHeader::new(format!(
                                "{title} · {}",
                                short(&command.command, 80)
                            ))
                            .id_salt(&command.id)
                            .show(ui, |ui| {
                                ui.label(RichText::new(&command.command).monospace().strong());
                                if !command.output.is_empty() {
                                    ScrollArea::vertical().max_height(160.).show(ui, |ui| {
                                        ui.label(RichText::new(&command.output).monospace());
                                    });
                                }
                                if command.truncated {
                                    ui.weak("Çıktının ilk 4 KiB’ı gösteriliyor.");
                                }
                            });
                        }
                    });
                }
                if let Some(diff) = report.and_then(|report| report.diff.as_ref()) {
                    egui::CollapsingHeader::new("Dosya değişikliklerini incele").show(ui, |ui| {
                        if diff.truncated {
                            ui.weak("Önizleme ilk 64 KiB ile sınırlı.");
                        }
                        let lines: Vec<_> = diff.body.lines().collect();
                        let line_height = ui.text_style_height(&egui::TextStyle::Monospace);
                        ScrollArea::both()
                            .max_height(250.)
                            .auto_shrink([false, true])
                            .show_rows(ui, line_height, lines.len(), |ui, rows| {
                                for line in &lines[rows] {
                                    let text = RichText::new(*line).monospace();
                                    let text =
                                        if line.starts_with("diff ") || line.starts_with("@@") {
                                            text.strong()
                                        } else if line.starts_with('+') {
                                            text.color(Color32::from_rgb(151, 224, 172))
                                        } else if line.starts_with('-') {
                                            text.color(visual::INVALID)
                                        } else {
                                            text
                                        };
                                    ui.add(egui::Label::new(text).extend());
                                }
                            });
                    });
                }
                if report.is_some_and(|report| report.context_compacted) {
                    ui.weak("Codex bağlamı sıkıştırıldı.");
                }
                if let Some(usage) = report.and_then(|report| report.usage.as_ref()) {
                    egui::CollapsingHeader::new("Token kullanımı").show(ui, |ui| {
                        ui.label(format!("Oturum toplamı · {}", usage.total.total_tokens));
                        ui.weak(format!("Girdi · {} · Önbellekten · {}", usage.total.input_tokens, usage.total.cached_input_tokens));
                        ui.weak(format!("Çıktı · {} · Düşünme · {}", usage.total.output_tokens, usage.total.reasoning_output_tokens));
                        ui.weak(format!("Son model çağrısı · {} token", usage.last.total_tokens));
                        if let Some(window) = usage.model_context_window.filter(|limit| *limit > 0) {
                            ui.weak(format!("Model bağlam kapasitesi · {window} token"));
                        }
                    });
                }
                ui.separator();
            });
        }
    }

    fn mcp_setup(&mut self, ui: &mut Ui) {
        ui.collapsing("Yeni MCP bağlantısı", |ui| {
            field(ui, "Bağlantı adı", &mut self.mcp_form.name, "ör. rea");
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.mcp_form.remote, false, "Yerel program");
                ui.selectable_value(&mut self.mcp_form.remote, true, "Sunucu adresi");
            });
            if self.mcp_form.remote {
                field(ui, "MCP adresi", &mut self.mcp_form.url, "https://sunucu.example/mcp");
                ui.weak("OAuth destekleyen bağlantılarda araç listesinden tarayıcıda giriş yapabilirsin.");
            } else {
                field(ui, "Program", &mut self.mcp_form.command, "Programın tam yolu veya komut adı");
                #[cfg(any(windows, target_os = "linux"))]
                if ui.small_button("Program seç").clicked()
                    && let Some(path) = rfd::FileDialog::new().set_title("MCP programını seç").pick_file()
                { self.mcp_form.command = path.to_string_lossy().into_owned(); }
                ui.label("Argümanlar · her satıra bir tane");
                ui.add(egui::TextEdit::multiline(&mut self.mcp_form.args).desired_rows(3).desired_width(f32::INFINITY).hint_text("--port\n8080"));
                ui.weak("Program, bağlantıları listelediğinde başlatılır. Argümanları tırnak içine alma.");
            }
            let filled = !self.mcp_form.name.trim().is_empty() && if self.mcp_form.remote { !self.mcp_form.url.trim().is_empty() } else { !self.mcp_form.command.trim().is_empty() };
            if ui.add_enabled(filled, egui::Button::new("Bağlantıyı kaydet")).clicked() {
                use crate::agent::app_server::{McpSetup, McpTransport};
                let transport = if self.mcp_form.remote {
                    McpTransport::Http { url: self.mcp_form.url.trim().to_owned() }
                } else {
                    McpTransport::Stdio {
                        command: self.mcp_form.command.trim().to_owned(),
                        args: self.mcp_form.args.lines().filter(|line| !line.is_empty()).map(str::to_owned).collect(),
                    }
                };
                self.command(Command::AddTaskMcp(McpSetup { name: self.mcp_form.name.trim().to_owned(), transport }));
            }
        });
    }

    fn task_skill_picker(
        &mut self,
        ui: &mut Ui,
        skills: Option<&[crate::agent::app_server::TaskSkill]>,
    ) -> Vec<std::path::PathBuf> {
        let id = ui.id().with("task-skills");
        let mut selected = ui
            .data(|data| data.get_temp::<Vec<std::path::PathBuf>>(id))
            .unwrap_or_default();
        selected.retain(|path| {
            skills.is_some_and(|skills| skills.iter().any(|skill| skill.path == *path))
        });
        let title = if selected.is_empty() {
            "Codex becerileri".into()
        } else {
            format!("Codex becerileri · {} seçildi", selected.len())
        };
        egui::CollapsingHeader::new(title)
            .id_salt("skill-picker")
            .show(ui, |ui| {
                if ui
                    .button(if skills.is_some() {
                        "Becerileri yenile"
                    } else {
                        "Becerileri yükle"
                    })
                    .clicked()
                {
                    self.command(Command::LoadTaskSkills);
                }
                if let Some(skills) = skills {
                    if skills.is_empty() {
                        ui.weak("Bu klasör için etkin beceri bulunamadı.");
                    }
                    for skill in skills {
                        let active = selected.iter().position(|path| *path == skill.path);
                        let mut checked = active.is_some();
                        ui.push_id(&skill.path, |ui| {
                            let scope = match skill.scope.as_str() {
                                "repo" => "Proje",
                                "user" => "Kişisel",
                                "system" => "Sistem",
                                "admin" => "Yönetici",
                                other => other,
                            };
                            if ui
                                .checkbox(&mut checked, format!("{} · {scope}", skill.name))
                                .on_hover_text(&skill.description)
                                .changed()
                            {
                                if let Some(index) = active {
                                    selected.remove(index);
                                }
                                if checked {
                                    selected.push(skill.path.clone());
                                }
                            }
                        });
                    }
                }
                ui.weak("Seçimler yalnızca bu görevin planlama veya çalıştırma adımına eklenir.");
            });
        ui.data_mut(|data| data.insert_temp(id, selected.clone()));
        selected
    }

    #[cfg(any(windows, target_os = "linux"))]
    fn desktop_task_controls(&mut self, ui: &mut Ui, local: &crate::session::Local) {
        #[cfg(target_os = "linux")]
        if !crate::agent::window_tools::is_native_x11_session() {
            return;
        }
        ui.add_space(8.);
        visual::profile_section(ui, Outline::Agent, "Pencere araçları", |ui| {
            let running = local
                .jobs
                .iter()
                .any(|job| job.status == crate::session::WorkspaceJobStatus::Running);
            let request = local.desktop_request.as_ref();
            let target = if running {
                local.desktop_active.as_ref()
            } else {
                local.desktop_target.as_ref()
            };
            if let Some(request) = request {
                ui.label(RichText::new("Codex pencere seçmeni istiyor").strong());
                ui.label(&request.reason);
            } else if running {
                if let Some(target) = target {
                    ui.label(&target.title);
                }
                ui.weak("Pencere eylemi istenirse burada onay isteyecek.");
                return;
            }
            let title =
                target.map_or_else(|| "Pencere seç…".into(), |target| short(&target.title, 31));
            egui::ComboBox::from_id_salt("desktop-task-target")
                .selected_text(title)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    if request.is_none()
                        && ui
                            .selectable_label(target.is_none(), "Pencere kullanma")
                            .clicked()
                    {
                        self.command(Command::SetDesktopTarget(None));
                    }
                    let windows = crate::agent::window_tools::windows();
                    if windows.is_empty() {
                        ui.weak("Uygun, açık pencere bulunamadı.");
                    }
                    for window in windows {
                        if ui
                            .selectable_label(target == Some(&window), short(&window.title, 55))
                            .on_hover_text(&window.title)
                            .clicked()
                        {
                            self.command(match request {
                                Some(request) => {
                                    Command::DecideDesktopTarget(request.id, Some(window))
                                }
                                None => Command::SetDesktopTarget(Some(window)),
                            });
                        }
                    }
                });
            if let Some(request) = request {
                ui.weak("Ekran görüntüsü ve giriş işlemleri ayrıca onay ister.");
                if ui.button("Pencere isteğini reddet").clicked() {
                    self.command(Command::DecideDesktopTarget(request.id, None));
                }
            } else {
                ui.weak("Seçim sıradaki görev içindir. Her pencere eylemi ayrı onay ister.");
                ui.weak("Onaylanan görüntüler Codex görev geçmişine girebilir.");
            }
        });
        ui.add_space(8.);
    }
}

fn right_drawer(
    ctx: &UiContext,
    role: Role,
    id: &'static str,
    icon: Outline,
    title: &str,
    contents: impl FnOnce(&mut Ui),
) -> bool {
    let width = (ctx.available_rect().width() * 0.42).min(306.);
    egui::SidePanel::right(id)
        .exact_width(width)
        .resizable(false)
        .frame(
            Frame::new()
                .fill(visual::palette(role).panel)
                .inner_margin(14),
        )
        .show(ctx, |ui| {
            let mut close = false;
            ui.horizontal(|ui| {
                visual::label_icon(ui, icon, RichText::new(title).size(16.).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = visual::rail_button(ui, Outline::Cross, "Kapat", false).clicked();
                });
            });
            ui.separator();
            ui.add_space(10.);
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, contents);
            close
        })
        .inner
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
            self.viewer.close();
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
        if self.viewer.is_open() {
            self.viewer.show(ctx);
        }
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
fn task_review_picker(ui: &mut Ui) -> Option<crate::agent::app_server::ReviewTarget> {
    use crate::agent::app_server::ReviewTarget;
    egui::CollapsingHeader::new("Kod incelemesi")
        .show(ui, |ui| {
            const TARGETS: [&str; 3] = [
                "Kaydedilmemiş değişiklikler",
                "Temel dalla karşılaştır",
                "Bir commit seç",
            ];
            let state = ui.id().with("git-review-target");
            let (mut kind, mut reference) = ui
                .data(|data| data.get_temp::<(usize, String)>(state))
                .unwrap_or_default();
            egui::ComboBox::from_id_salt("git-target")
                .selected_text(TARGETS[kind])
                .show_ui(ui, |ui| {
                    for (index, label) in TARGETS.iter().enumerate() {
                        if ui.selectable_value(&mut kind, index, *label).clicked() {
                            reference.clear();
                        }
                    }
                });
            if kind != 0 {
                ui.add(
                    egui::TextEdit::singleline(&mut reference).hint_text(if kind == 1 {
                        "ör. main"
                    } else {
                        "Commit kimliği"
                    }),
                );
            }
            let target = match kind {
                1 => ReviewTarget::BaseBranch {
                    branch: reference.trim().into(),
                },
                2 => ReviewTarget::Commit {
                    sha: reference.trim().into(),
                },
                _ => ReviewTarget::UncommittedChanges,
            };
            ui.data_mut(|data| data.insert_temp(state, (kind, reference)));
            ui.add_enabled(target.valid(), egui::Button::new("İncelemeyi başlat"))
                .clicked()
                .then_some(target)
        })
        .body_returned
        .flatten()
}

fn task_access_picker(ui: &mut Ui, access: &mut TaskAccess) {
    ui.label("Görev erişimi");
    egui::ComboBox::from_id_salt("task-access")
        .selected_text(access.label())
        .show_ui(ui, |ui| {
            for (level, description) in [
                (
                    TaskAccess::Workspace,
                    "Yazma işlemleri çalışma klasörüyle sınırlı.",
                ),
                (
                    TaskAccess::WorkspaceNetwork,
                    "Ağ açık, yazma işlemleri çalışma klasörüyle sınırlı.",
                ),
                (
                    TaskAccess::Full,
                    "Uygulamanın Windows hesabıyla erişebildiği dosyalar ve ağ.",
                ),
            ] {
                ui.selectable_value(access, level, level.label())
                    .on_hover_text(description);
            }
        });
}

fn task_model_picker(
    ui: &mut Ui,
    models: &[crate::agent::app_server::CatalogModel],
    default_label: &str,
    selected: &mut Option<crate::agent::app_server::ModelChoice>,
) -> bool {
    use crate::agent::app_server::ModelChoice;
    let original = selected.clone();
    let selected_name = selected.as_ref().and_then(|choice| {
        models
            .iter()
            .find(|model| model.model == choice.model)
            .map(|model| model.display_name.as_str())
    });
    ui.label("Görev modeli");
    egui::ComboBox::from_id_salt("task-model")
        .selected_text(
            selected_name
                .or_else(|| selected.as_ref().map(|choice| choice.model.as_str()))
                .unwrap_or(default_label),
        )
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(selected.is_none(), default_label)
                .clicked()
            {
                *selected = None;
            }
            for model in models {
                let active = selected
                    .as_ref()
                    .is_some_and(|choice| choice.model == model.model);
                if ui.selectable_label(active, &model.display_name).clicked() && !active {
                    *selected = Some(ModelChoice {
                        model: model.model.clone(),
                        effort: model.default_reasoning_effort.clone(),
                    });
                }
            }
        });
    if let Some(choice) = selected {
        ui.label("Düşünme düzeyi");
        egui::ComboBox::from_id_salt("task-effort")
            .selected_text(reasoning_label(&choice.effort))
            .show_ui(ui, |ui| {
                if let Some(model) = models.iter().find(|model| model.model == choice.model) {
                    for effort in &model.supported_reasoning_efforts {
                        let value = &effort.reasoning_effort;
                        ui.selectable_value(
                            &mut choice.effort,
                            value.clone(),
                            reasoning_label(value),
                        );
                    }
                } else {
                    ui.weak("Düzeyi değiştirmek için model listesini yükle.");
                }
            });
    }
    *selected != original
}

fn reasoning_label(effort: &str) -> &str {
    match effort {
        "none" => "Kapalı",
        "minimal" => "En düşük",
        "low" => "Düşük",
        "medium" => "Orta",
        "high" => "Yüksek",
        "xhigh" => "Çok yüksek",
        "max" => "Azami",
        value => value,
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

fn message_web_links(text: &str) -> Vec<reqwest::Url> {
    let mut urls = Vec::new();
    for (start, _) in text.match_indices("https://") {
        let value = text[start..]
            .split(|ch: char| ch.is_whitespace() || matches!(ch, ')' | ']' | '>' | '"' | '`'))
            .next()
            .unwrap_or_default();
        let value = value.trim_end_matches(['.', ',', ';']);
        if value.len() <= 400
            && let Ok(url) = reqwest::Url::parse(value)
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && !urls.contains(&url)
        {
            urls.push(url);
            if urls.len() == 5 {
                break;
            }
        }
    }
    urls
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

struct SlashCommandSuggestion {
    command: &'static str,
    description: &'static str,
    icon: Outline,
}

fn slash_command_token(draft: &str, cursor: usize) -> Option<(usize, usize, String)> {
    let chars = draft.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let start = chars[..cursor].iter().rposition(|ch| *ch == '/')?;
    if start > 0 && !chars[start - 1].is_whitespace() {
        return None;
    }
    if chars[start..cursor].iter().any(|ch| ch.is_whitespace()) {
        return None;
    }
    let end = start
        + chars[start..]
            .iter()
            .position(|ch| ch.is_whitespace())
            .unwrap_or(chars.len() - start);
    Some((start, end, chars[start..cursor].iter().collect()))
}

fn replace_slash_token(draft: &str, start: usize, end: usize, command: &str) -> (String, usize) {
    let byte_at = |char_index: usize| {
        draft
            .char_indices()
            .nth(char_index)
            .map_or(draft.len(), |(byte, _)| byte)
    };
    let start_byte = byte_at(start);
    let end_byte = byte_at(end);
    let add_space = end == draft.chars().count();
    let insertion = if add_space {
        format!("{command} ")
    } else {
        command.to_owned()
    };
    let caret = start + insertion.chars().count();
    let mut result = String::with_capacity(draft.len() + insertion.len());
    result.push_str(&draft[..start_byte]);
    result.push_str(&insertion);
    result.push_str(&draft[end_byte..]);
    (result, caret)
}

fn consume_mode_slash_token(draft: &str, start: usize, end: usize) -> (String, usize) {
    let byte_at = |char_index: usize| {
        draft
            .char_indices()
            .nth(char_index)
            .map_or(draft.len(), |(byte, _)| byte)
    };
    let start_byte = byte_at(start);
    let mut end_byte = byte_at(end);
    // Remove the command's separator too, preserving surrounding prose.
    if draft[end_byte..].starts_with(' ') {
        end_byte += 1;
    }
    let mut result = String::with_capacity(draft.len());
    result.push_str(&draft[..start_byte]);
    result.push_str(&draft[end_byte..]);
    let caret = start.min(result.chars().count());
    (result, caret)
}

fn loop_interval_label(seconds: u64) -> String {
    match seconds {
        10 => "10 sn".into(),
        1800 => "30 dk".into(),
        3600 => "1 saat".into(),
        86400 => "1 gün".into(),
        _ => format!("{seconds} sn"),
    }
}

fn slash_mode(query: &str) -> Option<crate::protocol::MessageMode> {
    match query {
        "/goal" => Some(crate::protocol::MessageMode::Goal),
        "/loop" => Some(crate::protocol::MessageMode::Loop {
            interval_seconds: 60 * 60,
        }),
        _ => None,
    }
}

// A complete command at the start of the draft still selects its mode even
// after the caret has moved into the task text (including pasted drafts).
fn leading_slash_mode(draft: &str) -> Option<(usize, usize, MessageMode)> {
    use crate::session::automation::parse_interval;
    let trimmed = draft.trim_start();
    let token = trimmed.split_whitespace().next()?;
    let mut mode = slash_mode(token)?;
    let start = draft[..draft.len() - trimmed.len()].chars().count();
    let mut end = start + token.chars().count();
    if matches!(mode, MessageMode::Loop { .. }) {
        let suffix = &trimmed[token.len()..];
        let interval = suffix.trim_start();
        if let Some(value) = interval.split_whitespace().next()
            && let Ok(interval_seconds) = parse_interval(value)
        {
            end += suffix[..suffix.len() - interval.len()].chars().count() + value.chars().count();
            mode = MessageMode::Loop { interval_seconds };
        }
    }
    Some((start, end, mode))
}

fn is_automation_management(draft: &str) -> bool {
    use crate::session::automation::{AutomationCommand, parse};
    if !draft.trim_start().starts_with('/') || draft.trim() == "/loop" {
        return false;
    }
    matches!(
        parse(draft),
        Ok(Some(
            AutomationCommand::GoalStatus
                | AutomationCommand::GoalStop
                | AutomationCommand::GoalResume
                | AutomationCommand::LoopList
                | AutomationCommand::LoopStop { .. }
        ))
    )
}

fn set_composer_cursor(ctx: &UiContext, id: egui::Id, cursor: usize) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::new(cursor),
        )));
    state.store(ctx, id);
}

fn slash_command_suggestions(query: &str) -> Vec<&'static SlashCommandSuggestion> {
    static COMMANDS: [SlashCommandSuggestion; 2] = [
        SlashCommandSuggestion {
            command: "/loop",
            description: "Tekrarlanan bir çalışma planla",
            icon: Outline::Loop,
        },
        SlashCommandSuggestion {
            command: "/goal",
            description: "Kalıcı bir hedefi izle",
            icon: Outline::Goal,
        },
    ];
    if !query.starts_with('/') || query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|command| command.command.starts_with(query))
        .collect()
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
    fn slash_autocomplete_limits_choices_to_supported_command_prefixes() {
        let labels = |draft: &str| {
            slash_command_suggestions(draft)
                .iter()
                .map(|suggestion| suggestion.command)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels("/"), ["/loop", "/goal"]);
        assert_eq!(labels("/l"), ["/loop"]);
        assert_eq!(labels("/g"), ["/goal"]);
        assert!(labels("/unknown").is_empty());
        assert!(labels("/goal status").is_empty());
    }

    #[test]
    fn slash_token_is_found_at_cursor_inside_a_sentence() {
        let draft = "Merhaba dünya /go devam";
        let cursor = "Merhaba dünya /go".chars().count();
        let (start, end, query) = slash_command_token(draft, cursor).unwrap();
        assert_eq!(query, "/go");
        assert_eq!(
            draft
                .chars()
                .skip(start)
                .take(end - start)
                .collect::<String>(),
            "/go"
        );
        assert_eq!(slash_command_suggestions(&query).len(), 1);
    }

    #[test]
    fn completing_middle_token_preserves_unicode_prefix_and_suffix() {
        let draft = "Türkçe /go devam eder";
        let (start, end, _) = slash_command_token(draft, "Türkçe /go".chars().count()).unwrap();
        let (updated, caret) = replace_slash_token(draft, start, end, "/goal");
        assert_eq!(updated, "Türkçe /goal devam eder");
        assert_eq!(
            updated.chars().take(caret).collect::<String>(),
            "Türkçe /goal"
        );
    }

    #[test]
    fn slash_suggestions_use_only_text_before_a_mid_token_caret() {
        let draft = "Türkçe /goal devam";
        let cursor = "Türkçe /go".chars().count();
        let (start, end, query) = slash_command_token(draft, cursor).unwrap();
        assert_eq!(query, "/go");
        assert_eq!(slash_command_suggestions(&query).len(), 1);
        let (updated, _) = replace_slash_token(draft, start, end, "/goal");
        assert_eq!(updated, "Türkçe /goal devam");
    }

    #[test]
    fn completing_end_token_adds_space_and_places_caret_after_command() {
        let draft = "İş /loo";
        let (start, end, _) = slash_command_token(draft, draft.chars().count()).unwrap();
        let (updated, caret) = replace_slash_token(draft, start, end, "/loop");
        assert_eq!(updated, "İş /loop ");
        assert_eq!(updated.chars().take(caret).collect::<String>(), "İş /loop ");
    }

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
    fn enter_commits_goal_without_sending_then_enter_sends_clean_text() -> Result<()> {
        let me = Identity::generate(Role::Human, "deniz", "Deniz")?;
        let room = Room {
            id: uuid::Uuid::new_v4(),
            name: "Test".into(),
            owner: me.id().into(),
            group: false,
            active: true,
            closed: false,
            members: vec![me.card.clone()],
        };
        let selection = Selection {
            host: "test".into(),
            room: room.id,
        };
        let ctx = UiContext::default();
        visual::theme(&ctx, Role::Human);
        let mut app = App::new(Role::Human, PathBuf::new());
        app.identity = Some(me);
        app.draft = "/goal Deneme hedefi".into();
        let draw = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1080., 720.),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| app.chat_composer(ui, &selection, &room, 0));
                },
            )
        };
        draw(&mut app, vec![]);
        let id = egui::Id::new(("conversation-composer", room.id)).with("input");
        ctx.memory_mut(|memory| memory.request_focus(id));
        set_composer_cursor(&ctx, id, app.draft.chars().count());
        let enter = || egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        draw(&mut app, vec![enter()]);
        assert_eq!(app.draft_mode, Some(MessageMode::Goal));
        assert_eq!(app.draft, "Deneme hedefi");
        assert!(app.submitted.is_none(), "selecting a mode must not send");
        draw(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        draw(&mut app, vec![enter()]);
        let sent = &app.submitted.as_ref().expect("ordinary Enter sends").2;
        assert_eq!(sent.text, "Deneme hedefi");
        assert_eq!(sent.mode, Some(MessageMode::Goal));
        Ok(())
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
            mode: None,
            withdrawn: false,
            undo_until_ms: 0,
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
    fn profile_actions_open_the_editor_to_the_right() -> Result<()> {
        let ctx = UiContext::default();
        visual::theme(&ctx, Role::Human);
        let mut app = App::new(Role::Human, PathBuf::new());
        app.identity = Some(Identity::generate(Role::Human, "deniz", "Deniz")?);
        app.page = Page::Profile;
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1080., 850.));
        let draw = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(bounds),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    app.navigation(ctx);
                    app.workspace(ctx);
                },
            )
        };
        let point = |frame: &egui::FullOutput, label: &str| {
            frame.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.job.text == label => {
                    Some(shape.shape.visual_bounding_rect().center())
                }
                _ => None,
            })
        };
        let click = |position, pressed| {
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        let initial = draw(&mut app, vec![]);
        let hero = initial
            .shapes
            .iter()
            .find_map(|entry| match &entry.shape {
                egui::Shape::Rect(rect)
                    if rect.fill == visual::palette(Role::Human).tint
                        && rect.rect.height() > 120.
                        && rect.rect.width() > 200. =>
                {
                    Some(rect.rect)
                }
                _ => None,
            })
            .expect("profile hero");
        assert!(hero.width() <= 421. && (hero.center().x - bounds.center().x).abs() < 34.);
        assert!(point(&initial, "Adı kaydet").is_none());

        let edit = point(&initial, "Görünen adı düzenle").expect("edit action");
        draw(&mut app, click(edit, true));
        draw(&mut app, click(edit, false));
        let editor = draw(&mut app, vec![]);
        assert_eq!(app.profile_pane, Some(ProfilePane::Name));
        assert!(point(&editor, "Adı kaydet").expect("save action").x > edit.x + 200.);
        let cancel = point(&editor, "Vazgeç").expect("cancel action");
        draw(&mut app, click(cancel, true));
        draw(&mut app, click(cancel, false));
        let closed = draw(&mut app, vec![]);
        assert_eq!(app.profile_pane, None);

        let photo = point(&closed, "Fotoğraf ekle").expect("photo action");
        draw(&mut app, click(photo, true));
        draw(&mut app, click(photo, false));
        let options = draw(&mut app, vec![]);
        assert_eq!(app.profile_pane, Some(ProfilePane::Photo));
        assert!(point(&options, "Profil fotoğrafı").expect("photo panel").x > photo.x + 200.);
        let narrow = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(720., 505.));
        let narrow_frame = |app: &mut App| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(narrow),
                    ..Default::default()
                },
                |ctx| {
                    app.navigation(ctx);
                    app.workspace(ctx);
                },
            )
        };
        narrow_frame(&mut app);
        let overlay = narrow_frame(&mut app);
        let photo_title = point(&overlay, "Profil fotoğrafı").expect("narrow photo panel");
        let profile_title = point(&overlay, "Profilim").expect("profile stays visible");
        assert!(photo_title.x > 450. && photo_title.x > profile_title.x + 200.);
        assert!(photo_title.y <= profile_title.y);
        Ok(())
    }

    #[test]
    fn codex_jobs_open_next_to_agent_identity() -> Result<()> {
        let ctx = UiContext::default();
        visual::theme(&ctx, Role::Agent);
        let mut app = App::new(Role::Agent, PathBuf::new());
        app.identity = Some(Identity::generate(Role::Agent, "poyraz", "Poyraz")?);
        let draw = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(720., 505.),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    app.navigation(ctx);
                    app.workspace(ctx);
                },
            )
        };
        let initial = draw(&mut app, vec![]);
        let label_positions = |frame: &egui::FullOutput| {
            frame
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.job.text == "Codex görevleri" => {
                        Some(shape.shape.visual_bounding_rect().center())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let opener = label_positions(&initial)[0];
        for pressed in [true, false] {
            draw(
                &mut app,
                vec![
                    egui::Event::PointerMoved(opener),
                    egui::Event::PointerButton {
                        pos: opener,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        let open = draw(&mut app, vec![]);
        let positions = label_positions(&open);
        assert_eq!(positions.len(), 2);
        assert!(
            positions
                .iter()
                .any(|pos| pos.x > opener.x + 190. && pos.y <= opener.y)
        );
        let closed = draw(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(label_positions(&closed).len(), 1);
        assert!(app.workspace_jobs_opener.is_none());
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
            (Page::Profile, "Profilim", "Görünen adı düzenle"),
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
