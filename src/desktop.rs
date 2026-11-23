use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use eframe::egui::{
    self, Align, Color32, Context as UiContext, Frame, Layout, RichText, ScrollArea, Ui,
};
use zeroize::Zeroize;

#[path = "desktop/remember.rs"]
mod remember;
#[cfg(windows)]
#[path = "desktop/taskbar.rs"]
mod taskbar;
#[path = "desktop/visual.rs"]
mod visual;

use visual::{INK, MUTED, Outline};

use crate::{
    agent::{FilePermit, Settings},
    host::mentions,
    identity::{
        AgentProfile, HANDLE_RULE, Identity, NAME_RULE, PASSWORD_MIN, Role, pack, password_rule,
        validate_handle, validate_name, validate_password,
    },
    protocol::{Grant, Message, Request, Room, Status, allowed_ip},
    session::{Command, Link, Selection, Session, View},
};

pub fn run(role: Role) -> Result<()> {
    let data = match std::env::args_os().nth(1) {
        Some(flag) if flag == "--data-dir" => PathBuf::from(
            std::env::args_os()
                .nth(2)
                .context("--data-dir için bir klasör gerekli")?,
        ),
        Some(_) => anyhow::bail!("Kullanım: [--data-dir KLASÖR]"),
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
            app.restore_saved();
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
    reply: Option<(i64, String, String)>,
    jump: Option<i64>,
    read_sent: Option<(Selection, i64)>,
    invite: bool,
    confirm_close: bool,
    settings: Settings,
    settings_dirty: bool,
    notice: String,
    quit: bool,
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
            reply: None,
            jump: None,
            read_sent: None,
            invite: false,
            confirm_close: false,
            settings: Settings::default(),
            settings_dirty: false,
            notice: String::new(),
            quit: false,
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
                    if self.role == Role::Human {
                        nav(ui, &mut self.page, Page::Chats, "Sohbetler");
                        let archive = if count == 0 {
                            "Arşivlenenler".into()
                        } else {
                            format!("Arşivlenenler · {count}")
                        };
                        nav(ui, &mut self.page, Page::Archive, &archive);
                    } else {
                        nav(ui, &mut self.page, Page::Agent, "Ajan");
                    }
                    let contact = if pending == 0 {
                        "Kişiler ve istekler".into()
                    } else {
                        format!("Kişiler ve istekler · {pending}")
                    };
                    nav(ui, &mut self.page, Page::Contacts, &contact);
                    nav(ui, &mut self.page, Page::Host, "Host");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.menu_button(
                            format!(
                                "{}  ▾",
                                short(
                                    &self.identity.as_ref().expect("signed in").card.data.name,
                                    21
                                )
                            ),
                            |ui| {
                                if visual::work_tab(ui, Outline::Lock, "Oturumu kapat", false)
                                    .clicked()
                                {
                                    if let Err(error) = self.sign_out() {
                                        self.notice = format!("Oturum kapatılamadı. {error:#}");
                                    }
                                    ui.close();
                                }
                                if ui.button("Uygulamadan çık").clicked() {
                                    self.quit = true;
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            },
                        );
                    });
                });
            });
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
            self.selected = Some(selection);
            self.draft.clear();
            self.reply = None;
            self.jump = None;
            self.invite = false;
            self.read_sent = None;
            self.confirm_close = false;
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
                        let width =
                            ui.available_width()
                                .min(if page == Page::Host { 700. } else { 910. });
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
                                        host.last(room.id).map(|m| (m.sent_at, m.text.clone())),
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
                        if visual::chat_row(
                            ui,
                            &name,
                            &preview,
                            &time.replace(" UTC", ""),
                            unread,
                            targeted,
                            self.selected.as_ref() == Some(&selection),
                        )
                        .clicked()
                        {
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
            visual::avatar(ui, &room.name, 32.);
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
        if room.active && !room.closed {
            self.chat_composer(ui, &s);
        }
        visual::chat_background(ui, ui.available_rect_before_wrap());
        let bottom = self.jump.is_none();
        ScrollArea::vertical()
            .id_salt((&s.host, s.room))
            .max_height(ui.available_height())
            .auto_shrink([false, false])
            .stick_to_bottom(bottom)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.;
                for message in messages {
                    self.message(ui, message, &s, room.group);
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
                    );
                    visual::bubble_lane(ui, true, &size, false, |ui| {
                        visual::bubble_text(ui, &size, &item.draft.text, stamp, true);
                        if let Some(error) = &item.error
                            && ui.link("Yeniden dene").on_hover_text(error).clicked()
                        {
                            self.command(Command::Retry(s.clone(), item.draft.id));
                        }
                    });
                }
            });
    }

    fn chat_composer(&mut self, ui: &mut Ui, selection: &Selection) {
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
                let enter = ui.memory(|memory| memory.has_focus(draft_id))
                    && ui.input_mut(|input| {
                        !input.modifiers.shift
                            && input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });
                let (field, button) = ui
                    .horizontal(|ui| {
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
                        let send =
                            visual::chat_icon_button(ui, true, !self.draft.trim().is_empty())
                                .clicked();
                        (field, send)
                    })
                    .inner;
                if (enter || button) && !self.draft.trim().is_empty() {
                    let text = std::mem::take(&mut self.draft);
                    let reply = self.reply.take().map(|quote| quote.0);
                    self.command(Command::Send(selection.clone(), text, reply));
                    field.request_focus();
                }
            });
    }

    fn message(&mut self, ui: &mut Ui, message: &Message, selection: &Selection, group: bool) {
        let colors = visual::palette(self.role);
        let mine = message.author.data.key == self.id();
        let author = (group && !mine).then_some(message.author.data.name.as_str());
        let stamp = time(message.sent_at);
        let stamp = stamp.trim_end_matches(" UTC");
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
            if message.invitation.is_some() && !mine {
                75.
            } else {
                0.
            },
        ]
        .into_iter()
        .fold(0_f32, f32::max);
        let size = visual::measure_bubble(ui, &message.text, stamp, extra);
        let (bubble, replied) =
            visual::bubble_lane(ui, mine, &size, message.invitation.is_none(), |ui| {
                if let Some(author) = author {
                    ui.add(
                        egui::Label::new(
                            RichText::new(author)
                                .size(12.)
                                .strong()
                                .color(colors.accent),
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
                visual::bubble_text(ui, &size, &message.text, stamp, false);
                if let Some(ticket) = &message.invitation
                    && !mine
                    && ui.link("Daveti aç").clicked()
                    && let Ok(code) = pack("invite", ticket)
                {
                    self.command(Command::Import(code));
                    self.page = Page::Contacts;
                }
            });
        if replied {
            self.reply = Some((
                message.seq,
                message.author.data.name.clone(),
                short(&message.text, 100),
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
                        if ui.button("Panodan yapıştır").clicked() {
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
                    self.command(Command::Import(code));
                }
            })
        };
        if ui.available_width() >= 660. {
            ui.columns(2, |columns| {
                share(&mut columns[0]);
                import(&mut columns[1]);
            });
        } else {
            share(ui);
            import(ui);
        }
        ui.add_space(12.);
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
                        visual::avatar(ui, &card.data.name, 33.);
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
        ui.horizontal(|ui| {
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
        self.update_view(ctx);
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quit
            && (self.view.as_ref().is_some_and(|v| v.host.is_some())
                || (self.role == Role::Agent && self.session.is_some()))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
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
        self.workspace(ctx);
        ctx.request_repaint_after(Duration::from_millis(180));
    }
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
    format!("{:02}:{:02} UTC", (epoch / 3600) % 24, (epoch / 60) % 60)
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
