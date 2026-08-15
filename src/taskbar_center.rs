use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, VARIANT};

use crate::utils::to_wide;

// ============================================================
// State
// ============================================================

static ENABLED: AtomicBool = AtomicBool::new(false);
static TIMER_ID: Mutex<usize> = Mutex::new(0);

const REFRESH_MS: u32 = 300;

// ============================================================
// Public API
// ============================================================

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn start() {
    if ENABLED.load(Ordering::Relaxed) {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    center_all();
    unsafe {
        let id = SetTimer(None, 0, REFRESH_MS, Some(timer_proc));
        *TIMER_ID.lock().unwrap() = id;
    }
    println!("[taskbar] Центрирование иконок включено");
}

pub fn stop() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    ENABLED.store(false, Ordering::Relaxed);
    let mut id = TIMER_ID.lock().unwrap();
    if *id != 0 {
        unsafe {
            let _ = KillTimer(None, *id);
        }
        *id = 0;
    }
    reset_all();
    println!("[taskbar] Центрирование иконок выключено");
}

pub fn toggle() -> bool {
    let new_state = !ENABLED.load(Ordering::Relaxed);
    if new_state {
        start();
    } else {
        stop();
    }
    new_state
}

// ============================================================
// Timer
// ============================================================

unsafe extern "system" fn timer_proc(_hwnd: HWND, _msg: u32, _id: usize, _tick: u32) {
    if ENABLED.load(Ordering::Relaxed) {
        center_all();
    }
}

// ============================================================
// Center / reset
// ============================================================

fn center_all() {
    // Primary taskbar
    if let Some((container, task_list)) = find_primary_task_list() {
        apply_centering(container, task_list);
    }
    // Secondary taskbars (multi-monitor)
    for (container, task_list) in find_secondary_task_lists() {
        apply_centering(container, task_list);
    }
}

fn reset_all() {
    if let Some((_, task_list)) = find_primary_task_list() {
        reset_position(task_list);
    }
    for (_, task_list) in find_secondary_task_lists() {
        reset_position(task_list);
    }
}

fn apply_centering(container: HWND, task_list: HWND) {
    let buttons_width = match get_buttons_width(task_list) {
        Some(w) if w > 0 => w,
        _ => return,
    };

    unsafe {
        let mut sw_rect = RECT::default();
        if GetClientRect(container, &mut sw_rect).is_err() {
            return;
        }
        let container_width = sw_rect.right - sw_rect.left;

        if buttons_width >= container_width {
            reset_position(task_list);
            return;
        }

        let offset = (container_width - buttons_width) / 2;
        let _ = SetWindowPos(
            task_list,
            None,
            offset,
            0,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
        );
    }
}

fn reset_position(task_list: HWND) {
    unsafe {
        let _ = SetWindowPos(
            task_list,
            None,
            0,
            0,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
        );
    }
}

// ============================================================
// Button width via Accessibility API
// ============================================================

fn get_buttons_width(task_list: HWND) -> Option<i32> {
    unsafe {
        let mut raw: *mut core::ffi::c_void = core::ptr::null_mut();
        let hr = AccessibleObjectFromWindow(
            task_list,
            OBJID_CLIENT.0 as u32,
            &IAccessible::IID,
            &mut raw,
        );
        if hr.is_err() || raw.is_null() {
            return None;
        }
        let acc: IAccessible = IAccessible::from_raw(raw);

        let child_count = acc.accChildCount().unwrap_or(0);
        if child_count == 0 {
            return None;
        }

        let mut min_x = i32::MAX;
        let mut max_right = i32::MIN;
        let mut found = 0;

        for i in 1..=child_count {
            let var_child = VARIANT::from(i);
            let (mut cx, mut cy, mut cw, mut ch) = (0i32, 0i32, 0i32, 0i32);
            if acc
                .accLocation(&mut cx, &mut cy, &mut cw, &mut ch, &var_child)
                .is_ok()
                && cw > 0
            {
                if cx < min_x {
                    min_x = cx;
                }
                if cx + cw > max_right {
                    max_right = cx + cw;
                }
                found += 1;
            }
        }

        if found == 0 || max_right <= min_x {
            return None;
        }

        Some(max_right - min_x)
    }
}

// ============================================================
// Window discovery
// ============================================================

fn find_primary_task_list() -> Option<(HWND, HWND)> {
    unsafe {
        let cls_tray = to_wide("Shell_TrayWnd");
        let tray = FindWindowW(PCWSTR(cls_tray.as_ptr()), PCWSTR::null()).ok()?;

        let cls_rebar = to_wide("ReBarWindow32");
        let rebar = FindWindowExW(tray, None, PCWSTR(cls_rebar.as_ptr()), PCWSTR::null()).ok()?;

        let cls_sw = to_wide("MSTaskSwWClass");
        let sw = FindWindowExW(rebar, None, PCWSTR(cls_sw.as_ptr()), PCWSTR::null()).ok()?;

        let cls_list = to_wide("MSTaskListWClass");
        let list = FindWindowExW(sw, None, PCWSTR(cls_list.as_ptr()), PCWSTR::null()).ok()?;

        Some((sw, list))
    }
}

fn find_secondary_task_lists() -> Vec<(HWND, HWND)> {
    let mut result = Vec::new();
    unsafe {
        let cls_sec = to_wide("Shell_SecondaryTrayWnd");
        let cls_worker = to_wide("WorkerW");
        let cls_sw = to_wide("MSTaskSwWClass");
        let cls_list = to_wide("MSTaskListWClass");

        let mut tray = FindWindowW(PCWSTR(cls_sec.as_ptr()), PCWSTR::null()).unwrap_or_default();

        while !tray.0.is_null() {
            let worker = FindWindowExW(tray, None, PCWSTR(cls_worker.as_ptr()), PCWSTR::null())
                .unwrap_or_default();

            let sw = FindWindowExW(worker, None, PCWSTR(cls_sw.as_ptr()), PCWSTR::null())
                .unwrap_or_default();

            let list = FindWindowExW(sw, None, PCWSTR(cls_list.as_ptr()), PCWSTR::null())
                .unwrap_or_default();

            if !sw.0.is_null() && !list.0.is_null() {
                result.push((sw, list));
            }

            tray = FindWindowExW(
                HWND::default(),
                tray,
                PCWSTR(cls_sec.as_ptr()),
                PCWSTR::null(),
            )
            .unwrap_or_default();
        }
    }
    result
}
