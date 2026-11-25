//! What a launch policy does to the title's window: bring it to the front,
//! then press Enter through `SendInput` on the schedule `launch.toml` asks
//! for ([`victory_core::launch`]).
//!
//! `SendKeys` and `PostMessage` do not reach this title (a posted key never
//! arrives, whatever focus does), so a press is a real key: its scancode from
//! `MapVirtualKey`, handed to `SendInput` as key down, a short hold, key up.
//! The focused window receives it, so `focus` is taken for the presses when
//! both are on and given back when the schedule is done.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use victory_core::launch::LaunchPolicy;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SW_RESTORE, SW_SHOW,
    SetForegroundWindow, ShowWindow,
};

/// How long a key stays down, in milliseconds: a tap this title misses at 0.
const HOLD_MS: u64 = 180;

/// This run's policy; `None` when both features are off and nothing is done.
static POLICY: Mutex<Option<LaunchPolicy>> = Mutex::new(None);
/// Whether the worker was started: the first window claims it, later ones
/// leave it alone (the title may re-make its swap chain).
static STARTED: AtomicBool = AtomicBool::new(false);

/// Hands the run's policy to the layer, which applies it once the window
/// exists. Called by `run` before the title starts.
pub fn install(policy: LaunchPolicy) {
    *POLICY.lock().unwrap_or_else(|error| error.into_inner()) = policy.active().then_some(policy);
}

/// The window exists (the swap chain was made for it): start applying the
/// policy, once per run.
pub(crate) fn window_ready(hwnd: isize) {
    let policy = *POLICY.lock().unwrap_or_else(|error| error.into_inner());
    let Some(policy) = policy else {
        return;
    };
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("victory-launch".into())
        .spawn(move || apply(hwnd, policy));
    if let Err(error) = spawned {
        tracing::warn!(error = %error, "launch: no thread to focus the window or press Enter");
    }
}

/// The policy on one thread of its own: focus, every press the schedule asks
/// for, then the foreground back. A failure is a warning, never a panic.
fn apply(hwnd: isize, policy: LaunchPolicy) {
    let mut previous = None;
    if policy.focus {
        match focus(hwnd) {
            Ok(was) => {
                tracing::info!(
                    window = format!("{hwnd:#x}"),
                    "launch: the window is in front"
                );
                previous = Some(was);
            }
            Err(error) => {
                tracing::warn!(error = %error, "launch: the window did not come to the front")
            }
        }
    }
    if policy.enter {
        let schedule = policy.schedule();
        let started = Instant::now();
        let mut pressed = 0;
        for (index, due) in schedule.presses().enumerate() {
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
            match press_enter() {
                Ok(()) => {
                    pressed += 1;
                    tracing::info!(
                        count = index + 1,
                        of = schedule.times,
                        "launch: pressed Enter"
                    );
                }
                Err(error) => {
                    tracing::warn!(count = index + 1, error = %error, "launch: Enter not pressed");
                }
            }
        }
        tracing::info!(
            count = pressed,
            of = schedule.times,
            "launch: the schedule finished"
        );
        // Focus was taken to make those presses land, not to stay: give the
        // window that had it back, as `press-key.ps1 -Mode focus` does.
        if let Some(previous) = previous {
            restore(previous);
        }
    }
}

/// Brings `hwnd` to the front and answers the window that had the foreground
/// (`0` when there was none), for [`restore`]. Plain `SetForegroundWindow`
/// first; the `AttachThreadInput` dance only when that is refused.
pub fn focus(hwnd: isize) -> Result<isize, String> {
    let target = HWND(hwnd as *mut _);
    // SAFETY: plain window calls on the handle the swap chain was made for.
    unsafe {
        let was = GetForegroundWindow();
        if was == target {
            return Ok(was.0 as isize);
        }
        if IsIconic(target).as_bool() {
            let _ = ShowWindow(target, SW_RESTORE);
        }
        if SetForegroundWindow(target).as_bool() {
            return Ok(was.0 as isize);
        }
        // A thread may set the foreground window only if it owns it or shares
        // input state with the thread that does: attach, try, detach.
        let (me, owner) = (GetCurrentThreadId(), GetWindowThreadProcessId(was, None));
        if owner != 0 && owner != me {
            let _ = AttachThreadInput(me, owner, true);
            let _ = SetForegroundWindow(target);
            let _ = AttachThreadInput(me, owner, false);
            if GetForegroundWindow() == target {
                return Ok(was.0 as isize);
            }
        }
        // Last resort: a hidden or minimised window cannot take it otherwise.
        let _ = ShowWindow(target, SW_SHOW);
        let _ = BringWindowToTop(target);
        let _ = SetForegroundWindow(target);
        if GetForegroundWindow() == target {
            Ok(was.0 as isize)
        } else {
            Err(format!("window {hwnd:#x} did not take the foreground"))
        }
    }
}

/// Puts back the window [`focus`] answered, giving focus back after the last
/// press.
pub fn restore(previous: isize) {
    if previous == 0 {
        return;
    }
    // SAFETY: a handle that had the foreground before this run took it.
    let back = unsafe { SetForegroundWindow(HWND(previous as *mut _)).as_bool() };
    if !back {
        tracing::warn!(
            window = format!("{previous:#x}"),
            "launch: the window that had the foreground did not take it back"
        );
    }
}

/// One Enter: key down, [`HOLD_MS`] of hold, key up. Two inputs on the stack,
/// nothing allocated.
pub fn press_enter() -> Result<(), String> {
    // SAFETY: a plain translation of the key code to its scancode.
    let scan = unsafe { MapVirtualKeyW(u32::from(VK_RETURN.0), MAPVK_VK_TO_VSC) } as u16;
    if scan == 0 {
        return Err("Enter has no scancode on this keyboard layout".to_owned());
    }
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_RETURN,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send(&key(KEYBD_EVENT_FLAGS(0)))?;
    std::thread::sleep(Duration::from_millis(HOLD_MS));
    send(&key(KEYEVENTF_KEYUP))
}

/// Hands one input to the system, which delivers it to the focused window.
fn send(input: &INPUT) -> Result<(), String> {
    // SAFETY: one input of the size stated, read only by the call.
    let sent = unsafe { SendInput(std::slice::from_ref(input), size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(format!("SendInput took {sent} of 1 input"))
    }
}
