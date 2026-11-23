use std::ptr::null_mut;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        CreateIcon, DestroyIcon, HICON, ICON_BIG, ICON_SMALL, IsWindow, SendMessageW, WM_GETICON,
        WM_SETICON,
    },
};

use crate::identity::Role;

pub struct TaskbarIcon {
    window: HWND,
    big: HICON,
    small: HICON,
    role: Role,
}

pub fn update(current: &mut Option<TaskbarIcon>, frame: &eframe::Frame, role: Role) {
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    set_for_window(current, handle.hwnd.get() as HWND, role);
}

fn set_for_window(current: &mut Option<TaskbarIcon>, window: HWND, role: Role) {
    if let Some(icon) = current
        .as_ref()
        .filter(|icon| icon.window == window && icon.role == role)
    {
        icon.install();
        return;
    }
    let big = create_icon(role, 64);
    let small = create_icon(role, 24);
    let replacement = TaskbarIcon {
        window,
        big,
        small,
        role,
    };
    if big.is_null() || small.is_null() {
        return;
    }
    replacement.install();
    *current = Some(replacement);
}

fn create_icon(role: Role, size: u32) -> HICON {
    let mut bgra = crate::branding::icon_rgba(role == Role::Agent, size);
    let mask_stride = size.div_ceil(32) as usize * 4;
    let mut mask = vec![0_u8; mask_stride * size as usize];
    for (index, pixel) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        pixel.swap(0, 2);
        if pixel[3] == 0 {
            let (x, y) = (index % size as usize, index / size as usize);
            mask[y * mask_stride + x / 8] |= 0x80 >> (x % 8);
        }
    }
    // CreateIcon copies both buffers.
    unsafe {
        CreateIcon(
            null_mut(),
            size as i32,
            size as i32,
            1,
            32,
            mask.as_ptr(),
            bgra.as_ptr(),
        )
    }
}

impl TaskbarIcon {
    fn install(&self) {
        for (kind, icon) in [(ICON_BIG, self.big), (ICON_SMALL, self.small)] {
            unsafe {
                if SendMessageW(self.window, WM_GETICON, kind as usize, 0) != icon as isize {
                    SendMessageW(self.window, WM_SETICON, kind as usize, icon as isize);
                }
            }
        }
    }
}

impl Drop for TaskbarIcon {
    fn drop(&mut self) {
        // Detach only if this handle is still installed. A role change installs
        // its replacement before this destructor releases the previous icon.
        for (kind, icon) in [(ICON_BIG, self.big), (ICON_SMALL, self.small)] {
            if icon.is_null() {
                continue;
            }
            unsafe {
                if IsWindow(self.window) != 0
                    && SendMessageW(self.window, WM_GETICON, kind as usize, 0) == icon as isize
                {
                    SendMessageW(self.window, WM_SETICON, kind as usize, 0);
                }
                DestroyIcon(icon);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::{
        Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow},
        w,
    };

    #[test]
    fn role_switch_replaces_both_icons_and_restores_a_reset_handle() {
        let window = unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                w!("ChatOid icon test"),
                0,
                0,
                0,
                1,
                1,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        assert!(!window.is_null());
        let read = || {
            [ICON_BIG, ICON_SMALL]
                .map(|kind| unsafe { SendMessageW(window, WM_GETICON, kind as usize, 0) })
        };
        let mut icon = None;
        set_for_window(&mut icon, window, Role::Human);
        let human = read();
        set_for_window(&mut icon, window, Role::Agent);
        let agent = read();
        unsafe {
            SendMessageW(window, WM_SETICON, ICON_BIG as usize, 0);
        }
        set_for_window(&mut icon, window, Role::Agent);
        let unchanged = read();
        drop(icon);
        let cleared = read();
        unsafe {
            DestroyWindow(window);
        }
        assert!(human.into_iter().all(|icon| icon != 0));
        assert!(agent.into_iter().all(|icon| icon != 0));
        assert_ne!(human, agent);
        assert_eq!(agent, unchanged);
        assert_eq!(cleared, [0, 0]);
    }
}
