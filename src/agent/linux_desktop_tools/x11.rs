use std::{
    fs, io,
    os::unix::{fs::MetadataExt, io::AsRawFd, net::UnixStream},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::{Connection, RequestConnection},
    protocol::{
        res::{self, ClientIdMask, ClientIdSpec},
        shape, xkb,
        xproto::{
            self, Atom, AtomEnum, ConnectionExt, EventMask, GrabMode, GrabStatus, ImageFormat,
            ImageOrder, KeyButMask, MapState, ModMask, VisualClass, Window, WindowClass,
        },
        xtest,
    },
    reexports::x11rb_protocol::xauth::get_auth,
    rust_connection::{DefaultStream, PollMode, RustConnection, Stream},
};

const MAX_WINDOWS: u32 = 512;
const MAX_WINDOW_DEPTH: usize = 24;
const FOCUS_WAIT: Duration = Duration::from_secs(12);
const MAX_CAPTURE_PIXELS: u32 = 16 * 1024 * 1024;

type Conn = RustConnection<LocalStream>;
pub(super) type Point = (u16, u16);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    display: Display,
    server: Process,
    window_manager: Window,
    window: Window,
    pub(super) owner: Process,
    client: u32,
    class: Vec<u8>,
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Process {
    pub(super) pid: u32,
    start: u64,
}

impl Process {
    fn read(pid: u32) -> Result<Self> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        // comm can contain spaces and parentheses. The start time is field 22.
        let (_, fields) = stat.rsplit_once(')').context("Süreç kimliği okunamadı")?;
        let start = fields
            .split_whitespace()
            .nth(19)
            .context("Süreç başlangıcı okunamadı")?
            .parse()?;
        Ok(Self { pid, start })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Display {
    number: u16,
    screen: usize,
}

impl Display {
    fn parse(value: &str) -> Option<Self> {
        let value = value.strip_prefix(':')?;
        let (number, screen) = value.split_once('.').unwrap_or((value, "0"));
        let digits =
            |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
        if !digits(number) || !digits(screen) {
            return None;
        }
        Some(Self {
            number: number.parse().ok()?,
            screen: screen.parse().ok()?,
        })
    }

    fn current() -> Option<Self> {
        if std::env::var("XDG_SESSION_TYPE").ok().as_deref() != Some("x11")
            || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|display| !display.is_empty())
            || std::env::var_os("WAYLAND_SOCKET").is_some()
        {
            return None;
        }
        Self::parse(&std::env::var("DISPLAY").ok()?)
    }
}

struct LocalStream(DefaultStream);

impl Stream for LocalStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let mut descriptor = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        if mode.readable() {
            descriptor.events |= libc::POLLIN;
        }
        if mode.writable() {
            descriptor.events |= libc::POLLOUT;
        }
        loop {
            // poll has a bounded wait so an unresponsive X server cannot stall a task.
            match unsafe { libc::poll(&mut descriptor, 1, 1500) } {
                0 => return Err(io::Error::from(io::ErrorKind::TimedOut)),
                result if result > 0 => return Ok(()),
                _ => {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        }
    }

    fn read(
        &self,
        buf: &mut [u8],
        fds: &mut Vec<x11rb::utils::RawFdContainer>,
    ) -> io::Result<usize> {
        self.0.read(buf, fds)
    }

    fn write(&self, buf: &[u8], fds: &mut Vec<x11rb::utils::RawFdContainer>) -> io::Result<usize> {
        self.0.write(buf, fds)
    }
}

fn open(display: Display) -> Result<(Conn, Process)> {
    let path = format!("/tmp/.X11-unix/X{}", display.number);
    let socket = UnixStream::connect(path)?;
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut size = size_of::<libc::ucred>() as libc::socklen_t;
    // SO_PEERCRED reports the local socket peer's kernel credentials.
    let status = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    ensure!(
        status == 0 && size as usize == size_of::<libc::ucred>(),
        "X11 sunucusu doğrulanamadı"
    );
    let uid = unsafe { libc::geteuid() };
    ensure!(
        credentials.pid > 0 && (credentials.uid == 0 || credentials.uid == uid),
        "X11 sunucusu yerel oturuma ait değil"
    );
    let server = Process::read(credentials.pid as u32)?;
    let name = fs::read_to_string(format!("/proc/{}/comm", server.pid))?;
    ensure!(
        matches!(name.trim(), "Xorg" | "Xorg.bin" | "X" | "Xephyr")
            || (cfg!(test) && name.trim() == "Xvfb"),
        "Pencere araçları yerel X11 oturumu gerektirir"
    );
    let (stream, (family, address)) = DefaultStream::from_unix_stream(socket)?;
    let (auth_name, auth_data) = get_auth(family, &address, display.number)?.unwrap_or_default();
    let connection = RustConnection::connect_to_stream_with_auth_info(
        LocalStream(stream),
        display.screen,
        auth_name,
        auth_data,
    )?;
    Ok((connection, server))
}

