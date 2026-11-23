use std::ptr::null_mut;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        CreateIcon, DestroyIcon, HICON, ICON_BIG, IsWindow, SendMessageW, WM_GETICON, WM_SETICON,
    },
};

use crate::identity::Role;

pub struct TaskbarIcon {
    window: HWND,
    icon: HICON,
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
    if current
        .as_ref()
        .is_some_and(|icon| icon.window == window && icon.role == role)
    {
        return;
    }
    let image = super::visual::app_icon(role);
    let mut bgra = image.rgba;
    let mask_stride = image.width.div_ceil(32) as usize * 4;
    let mut mask = vec![0_u8; mask_stride * image.height as usize];
    for (index, pixel) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        pixel.swap(0, 2);
        if pixel[3] == 0 {
            let (x, y) = (index % image.width as usize, index / image.width as usize);
            mask[y * mask_stride + x / 8] |= 0x80 >> (x % 8);
        }
    }
    // CreateIcon copies both buffers. Winit manages ICON_SMALL independently.
    let icon = unsafe {
        CreateIcon(
            null_mut(),
            image.width as i32,
            image.height as i32,
            1,
            32,
            mask.as_ptr(),
            bgra.as_ptr(),
        )
    };
    if icon.is_null() {
        return;
    }
    unsafe {
        SendMessageW(window, WM_SETICON, ICON_BIG as usize, icon as isize);
    }
    *current = Some(TaskbarIcon { window, icon, role });
}

impl Drop for TaskbarIcon {
    fn drop(&mut self) {
        // Detach only if this handle is still installed. A role change installs
        // its replacement before this destructor releases the previous icon.
        unsafe {
            if IsWindow(self.window) != 0
                && SendMessageW(self.window, WM_GETICON, ICON_BIG as usize, 0) == self.icon as isize
            {
                SendMessageW(self.window, WM_SETICON, ICON_BIG as usize, 0);
            }
            DestroyIcon(self.icon);
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
    fn role_switch_replaces_windows_large_icon() {
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
        let read = || unsafe { SendMessageW(window, WM_GETICON, ICON_BIG as usize, 0) };
        let mut icon = None;
        set_for_window(&mut icon, window, Role::Human);
        let human = read();
        set_for_window(&mut icon, window, Role::Agent);
        let agent = read();
        set_for_window(&mut icon, window, Role::Agent);
        let unchanged = read();
        drop(icon);
        let cleared = read();
        unsafe {
            DestroyWindow(window);
        }
        assert_ne!(human, 0);
        assert_ne!(agent, 0);
        assert_ne!(human, agent);
        assert_eq!(agent, unchanged);
        assert_eq!(cleared, 0);
    }
}
