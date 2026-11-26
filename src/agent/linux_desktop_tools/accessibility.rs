use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zbus::{Connection, Proxy, connection::Builder, zvariant::OwnedObjectPath};

use super::super::x11::{Bounds, Desktop, Target};

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const COMPONENT: &str = "org.a11y.atspi.Component";
const ACTION: &str = "org.a11y.atspi.Action";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const MAX_APPS: usize = 128;
const MAX_NODES: usize = 420;
const MAX_DEPTH: u8 = 14;
const MAX_CONTROLS: usize = 80;
const SCREEN: u32 = 0;

type Node = (String, OwnedObjectPath);

pub(super) struct Controls {
    pub(super) data: Value,
    pub(super) buttons: BTreeMap<String, String>,
}

struct Access<'a> {
    desktop: &'a Desktop,
    target: &'a Target,
    title: &'a str,
    cancelled: &'a AtomicBool,
}

impl Access<'_> {
    fn checked(&self) -> Result<()> {
        self.desktop
            .check_approved(self.target, self.title, self.cancelled)
    }

    async fn connection(&self) -> Result<Connection> {
        self.checked()?;
        let session = Connection::session().await?;
        let bus = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus").await?;
        let address: String = bus.call("GetAddress", &()).await?;
        ensure!(
            address.starts_with("unix:") && !address.contains(';'),
            "AT-SPI veriyolu yerel değil"
        );
        Ok(Builder::address(address.as_str())?.build().await?)
    }

    async fn window(&self, connection: &Connection, bounds: Bounds) -> Result<Node> {
        let registry = Proxy::new(connection, "org.a11y.atspi.Registry", ROOT, ACCESSIBLE).await?;
        let apps = children(&registry).await?;
        ensure!(apps.len() <= MAX_APPS, "AT-SPI uygulama listesi çok uzun");
        let dbus = Proxy::new(
            connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        let mut selected = None;
        for app in apps {
            if !app.0.starts_with(':') {
                continue;
            }
            let pid: zbus::Result<u32> = dbus
                .call("GetConnectionUnixProcessID", &(app.0.as_str(),))
                .await;
            if pid.ok() != Some(self.target.owner.pid) {
                continue;
            }
            let top_levels = children(&proxy(connection, &app, ACCESSIBLE).await?).await?;
            ensure!(
                top_levels.len() <= MAX_APPS,
                "AT-SPI pencere listesi çok uzun"
            );
            for candidate in top_levels {
                let Some(candidate) = same_application(candidate, &app.0) else {
                    continue;
                };
                let accessible = proxy(connection, &candidate, ACCESSIBLE).await?;
                let role: u32 = accessible.call("GetRole", &()).await?;
                if !matches!(role, 16 | 23 | 69) {
                    continue;
                }
                let name: String = accessible.get_property("Name").await?;
                if name != self.title || !visible(&state(&accessible).await?) {
                    continue;
                }
                if let Ok(extents) = extents(connection, &candidate).await
                    && matches_bounds(extents, bounds)
                {
                    ensure!(selected.is_none(), "AT-SPI pencere kimliği belirsiz");
                    selected = Some(candidate);
                }
            }
        }
        selected.context("Seçilen pencerenin AT-SPI denetim ağacı doğrulanamadı")
    }

    async fn controls(&self, connection: &Connection, wanted: Option<&str>) -> Result<Controls> {
        let bounds = self.desktop.bounds(self.target)?;
        let window = self.window(connection, bounds).await?;
        let mut queue = VecDeque::from([(window.clone(), 0)]);
        let mut visited = HashSet::new();
        let mut data = Vec::new();
        let mut buttons = BTreeMap::new();
        while let Some((node, depth)) = queue.pop_front() {
            ensure!(!self.cancelled.load(Ordering::Acquire), "Görev durduruldu");
            if visited.len() >= MAX_NODES || data.len() >= MAX_CONTROLS {
                break;
            }
            if node.0 != window.0 || !visited.insert(node.clone()) {
                continue;
            }
            let accessible = proxy(connection, &node, ACCESSIBLE).await?;
            let status = state(&accessible).await?;
            let role: u32 = accessible.call("GetRole", &()).await?;
            // Avoid reading names or descending into fields that can expose protected text.
            if !visible(&status) || has(&status, 7) || matches!(role, 12 | 40 | 52 | 60 | 61 | 79) {
                continue;
            }
            if let Some(kind) = control_kind(role)
                && let Ok(rect) = extents(connection, &node).await
            {
                let coordinate = center(rect, bounds);
                if let Some((x, y)) = coordinate {
                    let name: String = accessible.get_property("Name").await?;
                    let label = label(&name);
                    if !label.is_empty() && self.desktop.point_is_visible(self.target, rect)? {
                        let mut entry = json!({"label":label,"type":kind,"x":x,"y":y});
                        if role == 43
                            && has(&status, 8)
                            && has(&status, 24)
                            && let Some(index) = default_action(connection, &node).await
                        {
                            let id = button_id(
                                &node,
                                &label,
                                rect,
                                index,
                                self.title,
                                self.target.owner.pid,
                            );
                            if wanted == Some(id.as_str()) {
                                let action = proxy(connection, &node, ACTION).await?;
                                self.checked()?;
                                ensure!(
                                    self.desktop.bounds(self.target)? == bounds,
                                    "Seçilen pencerenin konumu değişti"
                                );
                                let done: bool = action.call("DoAction", &(index,)).await?;
                                ensure!(done, "AT-SPI düğme eylemini uygulamadı");
                                return Ok(Controls {
                                    data: Value::Null,
                                    buttons,
                                });
                            }
                            buttons.insert(id.clone(), label);
                            entry["button_id"] = id.into();
                        }
                        data.push(entry);
                    }
                }
            }
            if depth < MAX_DEPTH && !has(&status, 31) {
                let remaining = MAX_NODES.saturating_sub(visited.len() + queue.len());
                if remaining > 0 {
                    queue.extend(
                        children(&accessible)
                            .await?
                            .into_iter()
                            .filter_map(|child| same_application(child, &window.0))
                            .take(remaining)
                            .map(|child| (child, depth + 1)),
                    );
                }
            }
        }
        self.checked()?;
        ensure!(
            self.desktop.bounds(self.target)? == bounds,
            "Seçilen pencerenin konumu değişti"
        );
        ensure!(wanted.is_none(), "Düğme değişti veya görünür değil");
        Ok(Controls {
            data: json!({"controls":data}),
            buttons,
        })
    }
}