struct Atoms {
    clients: Atom,
    wm_check: Atom,
    active: Atom,
    name: Atom,
    utf8: Atom,
    wm_class: Atom,
}

impl Atoms {
    fn read(connection: &Conn) -> Result<Self> {
        let atom = |name: &str| -> Result<Atom> {
            let atom = connection.intern_atom(true, name.as_bytes())?.reply()?.atom;
            ensure!(
                atom != NONE,
                "X11 pencere yöneticisi bu özelliği desteklemiyor"
            );
            Ok(atom)
        };
        Ok(Self {
            clients: atom("_NET_CLIENT_LIST")?,
            wm_check: atom("_NET_SUPPORTING_WM_CHECK")?,
            active: atom("_NET_ACTIVE_WINDOW")?,
            name: atom("_NET_WM_NAME")?,
            utf8: atom("UTF8_STRING")?,
            wm_class: AtomEnum::WM_CLASS.into(),
        })
    }
}

pub(super) struct Desktop {
    connection: Conn,
    display: Display,
    server: Process,
    root: Window,
    window_manager: Window,
    atoms: Atoms,
}

impl Desktop {
    fn connect(display: Display) -> Result<Self> {
        let (connection, server) = open(display)?;
        let root = connection
            .setup()
            .roots
            .get(display.screen)
            .context("X11 ekranı bulunamadı")?
            .root;
        let atoms = Atoms::read(&connection)?;
        for name in [
            res::X11_EXTENSION_NAME,
            xtest::X11_EXTENSION_NAME,
            shape::X11_EXTENSION_NAME,
            xkb::X11_EXTENSION_NAME,
        ] {
            ensure!(
                connection.extension_information(name)?.is_some(),
                "X11 oturumu {name} özelliğini desteklemiyor"
            );
        }
        let res = res::query_version(&connection, 1, 2)?.reply()?;
        ensure!(
            (res.server_major, res.server_minor) >= (1, 2),
            "X11 süreç kimliğini doğrulayamıyor"
        );
        ensure!(
            xkb::use_extension(&connection, 1, 0)?.reply()?.supported,
            "X11 tuş düzeni doğrulanamıyor"
        );
        let mut desktop = Self {
            connection,
            display,
            server,
            root,
            atoms,
            window_manager: NONE,
        };
        desktop.window_manager = desktop.one_window(root, desktop.atoms.wm_check)?;
        ensure!(
            desktop.window_manager != NONE
                && desktop.one_window(desktop.window_manager, desktop.atoms.wm_check)?
                    == desktop.window_manager,
            "X11 pencere yöneticisi doğrulanamadı"
        );
        Ok(desktop)
    }

    fn property(
        &self,
        window: Window,
        atom: Atom,
        property_type: impl Into<Atom>,
        length: u32,
    ) -> Result<xproto::GetPropertyReply> {
        let reply = self
            .connection
            .get_property(false, window, atom, property_type, 0, length)?
            .reply()?;
        ensure!(reply.bytes_after == 0, "X11 pencere özelliği çok uzun");
        Ok(reply)
    }

    fn one_window(&self, window: Window, atom: Atom) -> Result<Window> {
        let reply = self.property(window, atom, AtomEnum::WINDOW, 1)?;
        let mut values = reply.value32().context("X11 pencere kimliği okunamadı")?;
        values.next().context("X11 pencere kimliği eksik")
    }

    fn clients(&self) -> Result<Vec<Window>> {
        let reply = self.property(self.root, self.atoms.clients, AtomEnum::WINDOW, MAX_WINDOWS)?;
        Ok(reply
            .value32()
            .context("X11 pencere listesi okunamadı")?
            .collect())
    }

    fn owner(&self, window: Window) -> Result<(Process, u32)> {
        let spec = ClientIdSpec {
            client: window,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        };
        let reply = res::query_client_ids(&self.connection, &[spec])?.reply()?;
        let id = reply
            .ids
            .iter()
            .find(|id| id.spec.mask == ClientIdMask::LOCAL_CLIENT_PID && id.value.len() == 1)
            .context("X11 pencere sahibi doğrulanamadı")?;
        let pid = id.value[0];
        ensure!(
            pid != std::process::id()
                && fs::metadata(format!("/proc/{pid}"))?.uid() == unsafe { libc::geteuid() },
            "Pencere başka bir kullanıcıya veya ChatOid’e ait"
        );
        Ok((Process::read(pid)?, id.spec.client))
    }

