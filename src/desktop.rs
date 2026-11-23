use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use directories::ProjectDirs;
use eframe::egui::{
    self, Align, Color32, Context as UiContext, Frame, Layout, RichText, ScrollArea, Ui,
};
use zeroize::Zeroize;

use crate::{
    agent::{FilePermit, Settings},
    host::mentions,
    identity::{Identity, Role, pack},
    protocol::{Grant, Message, Request, Room, Status},
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
    }
    .join(if role == Role::Human {
        "human"
    } else {
        "agent"
    });
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
            theme(&cc.egui_ctx);
            Ok(Box::new(App::new(role, data)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

fn theme(ctx: &UiContext) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = Color32::from_rgb(24, 20, 33);
    style.visuals.window_fill = Color32::from_rgb(33, 26, 44);
    style.visuals.extreme_bg_color = Color32::from_rgb(20, 16, 29);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(48, 37, 65);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(65, 47, 87);
    style.visuals.selection.bg_fill = Color32::from_rgb(81, 52, 118);
    style.visuals.hyperlink_color = Color32::from_rgb(208, 175, 254);
    style.spacing.item_spacing = egui::vec2(9., 8.);
    style.spacing.button_padding = egui::vec2(11., 6.);
    ctx.set_style(style);
}

#[derive(Clone, Copy, PartialEq, Eq)]
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
    agent_path: String,
    bind: String,
    import: String,
    search: String,
    pair: String,
    group_name: String,
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
            agent_path: String::new(),
            bind: if role == Role::Human {
                "127.0.0.1:47821"
            } else {
                "127.0.0.1:47822"
            }
            .into(),
            import: String::new(),
            search: String::new(),
            pair: String::new(),
            group_name: String::new(),
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

    fn identity_path(&self) -> Result<PathBuf> {
        let handle = self.handle.trim().to_ascii_lowercase();
        ensure!(
            (3..=24).contains(&handle.len())
                && handle
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "Kullanıcı adı 3–24 küçük Latin harfi, rakam veya alt çizgi içermeli"
        );
        Ok(self.data.join("accounts").join(format!(
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
            let identity = Identity::generate(self.role, &self.handle, &self.name)?;
            identity.save(&path, password)?;
            identity
        } else {
            Identity::load(&path, self.role, password).context("Hesap açılamadı")?
        };
        self.session = Some(Session::new(self.data.join("profiles"), identity.clone())?);
        self.identity = Some(identity);
        if self.signing_up && self.role == Role::Agent {
            self.notice = format!("Ajan anahtarı oluşturuldu · {}", path.display());
        } else {
            self.notice.clear();
        }
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
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(38.);
            ui.vertical_centered(|ui| {
                ui.heading(
                    RichText::new(if self.role == Role::Human {
                        "◈  ChatOid"
                    } else {
                        "◈  ChatOid Ajan"
                    })
                    .size(32.),
                );
                ui.add_space(15.);
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            !self.signing_up,
                            if self.role == Role::Human {
                                "Giriş yap"
                            } else {
                                "Anahtarı aç"
                            },
                        )
                        .clicked()
                    {
                        self.signing_up = false;
                    }
                    if ui
                        .selectable_label(
                            self.signing_up,
                            if self.role == Role::Human {
                                "Hesap oluştur"
                            } else {
                                "Ajan oluştur"
                            },
                        )
                        .clicked()
                    {
                        self.signing_up = true;
                    }
                });
                ui.add_space(8.);
                ui.set_max_width(530.);
                field(ui, "Kullanıcı adı", &mut self.handle, "ör. deniz");
                if self.signing_up {
                    field(ui, "Görünen ad", &mut self.name, "ör. Deniz");
                }
                if self.role == Role::Human {
                    ui.label("Parola");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                } else if !self.signing_up {
                    field(
                        ui,
                        "Mevcut .key dosyası (isteğe bağlı)",
                        &mut self.agent_path,
                        "Dosyanın tam yolu",
                    );
                }
                if ui
                    .button(if self.signing_up {
                        "Oluştur"
                    } else {
                        "Bağlan"
                    })
                    .clicked()
                {
                    if let Err(error) = self.sign_in() {
                        self.notice = if self.role == Role::Human && !self.signing_up {
                            "Kullanıcı adı veya parola yanlış.".into()
                        } else {
                            format!("{error:#}")
                        };
                    }
                    self.password.zeroize();
                }
                if !self.notice.is_empty() {
                    ui.label(&self.notice);
                }
            });
        });
    }

    fn navigation(&mut self, ctx: &UiContext) {
        egui::TopBottomPanel::top("navigation").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(if self.role == Role::Human {
                        "◈  ChatOid"
                    } else {
                        "◈  ChatOid Ajan"
                    })
                    .strong()
                    .size(20.),
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
                        self.identity
                            .as_ref()
                            .expect("signed in")
                            .card
                            .data
                            .name
                            .clone(),
                        |ui| {
                            if ui.button("Oturumu kapat").clicked() {
                                self.session = None;
                                self.identity = None;
                                self.view = None;
                                self.selected = None;
                                self.draft.clear();
                                self.reply = None;
                                self.notice.clear();
                                self.settings_dirty = false;
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
            egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
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

    fn inbox(&mut self, ctx: &UiContext) {
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
            .default_width(290.)
            .width_range(230. ..=380.)
            .show(ctx, |ui| {
                ui.add_space(10.);
                ui.heading(if archive {
                    "Arşivlenenler"
                } else {
                    "Sohbetler"
                });
                if !archive {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.group_name)
                                .hint_text("Grup adı")
                                .desired_width(120.),
                        );
                        if ui.button("Yeni grup").clicked() && !self.group_name.trim().is_empty() {
                            let name = std::mem::take(&mut self.group_name);
                            self.command(Command::Group(name));
                        }
                    });
                }
                ui.separator();
                ScrollArea::vertical().show(ui, |ui| {
                    for (selection, name, unread, targeted, last) in rows {
                        let (time, preview) = last
                            .map(|(t, p)| (time(t), short(&p, 36)))
                            .unwrap_or_default();
                        let avatar = name
                            .chars()
                            .next()
                            .unwrap_or('?')
                            .to_uppercase()
                            .to_string();
                        let count = if unread == 0 {
                            String::new()
                        } else {
                            format!("   {}{unread}", if targeted { "@" } else { "" })
                        };
                        let label = format!("{avatar}   {name}{count}    {time}\n      {preview}");
                        if ui
                            .add_sized(
                                [ui.available_width(), 51.],
                                egui::Button::selectable(
                                    self.selected.as_ref() == Some(&selection),
                                    label,
                                ),
                            )
                            .clicked()
                        {
                            self.choose(selection);
                        }
                    }
                });
            });
        egui::CentralPanel::default().show(ctx, |ui| {
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
                ui.centered_and_justified(|ui| {
                    ui.label(if archive {
                        "Arşivlenmiş sohbet yok."
                    } else {
                        "Bir kişi ekle veya yeni grup oluştur."
                    });
                });
            }
        });
    }

    fn conversation(&mut self, ui: &mut Ui, s: Selection, link: Link, room: Room) {
        let owner = room.owner == self.id();
        ui.horizontal_wrapped(|ui| {
            ui.heading(&room.name);
            if !link.online {
                ui.label(RichText::new("Host çevrimdışı").color(Color32::LIGHT_YELLOW))
                    .on_hover_text(&link.error);
            }
            if room.closed {
                ui.weak("Grup kapalı");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let archived = link.archived.contains(&room.id);
                if ui
                    .button(if archived {
                        "Arşivden çıkar"
                    } else {
                        "Arşivle"
                    })
                    .clicked()
                {
                    self.command(Command::Archive(s.clone(), !archived));
                }
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
        let messages: Vec<_> = link
            .messages
            .values()
            .filter(|m| m.room == room.id)
            .collect();
        let through = messages.last().map_or(0, |m| m.seq);
        if through > *link.seen.get(&room.id).unwrap_or(&0)
            && ui.ctx().input(|input| input.focused)
            && self.read_sent.as_ref() != Some(&(s.clone(), through))
        {
            self.read_sent = Some((s.clone(), through));
            self.command(Command::Read(s.clone(), through));
        }
        let pending: Vec<_> = link
            .queue
            .iter()
            .filter(|p| p.draft.room == room.id)
            .collect();
        let room_height =
            (ui.available_height() - if self.reply.is_some() { 144. } else { 94. }).max(90.);
        let bottom = self.jump.is_none();
        ScrollArea::vertical()
            .id_salt((&s.host, s.room))
            .max_height(room_height)
            .auto_shrink([false, false])
            .stick_to_bottom(bottom)
            .show(ui, |ui| {
                if let Some(first) = messages.first()
                    && ui.small_button("Önceki mesajlar").clicked()
                {
                    self.command(Command::History(s.clone(), first.seq));
                }
                for message in messages {
                    self.message(ui, message, &s);
                }
                for item in pending {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(&item.draft.text);
                        if let Some(error) = &item.error {
                            ui.label(RichText::new("Başarısız").color(Color32::LIGHT_RED))
                                .on_hover_text(error);
                            if ui.small_button("Yeniden dene").clicked() {
                                self.command(Command::Retry(s.clone(), item.draft.id));
                            }
                        } else {
                            ui.weak("Bekliyor");
                        }
                    });
                }
            });
        if room.active && !room.closed {
            if let Some((_, name, text)) = self.reply.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("Yanıtla · {name} · {text}"));
                    if ui
                        .small_button("×")
                        .on_hover_text("Alıntıyı kaldır")
                        .clicked()
                    {
                        self.reply = None;
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.draft)
                        .hint_text("Mesaj yaz…")
                        .desired_rows(2)
                        .desired_width((ui.available_width() - 94.).max(180.)),
                );
                if ui
                    .add_enabled(!self.draft.trim().is_empty(), egui::Button::new("Gönder"))
                    .clicked()
                {
                    let text = std::mem::take(&mut self.draft);
                    let reply = self.reply.take().map(|q| q.0);
                    self.command(Command::Send(s, text, reply));
                }
            });
        }
    }

    fn message(&mut self, ui: &mut Ui, m: &Message, s: &Selection) {
        let mine = m.author.data.key == self.id();
        let layout = if mine {
            Layout::right_to_left(Align::Min)
        } else {
            Layout::left_to_right(Align::Min)
        };
        let response = ui
            .with_layout(layout, |ui| {
                Frame::new()
                    .fill(if mine {
                        Color32::from_rgb(62, 42, 86)
                    } else {
                        Color32::from_rgb(39, 31, 51)
                    })
                    .corner_radius(12)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.set_max_width((ui.available_width() * 0.76).max(200.));
                            ui.horizontal(|ui| {
                                ui.strong(&m.author.data.name);
                                ui.weak(time(m.sent_at));
                            });
                            if let Some(q) = &m.reply
                                && ui
                                    .link(format!(
                                        "↪ {} · {}",
                                        q.author.data.name,
                                        short(&q.text, 100)
                                    ))
                                    .clicked()
                            {
                                self.jump = Some(q.id);
                                let cached = self.view.as_ref().is_some_and(|v| {
                                    v.local
                                        .hosts
                                        .get(&s.host)
                                        .is_some_and(|h| h.messages.contains_key(&q.id))
                                });
                                if !cached {
                                    self.command(Command::History(s.clone(), q.id + 1));
                                }
                            }
                            ui.label(&m.text);
                            if let Some(ticket) = &m.invitation {
                                if !mine
                                    && ui.small_button("Daveti aç").clicked()
                                    && let Ok(code) = pack("invite", ticket)
                                {
                                    self.command(Command::Import(code));
                                    self.page = Page::Contacts;
                                }
                            } else if ui.small_button("Yanıtla").clicked() {
                                self.reply =
                                    Some((m.seq, m.author.data.name.clone(), short(&m.text, 100)));
                            }
                        });
                    });
            })
            .response;
        if self.jump == Some(m.seq) {
            response.scroll_to_me(Some(Align::Center));
            self.jump = None;
        }
    }

    fn contacts(&mut self, ui: &mut Ui) {
        ui.heading("Kişiler ve istekler");
        if ui.button("Kişi kartımı kopyala").clicked()
            && let Some(code) = self
                .identity
                .as_ref()
                .and_then(|i| pack("card", &i.card).ok())
        {
            ui.ctx().copy_text(code);
        }
        field(
            ui,
            "Kişi kartı, davet veya host bağlantısı",
            &mut self.import,
            "ChatOid kodunu yapıştır",
        );
        if ui
            .add_enabled(
                !self.import.trim().is_empty(),
                egui::Button::new("İçe aktar"),
            )
            .clicked()
        {
            let code = std::mem::take(&mut self.import);
            self.command(Command::Import(code));
        }
        field(ui, "Kullanıcı adıyla ara", &mut self.search, "ör. @poyraz");
        let local = self
            .view
            .as_ref()
            .map(|v| v.local.clone())
            .unwrap_or_default();
        let mut query = self
            .search
            .trim_start_matches('@')
            .trim()
            .to_ascii_lowercase();
        if query.len() > 64 {
            query.clear();
        }
        for card in local
            .contacts
            .values()
            .filter(|c| c.data.handle.contains(&query))
        {
            ui.horizontal_wrapped(|ui| {
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
                if !local.already_requested(self.id(), &card.data.key)
                    && ui.button("Arkadaşlık isteği gönder").clicked()
                {
                    self.command(Command::Offer(card.clone()));
                }
            });
        }
        ui.separator();
        ui.heading("İstekler");
        for (pin, link) in &local.hosts {
            for request in &link.requests {
                self.request(ui, pin, request, &local);
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
            ui.strong(&person.data.name);
            ui.weak(format!(
                "{} · {status}",
                request.room_name.as_deref().unwrap_or("Arkadaşlık")
            ));
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
                    if ui.small_button("Kabul et").clicked() {
                        self.command(Command::Decide(pin.into(), Box::new(request.clone()), true));
                    }
                    if ui.small_button("Reddet").clicked() {
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
        ui.heading("Bu cihazın hostu");
        ui.label("Sohbet geçmişi bu cihazda tutulur. Hostu çalıştıran hesap verilere yerel olarak erişebilir.");
        field(
            ui,
            "Yerel ağ veya VPN adresi",
            &mut self.bind,
            "192.168.1.20:47821",
        );
        let address = self.view.as_ref().and_then(|v| v.host.clone());
        if let Some(address) = address {
            ui.label(format!("Çalışıyor · {}", address.socket));
            if ui.button("Host bağlantısını kopyala").clicked()
                && let Ok(code) = pack("host", &address)
            {
                ui.ctx().copy_text(code);
            }
            if ui.button("Hostu durdur").clicked() {
                self.command(Command::Stop);
            }
        } else if ui.button("Hostu başlat").clicked() {
            match self.bind.trim().parse() {
                Ok(bind) => self.command(Command::Start(bind)),
                Err(_) => self.notice = "IP adresi ve port gir, ör. 192.168.1.20:47821".into(),
            }
        }
        ui.separator();
        if let Some(view) = self.view.as_ref() {
            for link in view.local.hosts.values() {
                ui.label(format!(
                    "{} · {}",
                    link.address.socket,
                    if link.online {
                        "Bağlı"
                    } else {
                        "Host çevrimdışı"
                    }
                ))
                .on_hover_text(&link.error);
            }
        }
    }

    fn agent(&mut self, ui: &mut Ui) {
        let local = self
            .view
            .as_ref()
            .map(|v| v.local.clone())
            .unwrap_or_default();
        ui.heading("Ajan kimliği");
        if let Some(identity) = self.identity.as_ref() {
            ui.label(format!(
                "{}  @{}",
                identity.card.data.name, identity.card.data.handle
            ));
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
        ui.heading("Codex bağlantısı");
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
        ui.heading("Çalışma klasörü");
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
        ui.heading("Otomatik istek kararları");
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
    fn update(&mut self, ctx: &UiContext, _: &mut eframe::Frame) {
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
            return;
        }
        self.navigation(ctx);
        if self.identity.is_none() {
            ctx.request_repaint();
            return;
        }
        match self.page {
            Page::Chats | Page::Archive => self.inbox(ctx),
            page => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ScrollArea::vertical().show(ui, |ui| match page {
                        Page::Contacts => self.contacts(ui),
                        Page::Host => self.host(ui),
                        Page::Agent => self.agent(ui),
                        _ => unreachable!(),
                    });
                });
            }
        }
        ctx.request_repaint_after(Duration::from_millis(180));
    }
}

fn field(ui: &mut Ui, label: &str, value: &mut String, placeholder: &str) -> bool {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(placeholder)
            .desired_width(f32::INFINITY),
    )
    .changed()
}
fn nav(ui: &mut Ui, page: &mut Page, choice: Page, label: &str) {
    if ui.selectable_label(*page == choice, label).clicked() {
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
