use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::border_manager;
use crate::container::Container;
use crate::core::Rect;
use crate::window::RuleDebug;
use crate::window::Window;
use crate::window_manager_event::WindowManagerEvent;
use crate::windows_api::WindowsApi;
use crate::winevent::WinEvent;
use crate::winevent_listener;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Foundation::RECT;
use windows::Win32::Foundation::WPARAM;
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;
use windows::Win32::UI::WindowsAndMessaging::GWL_STYLE;
use windows::Win32::UI::WindowsAndMessaging::GetWindowLongW;
use windows::Win32::UI::WindowsAndMessaging::OBJID_WINDOW;
use windows::Win32::UI::WindowsAndMessaging::SendNotifyMessageW;
use windows::Win32::UI::WindowsAndMessaging::WS_CHILD;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_NOACTIVATE;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW;
use windows_core::BOOL;

static MOVE_SIZE_EVENTS: AtomicUsize = AtomicUsize::new(0);

fn moved_without_resizing(from: Rect, to: Rect) -> bool {
    (from.left != to.left || from.top != to.top)
        && from.right == to.right
        && from.bottom == to.bottom
}

fn watch_for_missing_move_size_event(window: Window, started_from: Rect, pressed_at_focus: bool) {
    let generation = MOVE_SIZE_EVENTS.load(Ordering::SeqCst);

    std::thread::spawn(move || {
        // A custom title bar can focus its window just before the mouse press is observable.
        let press_deadline = Instant::now() + Duration::from_millis(500);
        let mut pressed = pressed_at_focus;
        while !pressed && Instant::now() < press_deadline {
            pressed = WindowsApi::lbutton_is_pressed();
            if !pressed {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        if !pressed {
            tracing::debug!(hwnd = window.hwnd, "mouse drag fallback: no press after focus");
            return;
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        while WindowsApi::lbutton_is_pressed() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        if WindowsApi::lbutton_is_pressed() {
            tracing::debug!(hwnd = window.hwnd, "mouse drag fallback: press timed out");
            return;
        }

        // Give a real move-size end event time to arrive before using the fallback.
        std::thread::sleep(Duration::from_millis(50));
        let native_generation = MOVE_SIZE_EVENTS.load(Ordering::SeqCst);
        let foreground = WindowsApi::foreground_window().ok();
        let current = WindowsApi::window_rect(window.hwnd).ok();
        tracing::debug!(
            hwnd = window.hwnd,
            generation,
            native_generation,
            ?foreground,
            ?started_from,
            ?current,
            "mouse drag fallback: checking after release"
        );
        if native_generation == generation
            && foreground == Some(window.hwnd)
            && current.is_some_and(|current| moved_without_resizing(started_from, current))
        {
            let _ = winevent_listener::event_tx().send(WindowManagerEvent::MoveResizeEndFallback(
                window,
                started_from,
            ));
        }
    });
}

/// Collects the rectangle of every monitor, as the system sees them.
pub extern "system" fn enum_monitor_rects(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let rects = unsafe { &mut *(lparam.0 as *mut Vec<Rect>) };

    if let Ok(info) = WindowsApi::monitor_info_w(hmonitor) {
        rects.push(Rect::from(info.monitorInfo.rcMonitor));
    }

    true.into()
}

pub extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let containers = unsafe { &mut *(lparam.0 as *mut VecDeque<Container>) };

    let is_visible = WindowsApi::is_window_visible(hwnd.0 as isize);
    let is_window = WindowsApi::is_window(hwnd.0 as isize);
    let is_minimized = WindowsApi::is_iconic(hwnd.0 as isize);
    let is_maximized = WindowsApi::is_zoomed(hwnd.0 as isize);

    if is_visible && is_window && !is_minimized {
        let window = Window::from(hwnd);

        if let Ok(should_manage) = window.should_manage(None, &mut RuleDebug::default())
            && should_manage
        {
            if is_maximized {
                WindowsApi::restore_window(window.hwnd);
            }

            let mut container = Container::default();
            container.windows_mut().push_back(window);
            containers.push_back(container);
        }
    }

    true.into()
}

pub extern "system" fn alt_tab_windows(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = unsafe { &mut *(lparam.0 as *mut Vec<Window>) };

    let is_visible = WindowsApi::is_window_visible(hwnd.0 as isize);
    let is_window = WindowsApi::is_window(hwnd.0 as isize);
    let is_minimized = WindowsApi::is_iconic(hwnd.0 as isize);

    if is_visible && is_window && !is_minimized {
        let window = Window::from(hwnd);

        if let Ok(should_manage) = window.should_manage(None, &mut RuleDebug::default())
            && should_manage
        {
            windows.push(window);
        }
    }

    true.into()
}

fn has_filtered_style(hwnd: HWND) -> bool {
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 };

    style & WS_CHILD.0 != 0
        || ex_style & WS_EX_TOOLWINDOW.0 != 0
        || ex_style & WS_EX_NOACTIVATE.0 != 0
}

lazy_static::lazy_static! {
    /// The windows of the window manager that were maximized when last looked at
    static ref ZOOMED: parking_lot::Mutex<std::collections::HashSet<isize>> =
        parking_lot::Mutex::new(std::collections::HashSet::new());
}

#[derive(Debug, PartialEq, Eq)]
enum ZoomChange {
    Maximized,
    Restored,
}

fn zoom_change(was_zoomed: bool, is_zoomed: bool) -> Option<ZoomChange> {
    match (was_zoomed, is_zoomed) {
        (false, true) => Some(ZoomChange::Maximized),
        (true, false) => Some(ZoomChange::Restored),
        _ => None,
    }
}

/// A window of the window manager that has been maximized or restored since the last time it was
/// looked at, which the user does with the button or the title bar of the window. This is read
/// for the location change of every window that moves, so it is a lookup of the handle and one
/// call, before anything is asked of the window.
fn zoom_event(winevent: WinEvent, window: Window) -> Option<WindowManagerEvent> {
    if !crate::reaper::HWNDS_CACHE.lock().contains_key(&window.hwnd) {
        ZOOMED.lock().remove(&window.hwnd);
        return None;
    }

    let is_zoomed = WindowsApi::is_zoomed(window.hwnd);
    let was_zoomed = if is_zoomed {
        !ZOOMED.lock().insert(window.hwnd)
    } else {
        ZOOMED.lock().remove(&window.hwnd)
    };

    match zoom_change(was_zoomed, is_zoomed)? {
        ZoomChange::Maximized => Some(WindowManagerEvent::Maximize(winevent, window)),
        ZoomChange::Restored => Some(WindowManagerEvent::Unmaximize(winevent, window)),
    }
}

pub extern "system" fn win_event_hook(
    _h_win_event_hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if id_object != OBJID_WINDOW.0 {
        return;
    }

    let window = Window::from(hwnd);

    let winevent = match WinEvent::try_from(event) {
        Ok(event) => event,
        Err(_) => return,
    };

    // a window the user has hold of by an edge is laid out around as it is dragged
    match winevent {
        WinEvent::SystemMoveSizeStart => {
            MOVE_SIZE_EVENTS.fetch_add(1, Ordering::SeqCst);
            crate::live_resize::taken_hold_of(hwnd.0 as isize);
        }
        WinEvent::SystemMoveSizeEnd => {
            MOVE_SIZE_EVENTS.fetch_add(1, Ordering::SeqCst);
            crate::live_resize::let_go(hwnd.0 as isize);
        }
        WinEvent::ObjectDestroy => {
            crate::live_resize::let_go(hwnd.0 as isize);
        }
        WinEvent::ObjectLocationChange => crate::live_resize::window_moved(hwnd.0 as isize),
        _ => {}
    }

    // this forwards the message to the window's border when it moves or is destroyed
    // see border_manager/border.rs
    if matches!(
        winevent,
        WinEvent::ObjectLocationChange | WinEvent::ObjectDestroy
    ) && !has_filtered_style(hwnd)
    {
        let border_info = border_manager::window_border(hwnd.0 as isize);

        if let Some(border_info) = border_info {
            unsafe {
                let _ = SendNotifyMessageW(
                    border_info.hwnd(),
                    event,
                    WPARAM(0),
                    LPARAM(hwnd.0 as isize),
                );
            }
        }
    }

    // sometimes the border focus state and colors don't get updated because this event comes too
    // slow for the value of GetForegroundWindow to be up to date by the time it is inspected in
    // the border manager to determine if a window show have its border show as "focused"
    //
    // so here we can just fire another event at the border manager when the system has finally
    // registered the new foreground window and this time the correct border colors will be applied
    if matches!(winevent, WinEvent::SystemForeground) && !has_filtered_style(hwnd) {
        border_manager::send_notification(Some(hwnd.0 as isize));
    }

    let event_type = match WindowManagerEvent::from_win_event(winevent, window) {
        None => {
            // far more of these are sent than of anything else, by every window that
            // is moved, and what a window is takes a call into it for each of the
            // exe, the title and the class: the hook is held up for as long as that takes
            if matches!(winevent, WinEvent::ObjectLocationChange) {
                if let Some(event) = zoom_event(winevent, window) {
                    winevent_listener::event_tx()
                        .send(event)
                        .expect("could not send message on winevent_listener::event_tx");
                }

                return;
            }

            tracing::trace!(
                "Unhandled WinEvent: {winevent} (hwnd: {}, exe: {}, title: {}, class: {})",
                window.hwnd,
                window.exe().unwrap_or_default(),
                window.title().unwrap_or_default(),
                window.class().unwrap_or_default()
            );

            return;
        }
        Some(event) => event,
    };

    let fallback_origin = if matches!(winevent, WinEvent::SystemForeground) {
        let pressed = WindowsApi::lbutton_is_pressed();
        let dragging = crate::live_resize::is_dragging(window.hwnd);
        let managed = crate::reaper::HWNDS_CACHE.lock().contains_key(&window.hwnd);
        tracing::debug!(
            hwnd = window.hwnd,
            pressed,
            dragging,
            managed,
            "mouse drag fallback: foreground event"
        );
        if managed && !dragging {
            WindowsApi::window_rect(window.hwnd)
                .ok()
                .map(|rect| (rect, pressed))
        } else {
            None
        }
    } else {
        None
    };

    winevent_listener::event_tx()
        .send(event_type)
        .expect("could not send message on winevent_listener::event_tx");

    if let Some((started_from, pressed)) = fallback_origin {
        watch_for_missing_move_size_event(window, started_from, pressed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_move_size_event_is_only_reconciled_for_a_move() {
        let from = Rect {
            left: 100,
            top: 200,
            right: 400,
            bottom: 300,
        };
        let moved = Rect {
            left: 150,
            top: 250,
            ..from
        };
        let resized = Rect { right: 450, ..from };

        assert!(moved_without_resizing(from, moved));
        assert!(!moved_without_resizing(from, from));
        assert!(!moved_without_resizing(from, resized));
    }

    #[test]
    fn a_window_is_maximized_or_restored_only_when_it_changes() {
        assert_eq!(zoom_change(false, true), Some(ZoomChange::Maximized));
        assert_eq!(zoom_change(true, false), Some(ZoomChange::Restored));
        assert_eq!(zoom_change(true, true), None);
        assert_eq!(zoom_change(false, false), None);
    }
}