    fn identify(&self, window: Window) -> Result<Target> {
        let attrs = self.connection.get_window_attributes(window)?.reply()?;
        ensure!(
            attrs.class == WindowClass::INPUT_OUTPUT
                && attrs.map_state == MapState::VIEWABLE
                && !attrs.override_redirect,
            "X11 penceresi görünür değil"
        );
        let (owner, client) = self.owner(window)?;
        let class = self
            .property(window, self.atoms.wm_class, AtomEnum::STRING, 128)?
            .value;
        ensure!(!class.is_empty(), "X11 pencere sınıfı doğrulanamadı");
        let name = self
            .property(window, self.atoms.name, self.atoms.utf8, 1024)?
            .value;
        let name = if name.is_empty() {
            self.property(window, AtomEnum::WM_NAME.into(), AtomEnum::STRING, 1024)?
                .value
        } else {
            name
        };
        let title: String = String::from_utf8_lossy(&name)
            .chars()
            .filter(|ch| !ch.is_control())
            .take(240)
            .collect();
        ensure!(!title.trim().is_empty(), "X11 pencere başlığı eksik");
        Ok(Target {
            display: self.display,
            server: self.server,
            window_manager: self.window_manager,
            window,
            owner,
            client,
            class,
            title,
        })
    }

    pub(super) fn verify(&self, target: &Target) -> Result<Target> {
        ensure!(
            target.display == self.display
                && target.server == self.server
                && target.window_manager == self.window_manager
                && self.one_window(self.root, self.atoms.wm_check)? == self.window_manager
                && self.clients()?.contains(&target.window),
            "Seçilen pencere artık açık değil"
        );
        let current = self.identify(target.window)?;
        ensure!(
            current.owner == target.owner
                && current.client == target.client
                && current.class == target.class,
            "Seçilen pencerenin kimliği değişti"
        );
        Ok(current)
    }

    fn belongs_to(&self, mut window: Window, target: Window) -> Result<bool> {
        for _ in 0..MAX_WINDOW_DEPTH {
            if window == target {
                return Ok(true);
            }
            if window <= 1 || window == self.root {
                return Ok(false);
            }
            window = self.connection.query_tree(window)?.reply()?.parent;
        }
        Ok(false)
    }

    pub(super) fn verify_foreground(&self, target: &Target) -> Result<Target> {
        let current = self.verify(target)?;
        ensure!(self.in_foreground(target)?, "Seçilen pencere odakta değil");
        Ok(current)
    }

    fn in_foreground(&self, target: &Target) -> Result<bool> {
        let active = self
            .one_window(self.root, self.atoms.active)
            .unwrap_or(NONE);
        if active != target.window {
            return Ok(false);
        }
        let focus = self.connection.get_input_focus()?.reply()?.focus;
        self.belongs_to(focus, target.window)
    }

