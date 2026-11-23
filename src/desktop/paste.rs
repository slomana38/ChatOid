use std::cell::Cell;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{IsWindow, WM_KEYDOWN, WM_NCDESTROY},
    },
};

thread_local! {
    static PASTE: Cell<bool> = const { Cell::new(false) };
}

const SUBCLASS_ID: usize = 0x434F_4944;

pub struct NativePaste(HWND);

impl NativePaste {
    pub fn install(frame: &eframe::Frame) -> Option<Self> {
        let RawWindowHandle::Win32(window) = frame.window_handle().ok()?.as_raw() else {
            return None;
        };
        let window = window.hwnd.get() as HWND;
        // Egui handles text paste and consumes Ctrl+V. Observe the native event without intercepting it.
        (unsafe { SetWindowSubclass(window, Some(paste_proc), SUBCLASS_ID, 0) } != 0)
            .then_some(Self(window))
    }

    pub fn take(&self) -> bool {
        PASTE.with(|pending| pending.replace(false))
    }
}

unsafe extern "system" fn paste_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    if message == WM_KEYDOWN
        && wparam == b'V' as usize
        && lparam & (1 << 30) == 0
        && unsafe { GetKeyState(i32::from(VK_CONTROL)) < 0 && GetKeyState(i32::from(VK_MENU)) >= 0 }
    {
        PASTE.with(|pending| pending.set(true));
    }
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(window, Some(paste_proc), id) };
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

impl Drop for NativePaste {
    fn drop(&mut self) {
        if unsafe { IsWindow(self.0) } != 0 {
            unsafe { RemoveWindowSubclass(self.0, Some(paste_proc), SUBCLASS_ID) };
        }
    }
}