async fn proxy<'a>(
    connection: &Connection,
    node: &'a Node,
    interface: &'static str,
) -> Result<Proxy<'a>> {
    Ok(Proxy::new(connection, node.0.as_str(), node.1.as_str(), interface).await?)
}

async fn children(node: &Proxy<'_>) -> Result<Vec<Node>> {
    Ok(node.call("GetChildren", &()).await?)
}

fn same_application(mut node: Node, application: &str) -> Option<Node> {
    if node.0.is_empty() {
        node.0 = application.into();
    }
    (node.0 == application && node.1.as_str() != "/org/a11y/atspi/null").then_some(node)
}

async fn state(node: &Proxy<'_>) -> Result<Vec<u32>> {
    Ok(node.call("GetState", &()).await?)
}

fn has(state: &[u32], bit: usize) -> bool {
    state
        .get(bit / 32)
        .is_some_and(|word| word & (1 << (bit % 32)) != 0)
}

fn visible(state: &[u32]) -> bool {
    has(state, 25) && has(state, 30) && !has(state, 6) && !has(state, 15) && !has(state, 27)
}

async fn extents(connection: &Connection, node: &Node) -> Result<Bounds> {
    let (x, y, width, height): (i32, i32, i32, i32) = proxy(connection, node, COMPONENT)
        .await?
        .call("GetExtents", &(SCREEN,))
        .await?;
    ensure!(
        (-32768..=32767).contains(&x)
            && (-32768..=32767).contains(&y)
            && (1..=65535).contains(&width)
            && (1..=65535).contains(&height),
        "AT-SPI denetim alanı geçersiz"
    );
    Ok(Bounds {
        x,
        y,
        width,
        height,
    })
}