    pub(super) fn wait_for_foreground(
        target: &Target,
        approved_title: &str,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        let desktop = Self::connect(target.display)?;
        let deadline = Instant::now() + FOCUS_WAIT;
        loop {
            ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
            let current = desktop.verify(target)?;
            ensure!(
                current.title == approved_title,
                "Seçilen pencerenin başlığı değişti"
            );
            if desktop.in_foreground(target)? {
                return Ok(desktop);
            }
            ensure!(
                Instant::now() < deadline,
                "Onaydan sonra seçilen pencere odaklanmadı"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub(super) fn bounds(&self, target: &Target) -> Result<Bounds> {
        self.window_bounds(target.window)
    }

    fn window_bounds(&self, window: Window) -> Result<Bounds> {
        let geometry = self.connection.get_geometry(window)?.reply()?;
        let origin = self
            .connection
            .translate_coordinates(window, self.root, 0, 0)?
            .reply()?;
        ensure!(
            geometry.root == self.root && origin.same_screen,
            "Pencere başka bir ekranda"
        );
        Ok(Bounds {
            x: i32::from(origin.dst_x),
            y: i32::from(origin.dst_y),
            width: i32::from(geometry.width),
            height: i32::from(geometry.height),
        })
    }

    fn screen_bounds(&self) -> Result<Bounds> {
        // The root geometry can change while the connection is open.
        self.window_bounds(self.root)
    }

    fn ensure_unobscured(&self, target: &Target, bounds: Bounds) -> Result<()> {
        ensure!(
            self.screen_bounds()?.contains(bounds),
            "Pencerenin tamamı ekranda görünmeli"
        );
        let shape = shape::query_extents(&self.connection, target.window)?.reply()?;
        ensure!(
            !shape.bounding_shaped && !shape.clip_shaped,
            "Şekilli X11 penceresi yakalanamıyor"
        );
        let mut window = target.window;
        for _ in 0..MAX_WINDOW_DEPTH {
            let parent = self.connection.query_tree(window)?.reply()?.parent;
            let tree = self.connection.query_tree(parent)?.reply()?;
            ensure!(
                tree.children.len() <= MAX_WINDOWS as usize,
                "Pencere görünürlüğü doğrulanamadı"
            );
            let position = tree
                .children
                .iter()
                .position(|&child| child == window)
                .context("Pencere yığını doğrulanamadı")?;
            for &above in &tree.children[position + 1..] {
                let attrs = self.connection.get_window_attributes(above)?.reply()?;
                if attrs.class == WindowClass::INPUT_OUTPUT && attrs.map_state == MapState::VIEWABLE
                {
                    let geometry = self.connection.get_geometry(above)?.reply()?;
                    let expanded = self
                        .window_bounds(above)?
                        .with_border(i32::from(geometry.border_width));
                    ensure!(
                        !expanded.intersects(bounds),
                        "Seçilen pencerenin üzeri örtülü"
                    );
                }
            }
            if parent == self.root {
                return Ok(());
            }
            ensure!(
                self.window_bounds(parent)?.contains(bounds),
                "Seçilen pencerenin bir bölümü kırpılmış"
            );
            window = parent;
        }
        bail!("Pencere görünürlüğü doğrulanamadı")
    }

    fn grab(&self) -> Result<ServerGrab<'_>> {
        self.connection.grab_server()?.check()?;
        Ok(ServerGrab(&self.connection))
    }

    pub(super) fn capture(
        &self,
        target: &Target,
        approved_title: &str,
        cancelled: &AtomicBool,
    ) -> Result<image::RgbImage> {
        let grab = self.grab()?;
        self.check_approved(target, approved_title, cancelled)?;
        let bounds = self.bounds(target)?;
        self.ensure_unobscured(target, bounds)?;
        let (width, height) = (u16::try_from(bounds.width)?, u16::try_from(bounds.height)?);
        ensure!(
            width > 0 && height > 0 && u32::from(width) * u32::from(height) <= MAX_CAPTURE_PIXELS,
            "Pencere görüntüsü çok büyük veya boş"
        );
        let reply = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                target.window,
                0,
                0,
                width,
                height,
                u32::MAX,
            )?
            .reply()?;
        drop(grab);
        self.decode_image(width, height, reply)
    }

    fn decode_image(
        &self,
        width: u16,
        height: u16,
        reply: xproto::GetImageReply,
    ) -> Result<image::RgbImage> {
        let setup = self.connection.setup();
        let visual = setup
            .roots
            .get(self.display.screen)
            .context("X11 ekranı bulunamadı")?
            .allowed_depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| {
                visual.visual_id == reply.visual && visual.class == VisualClass::TRUE_COLOR
            })
            .context("X11 pencere renk biçimi desteklenmiyor")?;
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == reply.depth)
            .context("X11 görüntü biçimi desteklenmiyor")?;
        let pixel_bytes = usize::from(format.bits_per_pixel / 8);
        let alignment = usize::from(format.scanline_pad / 8);
        ensure!(
            matches!(pixel_bytes, 2..=4) && alignment > 0,
            "X11 piksel biçimi desteklenmiyor"
        );
        let stride = (usize::from(width) * pixel_bytes).div_ceil(alignment) * alignment;
        let length = stride * usize::from(height);
        ensure!(
            reply.data.len() >= length && reply.data.len() < length + 4,
            "X11 piksel verisi eksik"
        );
        let mut image = image::RgbImage::new(width.into(), height.into());
        let masks = [visual.red_mask, visual.green_mask, visual.blue_mask];
        ensure!(
            masks.iter().all(|&mask| mask != 0),
            "X11 renk maskesi geçersiz"
        );
        for (row, dest) in reply.data[..length]
            .chunks_exact(stride)
            .zip(image.rows_mut())
        {
            for (source, pixel) in row.chunks_exact(pixel_bytes).zip(dest) {
                let mut bytes = [0; 4];
                let value = if setup.image_byte_order == ImageOrder::LSB_FIRST {
                    bytes[..pixel_bytes].copy_from_slice(source);
                    u32::from_le_bytes(bytes)
                } else {
                    bytes[4 - pixel_bytes..].copy_from_slice(source);
                    u32::from_be_bytes(bytes)
                };
                *pixel = image::Rgb(masks.map(|mask| {
                    let shift = mask.trailing_zeros();
                    ((u64::from((value & mask) >> shift) * 255) / u64::from(mask >> shift)) as u8
                }));
            }
        }
        Ok(image)
    }

    pub(super) fn check_approved(
        &self,
        target: &Target,
        approved_title: &str,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
        ensure!(
            self.verify_foreground(target)?.title == approved_title,
            "Seçilen pencerenin başlığı değişti"
        );
        Ok(())
    }

    pub(super) fn point_is_visible(&self, target: &Target, rect: Bounds) -> Result<bool> {
        let position = (
            i16::try_from(rect.x + rect.width / 2)?,
            i16::try_from(rect.y + rect.height / 2)?,
        );
        Ok(self.screen_bounds()?.contains_point(position) && self.hit(target, position).is_ok())
    }

    fn hit(&self, target: &Target, position: (i16, i16)) -> Result<()> {
        let mut window = self.root;
        for _ in 0..MAX_WINDOW_DEPTH {
            if window == target.window {
                return Ok(());
            }
            let next = self
                .connection
                .translate_coordinates(self.root, window, position.0, position.1)?
                .reply()?;
            if !next.same_screen || next.child == NONE {
                break;
            }
            window = next.child;
        }
        bail!("Seçilen nokta pencerenin görünür alanında değil")
    }

    fn idle_input(&self, target: &Target, keyboard: bool) -> Result<()> {
        // Try a temporary grab under the server grab to detect an existing active grab.
        let connection = &self.connection;
        let checked = if keyboard {
            let reply = connection
                .grab_keyboard(
                    false,
                    target.window,
                    CURRENT_TIME,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?
                .reply()?;
            if reply.status == GrabStatus::SUCCESS {
                connection.ungrab_keyboard(CURRENT_TIME)?.check()?;
            }
            reply.status
        } else {
            let reply = connection
                .grab_pointer(
                    false,
                    target.window,
                    EventMask::NO_EVENT,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    NONE,
                    NONE,
                    CURRENT_TIME,
                )?
                .reply()?;
            if reply.status == GrabStatus::SUCCESS {
                connection.ungrab_pointer(CURRENT_TIME)?.check()?;
            }
            reply.status
        };
        ensure!(
            checked == GrabStatus::SUCCESS,
            "X11 girişi başka bir uygulama tarafından tutuluyor"
        );
        let pointer = connection.query_pointer(self.root)?.reply()?;
        let buttons = KeyButMask::BUTTON1
            | KeyButMask::BUTTON2
            | KeyButMask::BUTTON3
            | KeyButMask::BUTTON4
            | KeyButMask::BUTTON5;
        ensure!(
            pointer.same_screen
                && u16::from(pointer.mask & buttons) == 0
                && connection
                    .query_keymap()?
                    .reply()?
                    .keys
                    .iter()
                    .all(|&state| state == 0),
            "Pencere eylemi için fare ve klavye tuşlarını bırak"
        );
        Ok(())
    }

    fn fake(&self, event: u8, detail: u8, position: (i16, i16)) -> Result<()> {
        xtest::fake_input(
            &self.connection,
            event,
            detail,
            CURRENT_TIME,
            self.root,
            position.0,
            position.1,
            0,
        )?
        .check()?;
        Ok(())
    }

    pub(super) fn pointer(
        &self,
        target: &Target,
        approved_title: &str,
        cancelled: &AtomicBool,
        action: PointerAction,
    ) -> Result<()> {
        let _grab = self.grab()?;
        self.check_approved(target, approved_title, cancelled)?;
        let bounds = self.bounds(target)?;
        let screen = self.screen_bounds()?;
        let resolve = |point| -> Result<(i16, i16)> {
            let position = bounds.point(point)?;
            ensure!(
                screen.contains_point(position),
                "Seçilen nokta ekran dışında"
            );
            self.hit(target, position)?;
            Ok(position)
        };

        let start = resolve(action.start())?;
        let path = match action {
            PointerAction::Drag(_, to) => {
                let end = bounds.point(to)?;
                let xd = i32::from(end.0) - i32::from(start.0);
                let yd = i32::from(end.1) - i32::from(start.1);
                let length = xd.abs().max(yd.abs());
                ensure!((1..=4096).contains(&length), "Sürükleme uzaklığı geçersiz");
                // Reject occlusion conservatively so all intermediate points are in the window.
                self.ensure_unobscured(target, bounds)?;
                (1..=length.min(24))
                    .map(|i| {
                        let steps = length.min(24);
                        (
                            start.0 + (xd * i / steps) as i16,
                            start.1 + (yd * i / steps) as i16,
                        )
                    })
                    .collect::<Vec<_>>()
            }
            _ => Vec::new(),
        };
        for position in &path {
            self.hit(target, *position)?;
        }
        self.idle_input(target, false)?;
        self.check_approved(target, approved_title, cancelled)?;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, start)?;
        let actual = self.connection.query_pointer(self.root)?.reply()?;
        ensure!(
            (actual.root_x, actual.root_y) == start,
            "İmleç seçilen noktada değil"
        );
        let click = |button| -> Result<()> {
            self.fake(xproto::BUTTON_PRESS_EVENT, button, (0, 0))?;
            self.fake(xproto::BUTTON_RELEASE_EVENT, button, (0, 0))
        };
        match action {
            PointerAction::Hover(_) => {}
            PointerAction::Click(_, button, count) => {
                for _ in 0..count {
                    click(button)?;
                }
            }
            PointerAction::Scroll(_, steps) => {
                let button = if steps > 0 { 5 } else { 4 };
                for _ in 0..steps.unsigned_abs() {
                    click(button)?;
                }
            }
            PointerAction::Drag(_, _) => {
                self.fake(xproto::BUTTON_PRESS_EVENT, 1, (0, 0))?;
                let motion = path
                    .into_iter()
                    .try_for_each(|position| self.fake(xproto::MOTION_NOTIFY_EVENT, 0, position));
                let release = self.fake(xproto::BUTTON_RELEASE_EVENT, 1, (0, 0));
                motion?;
                release?;
            }
        }
        Ok(())
    }

    pub(super) fn keyboard(
        &self,
        target: &Target,
        approved_title: &str,
        cancelled: &AtomicBool,
        action: KeyboardAction<'_>,
    ) -> Result<()> {
        let _grab = self.grab()?;
        self.check_approved(target, approved_title, cancelled)?;
        self.idle_input(target, true)?;
        let state = xkb::get_state(&self.connection, u16::from(xkb::ID::USE_CORE_KBD))?.reply()?;
        ensure!(
            u8::from(state.group) == 0 && (u16::from(state.mods) & !u16::from(ModMask::M2)) == 0,
            "Etkin X11 klavye düzeni veya değiştirici desteklenmiyor"
        );
        let keys = Keymap::read(&self.connection)?;
        let presses = match action {
            KeyboardAction::Key(key) => vec![keys.resolve(key, false)?],
            KeyboardAction::Text(text) => text
                .chars()
                .map(|character| {
                    let symbol = u32::from(character);
                    let symbol = if symbol > 0xff {
                        0x0100_0000 | symbol
                    } else {
                        symbol
                    };
                    keys.resolve(
                        Key {
                            symbol,
                            modifier: None,
                        },
                        true,
                    )
                    .with_context(|| format!("Etkin klavye düzeninde {character:?} yazılamıyor"))
                })
                .collect::<Result<Vec<_>>>()?,
        };
        for (key, modifier) in presses {
            self.check_approved(target, approved_title, cancelled)?;
            if let Some(modifier) = modifier {
                self.fake(xproto::KEY_PRESS_EVENT, modifier, (0, 0))?;
            }
            let result = self
                .fake(xproto::KEY_PRESS_EVENT, key, (0, 0))
                .and_then(|()| self.fake(xproto::KEY_RELEASE_EVENT, key, (0, 0)));
            let cleanup = modifier
                .map(|code| self.fake(xproto::KEY_RELEASE_EVENT, code, (0, 0)))
                .transpose();
            result?;
            cleanup?;
        }
        Ok(())
    }
}

