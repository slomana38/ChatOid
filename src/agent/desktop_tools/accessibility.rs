use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use windows::Win32::{
    Foundation::{HWND, RECT},
    System::{
        Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize, SAFEARRAY,
        },
        Ole::{
            SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetElemsize,
            SafeArrayGetLBound, SafeArrayGetUBound,
        },
    },
    UI::Accessibility::{
        CUIAutomation8, IUIAutomation2, IUIAutomationElement, IUIAutomationInvokePattern,
        UIA_ButtonControlTypeId, UIA_EditControlTypeId, UIA_InvokePatternId,
    },
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor};

use super::{PhysicalCoordinates, Target, bounds, readable_window};

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub(super) struct ControlReport {
    pub(super) data: Value,
    pub(super) buttons: BTreeMap<String, String>,
}

pub(super) fn visible_controls(target: &Target, cancelled: &AtomicBool) -> Result<ControlReport> {
    let mut controls = Vec::new();
    let mut buttons = BTreeMap::new();
    let truncated = scan(target, cancelled, |_, role, label, x, y, id| {
        let mut control = json!({"role":role,"label":label,"x":x,"y":y});
        if let Some(id) = id {
            control["button_id"] = json!(id);
            buttons.insert(id, label.to_owned());
        }
        controls.push(control);
        Ok(true)
    })?;
    Ok(ControlReport {
        data: json!({"controls":controls,"truncated":truncated}),
        buttons,
    })
}

pub(super) fn invoke_button(target: &Target, id: &str, cancelled: &AtomicBool) -> Result<()> {
    let mut invoked = false;
    scan(target, cancelled, |element, _, _, _, _, key| {
        if key.as_deref() != Some(id) {
            return Ok(true);
        }
        let action: IUIAutomationInvokePattern =
            unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) }
                .context("Düğme doğrudan çalıştırmayı desteklemiyor")?;
        target.checked()?;
        ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
        unsafe { action.Invoke() }.context("Windows düğmeyi çalıştıramadı")?;
        invoked = true;
        Ok(false)
    })?;
    ensure!(
        invoked,
        "Düğme değişti veya görünmüyor. Pencereyi yeniden oku."
    );
    Ok(())
}

fn scan(
    target: &Target,
    cancelled: &AtomicBool,
    mut visit: impl FnMut(&IUIAutomationElement, &str, &str, i32, i32, Option<String>) -> Result<bool>,
) -> Result<bool> {
    ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
    let _dpi = PhysicalCoordinates::enter()?;
    let window = readable_window(target)?;
    let rect = bounds(window)?;
    // This tool runs on the task worker. Release UI Automation interfaces before COM.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .context("Windows erişilebilirlik bağlantısı başlatılamadı")?;
    let _apartment = ComApartment;
    let automation: IUIAutomation2 =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }
            .context("Windows erişilebilirlik servisine bağlanılamadı")?;
    unsafe {
        automation.SetConnectionTimeout(450)?;
        automation.SetTransactionTimeout(450)?;
    }
    let root = unsafe { automation.ElementFromHandle(HWND(window)) }
        .context("Seçilen pencere erişilebilirlik bilgisi sunmadı")?;
    let walker = unsafe { automation.ControlViewWalker()? };
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut pending = vec![(root, 0)];
    let mut visited = 0;
    let mut shown = 0;
    let mut truncated = false;
    while let Some((element, depth)) = pending.pop() {
        ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
        if visited >= 120 || shown >= 50 || Instant::now() >= deadline {
            truncated = true;
            break;
        }
        visited += 1;
        if depth > 0
            && let Ok(sibling) = unsafe { walker.GetNextSiblingElement(&element) }
        {
            pending.push((sibling, depth));
        }
        // Check scope and password state before reading any provider-supplied label.
        if unsafe { element.CurrentProcessId() }
            .ok()
            .map(|id| id as u32)
            != Some(target.process)
            || !unsafe { element.CurrentIsPassword() }.is_ok_and(|flag| !flag.as_bool())
            || !unsafe { element.CurrentIsOffscreen() }.is_ok_and(|flag| !flag.as_bool())
            || unsafe { element.CurrentNativeWindowHandle() }
                .ok()
                .is_some_and(|handle| {
                    !handle.0.is_null() && unsafe { GetAncestor(handle.0, GA_ROOT) } != window
                })
        {
            continue;
        }
        if depth < 6
            && let Ok(child) = unsafe { walker.GetFirstChildElement(&element) }
        {
            pending.push((child, depth + 1));
        }
        if depth == 0 {
            continue;
        }
        let Ok(area) = (unsafe { element.CurrentBoundingRectangle() }) else {
            continue;
        };
        let left = rect.left.max(area.left);
        let right = rect.right.min(area.right);
        let top = rect.top.max(area.top);
        let bottom = rect.bottom.min(area.bottom);
        if left >= right || top >= bottom {
            continue;
        }
        let Ok(kind) = (unsafe { element.CurrentControlType() }) else {
            continue;
        };
        let role = if let Ok(role) = unsafe { element.CurrentLocalizedControlType() } {
            role.to_string().chars().take(48).collect::<String>()
        } else {
            continue;
        };
        let name = if kind == UIA_EditControlTypeId {
            String::new()
        } else {
            unsafe { element.CurrentName() }
                .ok()
                .map(|value| value.to_string())
                .unwrap_or_default()
        };
        let label: String = name
            .chars()
            .take(120)
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        let x =
            ((left - rect.left) + (right - left - 1) / 2) * 1000 / (rect.right - rect.left).max(1);
        let y =
            ((top - rect.top) + (bottom - top - 1) / 2) * 1000 / (rect.bottom - rect.top).max(1);
        let button = (kind == UIA_ButtonControlTypeId
            && !label.trim().is_empty()
            && unsafe { element.CurrentIsEnabled() }.is_ok_and(|enabled| enabled.as_bool()))
        .then(|| button_key(&element, target, &name, area))
        .flatten();
        shown += 1;
        if !visit(&element, &role, &label, x, y, button)? {
            return Ok(false);
        }
    }
    ensure!(
        readable_window(target)? == window && !cancelled.load(Ordering::Acquire),
        "Pencere okuması kesildi"
    );
    Ok(truncated)
}

struct RuntimeId(*mut SAFEARRAY);

impl Drop for RuntimeId {
    fn drop(&mut self) {
        let _ = unsafe { SafeArrayDestroy(self.0) };
    }
}

fn button_key(
    element: &IUIAutomationElement,
    target: &Target,
    label: &str,
    area: RECT,
) -> Option<String> {
    let id = unsafe { element.GetRuntimeId().ok()? };
    if id.is_null() {
        return None;
    }
    let id = RuntimeId(id);
    if unsafe { SafeArrayGetDim(id.0) } != 1 || unsafe { SafeArrayGetElemsize(id.0) } != 4 {
        return None;
    }
    let lo = unsafe { SafeArrayGetLBound(id.0, 1).ok()? };
    let hi = unsafe { SafeArrayGetUBound(id.0, 1).ok()? };
    if !(0..64).contains(&hi.checked_sub(lo)?) {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(target.handle.to_le_bytes());
    hash.update(target.process.to_le_bytes());
    for coord in [area.left, area.top, area.right, area.bottom] {
        hash.update(coord.to_le_bytes());
    }
    for index in lo..=hi {
        let mut part = 0i32;
        unsafe {
            SafeArrayGetElement(id.0, &index, (&mut part as *mut i32).cast()).ok()?;
        }
        hash.update(part.to_le_bytes());
    }
    hash.update(label.as_bytes());
    Some(format!("{:x}", hash.finalize()))
}