fn matches_bounds(accessible: Bounds, client: Bounds) -> bool {
    accessible == client
        || (accessible.contains(client)
            && accessible.width - client.width <= 64
            && accessible.height - client.height <= 64)
}

fn center(rect: Bounds, client: Bounds) -> Option<(i32, i32)> {
    client.contains(rect).then(|| {
        let normalize = |location: i32, size: i32, origin: i32, dimension: i32| {
            ((location + size / 2 - origin) * 1000 / (dimension - 1).max(1)).min(1000)
        };
        (
            normalize(rect.x, rect.width, client.x, client.width),
            normalize(rect.y, rect.height, client.y, client.height),
        )
    })
}

fn control_kind(role: u32) -> Option<&'static str> {
    // AT-SPI role IDs from the Accessible interface.
    match role {
        7 => Some("check_box"),
        11 => Some("combo_box"),
        29 | 116 => Some("label"),
        35 => Some("menu_item"),
        37 => Some("tab"),
        43 => Some("button"),
        44 => Some("radio_button"),
        62 => Some("toggle_button"),
        83 => Some("heading"),
        _ => None,
    }
}

fn label(raw: &str) -> String {
    raw.chars()
        .filter(|ch| !ch.is_control())
        .take(180)
        .collect::<String>()
        .trim()
        .into()
}

async fn default_action(connection: &Connection, node: &Node) -> Option<i32> {
    let action = proxy(connection, node, ACTION).await.ok()?;
    let name: String = action.call("GetName", &(0_i32,)).await.ok()?;
    matches!(name.as_str(), "click" | "press" | "activate").then_some(0)
}

fn button_id(node: &Node, label: &str, rect: Bounds, index: i32, title: &str, pid: u32) -> String {
    let mut hash = Sha256::new();
    for field in [node.0.as_str(), node.1.as_str(), label, title] {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field);
    }
    for field in [rect.x, rect.y, rect.width, rect.height, index] {
        hash.update(field.to_le_bytes());
    }
    hash.update(pid.to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn run(access: Access<'_>, wanted: Option<&str>) -> Result<Controls> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let connection = access.connection().await?;
            access.controls(&connection, wanted).await
        })
        .await
        .context("AT-SPI erişim isteğinin süresi doldu")?
    })
}

pub(super) fn visible_controls(
    desktop: &Desktop,
    target: &Target,
    title: &str,
    cancelled: &AtomicBool,
) -> Result<Controls> {
    run(
        Access {
            desktop,
            target,
            title,
            cancelled,
        },
        None,
    )
}

pub(super) fn invoke_button(
    desktop: &Desktop,
    target: &Target,
    title: &str,
    id: &str,
    cancelled: &AtomicBool,
) -> Result<()> {
    run(
        Access {
            desktop,
            target,
            title,
            cancelled,
        },
        Some(id),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipped_accessibility_nodes_have_no_coordinates() {
        let window = Bounds {
            x: 40,
            y: 20,
            width: 101,
            height: 101,
        };
        let button = Bounds {
            x: 80,
            y: 60,
            width: 20,
            height: 20,
        };
        assert_eq!(center(button, window), Some((500, 500)));
        assert_eq!(center(Bounds { x: 30, ..button }, window), None);
        assert!(matches_bounds(window, window));
        assert!(!matches_bounds(button, window));
    }

    #[test]
    fn button_identity_changes_for_relabelled_or_moved_nodes() -> Result<()> {
        let node = (":1.5".into(), OwnedObjectPath::try_from("/fixture/button")?);
        let rect = Bounds {
            x: 40,
            y: 20,
            width: 100,
            height: 100,
        };
        let id = button_id(&node, "Save", rect, 0, "Document", 42);
        assert_eq!(id.len(), 64);
        assert_ne!(id, button_id(&node, "Delete", rect, 0, "Document", 42));
        assert_ne!(
            id,
            button_id(&node, "Save", Bounds { x: 41, ..rect }, 0, "Document", 42)
        );
        Ok(())
    }
}