struct ServerGrab<'a>(&'a Conn);

impl Drop for ServerGrab<'_> {
    fn drop(&mut self) {
        let _ = self.0.ungrab_server();
        let _ = self.0.flush();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Bounds {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: i32,
    pub(super) height: i32,
}

impl Bounds {
    pub(super) fn contains(self, other: Self) -> bool {
        other.width > 0
            && other.height > 0
            && self.x <= other.x
            && self.y <= other.y
            && self.x + self.width >= other.x + other.width
            && self.y + self.height >= other.y + other.height
    }

    fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    fn with_border(self, border: i32) -> Self {
        Self {
            x: self.x - border,
            y: self.y - border,
            width: self.width + border * 2,
            height: self.height + border * 2,
        }
    }

    fn contains_point(self, (x, y): (i16, i16)) -> bool {
        self.contains(Self {
            x: i32::from(x),
            y: i32::from(y),
            width: 1,
            height: 1,
        })
    }

    fn point(self, (x, y): Point) -> Result<(i16, i16)> {
        ensure!(self.width > 0 && self.height > 0, "Pencere alanı boş");
        Ok((
            i16::try_from(self.x + (self.width - 1) * i32::from(x) / 1000)?,
            i16::try_from(self.y + (self.height - 1) * i32::from(y) / 1000)?,
        ))
    }
}

#[derive(Clone, Copy)]
pub(super) enum PointerAction {
    Click(Point, u8, u8),
    Scroll(Point, i8),
    Hover(Point),
    Drag(Point, Point),
}

impl PointerAction {
    fn start(self) -> Point {
        match self {
            Self::Click(point, ..)
            | Self::Scroll(point, ..)
            | Self::Hover(point)
            | Self::Drag(point, _) => point,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum KeyboardAction<'a> {
    Key(Key),
    Text(&'a str),
}

#[derive(Clone, Copy)]
pub(super) struct Key {
    symbol: u32,
    modifier: Option<u32>,
}

impl Key {
    pub(super) fn named(name: &str) -> Option<Self> {
        // X11 keysyms from X11/keysymdef.h.
        let (symbol, modifier) = match name {
            "Tab" => (0xff09, None),
            "Enter" => (0xff0d, None),
            "Escape" => (0xff1b, None),
            "Space" => (0x20, None),
            "Backspace" => (0xff08, None),
            "ArrowUp" => (0xff52, None),
            "ArrowDown" => (0xff54, None),
            "ArrowLeft" => (0xff51, None),
            "ArrowRight" => (0xff53, None),
            "Home" => (0xff50, None),
            "End" => (0xff57, None),
            "PageUp" => (0xff55, None),
            "PageDown" => (0xff56, None),
            "Shift+Tab" => (0xff09, Some(0xffe1)),
            "Ctrl+A" => (u32::from(b'a'), Some(0xffe3)),
            "Ctrl+F" => (u32::from(b'f'), Some(0xffe3)),
            "Ctrl+S" => (u32::from(b's'), Some(0xffe3)),
            "Ctrl+Z" => (u32::from(b'z'), Some(0xffe3)),
            "Ctrl+Y" => (u32::from(b'y'), Some(0xffe3)),
            _ => return None,
        };
        Some(Self { symbol, modifier })
    }
}

struct Keymap {
    first: u8,
    per_key: usize,
    symbols: Vec<u32>,
    modifiers: xproto::GetModifierMappingReply,
}

impl Keymap {
    fn read(connection: &Conn) -> Result<Self> {
        let setup = connection.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - first + 1;
        let mapping = connection.get_keyboard_mapping(first, count)?.reply()?;
        ensure!(mapping.keysyms_per_keycode > 0, "X11 tuş eşlemesi eksik");
        Ok(Self {
            first,
            per_key: usize::from(mapping.keysyms_per_keycode),
            symbols: mapping.keysyms,
            modifiers: connection.get_modifier_mapping()?.reply()?,
        })
    }

    fn resolve(&self, key: Key, allow_shift: bool) -> Result<(u8, Option<u8>)> {
        let rows = || self.symbols.chunks_exact(self.per_key);
        let code = |i| self.first + i as u8;
        let lookup = |symbol| rows().position(|row| row[0] == symbol).map(code);
        let modifier = |symbol| -> Result<u8> {
            let per = usize::from(self.modifiers.keycodes_per_modifier());
            let index = if symbol == 0xffe1 { 0 } else { 2 };
            let code = lookup(symbol).context("X11 değiştirici tuşu eşlenemedi")?;
            ensure!(
                self.modifiers.keycodes[index * per..(index + 1) * per].contains(&code),
                "X11 değiştirici tuşu doğrulanamadı"
            );
            Ok(code)
        };
        if let Some(code) = lookup(key.symbol) {
            return Ok((code, key.modifier.map(modifier).transpose()?));
        }
        if allow_shift
            && let Some(code) = rows()
                .position(|row| row.get(1) == Some(&key.symbol))
                .map(code)
        {
            return Ok((code, Some(modifier(0xffe1)?)));
        }
        bail!("Tuş etkin X11 klavye düzeninde eşlenemedi")
    }
}

impl Target {
    pub(in crate::agent) fn checked(&self) -> Result<Self> {
        ensure!(
            Display::current() == Some(self.display),
            "Seçilen X11 oturumu değişti"
        );
        Desktop::connect(self.display)?.verify(self)
    }
}

pub fn is_native_x11_session() -> bool {
    Display::current().is_some()
}

pub fn window_support_available() -> bool {
    Display::current().is_some_and(|display| Desktop::connect(display).is_ok())
}

pub fn windows() -> Vec<Target> {
    let Some(display) = Display::current() else {
        return Vec::new();
    };
    let Ok(desktop) = Desktop::connect(display) else {
        return Vec::new();
    };
    desktop
        .clients()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|window| desktop.identify(window).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    #[test]
    fn window_display_requires_a_local_socket_address() {
        assert_eq!(
            Display::parse(":42.1"),
            Some(Display {
                number: 42,
                screen: 1
            })
        );
        for rejected in [
            "localhost:0",
            "example.com:0",
            "unix:0",
            ":0.",
            ":0.1.2",
            ":+0",
        ] {
            assert_eq!(Display::parse(rejected), None, "{rejected}");
        }
    }

    struct Fixture(Child);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Run with xvfb-run -a and XDG_SESSION_TYPE=x11. Refuses to use a real desktop.
    #[test]
    #[ignore = "requires an isolated Xvfb display with xmessage"]
    fn x11_isolated_window_capture_and_input() -> Result<()> {
        let display = Display::current().context("Set XDG_SESSION_TYPE=x11 under xvfb-run")?;
        let (connection, server) = open(display)?;
        let server_name = fs::read_to_string(format!("/proc/{}/comm", server.pid))?;
        ensure!(
            server_name.trim() == "Xvfb",
            "Fixture requires an isolated Xvfb server"
        );
        let screen = connection
            .setup()
            .roots
            .get(display.screen)
            .context("Fixture screen missing")?;
        let root = screen.root;
        let atom = |name: &str| -> Result<Atom> {
            Ok(connection
                .intern_atom(false, name.as_bytes())?
                .reply()?
                .atom)
        };
        let clients = atom("_NET_CLIENT_LIST")?;
        let wm_check = atom("_NET_SUPPORTING_WM_CHECK")?;
        let active = atom("_NET_ACTIVE_WINDOW")?;
        atom("_NET_WM_NAME")?;
        atom("UTF8_STRING")?;
        let make_window = |x, y, width, height| -> Result<Window> {
            let id = connection.generate_id()?;
            connection
                .create_window(
                    screen.root_depth,
                    id,
                    root,
                    x,
                    y,
                    width,
                    height,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    screen.root_visual,
                    &xproto::CreateWindowAux::new().background_pixel(screen.black_pixel),
                )?
                .check()?;
            Ok(id)
        };
        let set_window = |window, property, values: &[Window]| -> Result<()> {
            use x11rb::wrapper::ConnectionExt as _;
            connection
                .change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    property,
                    AtomEnum::WINDOW,
                    values,
                )?
                .check()?;
            Ok(())
        };
        let manager = make_window(0, 0, 1, 1)?;
        set_window(root, wm_check, &[manager])?;
        set_window(manager, wm_check, &[manager])?;
        let _fixture = Fixture(
            Command::new("xmessage")
                .args([
                    "-geometry",
                    "240x120+40+40",
                    "-title",
                    "ChatOid fixture",
                    "Selected window",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let desktop = Desktop::connect(display)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        let target = loop {
            let tree = connection.query_tree(root)?.reply()?;
            if let Some(target) = tree
                .children
                .into_iter()
                .filter_map(|window| desktop.identify(window).ok())
                .find(|target| target.title == "ChatOid fixture")
            {
                break target;
            }
            ensure!(Instant::now() < deadline, "Fixture window did not appear");
            thread::sleep(Duration::from_millis(20));
        };
        set_window(root, clients, &[target.window])?;
        assert_eq!(windows(), vec![target.clone()]);
        let cancelled = AtomicBool::new(false);
        let title = &target.title;
        set_window(root, active, &[manager])?;
        connection
            .set_input_focus(xproto::InputFocus::PARENT, target.window, CURRENT_TIME)?
            .check()?;
        assert!(desktop.capture(&target, title, &cancelled).is_err());
        assert!(
            desktop
                .pointer(
                    &target,
                    title,
                    &cancelled,
                    PointerAction::Click((500, 500), 1, 1)
                )
                .is_err()
        );
        set_window(root, active, &[target.window])?;
        let image = desktop.capture(&target, title, &cancelled)?;
        assert_eq!(image.dimensions(), (240, 120));
        assert!(image.pixels().any(|pixel| pixel.0 != [0; 3]));
        desktop.pointer(&target, title, &cancelled, PointerAction::Hover((500, 500)))?;
        let position = desktop.bounds(&target)?.point((500, 500))?;
        let pointer = connection.query_pointer(root)?.reply()?;
        assert_eq!((pointer.root_x, pointer.root_y), position);

        let overlay = make_window(position.0 - 5, position.1 - 5, 10, 10)?;
        connection.map_window(overlay)?.check()?;
        assert!(desktop.capture(&target, title, &cancelled).is_err());
        assert!(
            desktop
                .pointer(
                    &target,
                    title,
                    &cancelled,
                    PointerAction::Click((500, 500), 1, 1)
                )
                .is_err()
        );
        connection.destroy_window(overlay)?.check()?;
        desktop.keyboard(&target, title, &cancelled, KeyboardAction::Text("Ab"))?;
        desktop.pointer(
            &target,
            title,
            &cancelled,
            PointerAction::Drag((300, 250), (800, 250)),
        )?;

        cancelled.store(true, Ordering::Release);
        assert!(
            desktop
                .pointer(&target, title, &cancelled, PointerAction::Hover((500, 500)))
                .is_err()
        );
        cancelled.store(false, Ordering::Release);
        set_window(root, clients, &[])?;
        assert!(desktop.capture(&target, title, &cancelled).is_err());
        Ok(())
    }
}
