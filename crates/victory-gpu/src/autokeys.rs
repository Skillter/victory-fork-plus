//! What a launch policy does to the title's window: bring it to the front,
//! then press Enter through `SendInput` on the schedule `launch.toml` asks
//! for ([`victory_core::launch`]).
//!
//! `SendKeys` and `PostMessage` do not reach this title (a posted key never
//! arrives, whatever focus does), so a press is a real key: its scancode from
//! `MapVirtualKey`, handed to `SendInput` as key down, a short hold, key up.
//!
//! `SendInput` delivers to the focused window of the foreground window's
//! thread, so a window merely in front is not enough: a run that only set the
//! foreground logged 12 presses while the title stayed in its menu
//! (`run-20261125-204657.log`). [`activate`] is the sequence that does work —
//! share input state with the window's own thread, `SetActiveWindow`,
//! `BringWindowToTop`, `SetFocus`, `SetForegroundWindow`, let go — and it runs
//! again before every press, because the user's own window can come back in
//! front between them.
//!
//! Which window: this process owns more than one. The layer makes its own
//! (`host::make_window`, class [`OUR_CLASS`]) when the title has no visible
//! window yet, and the swap chain then presents into ours while the title's
//! later window is the one with its input. [`Choice`] picks the largest
//! visible top-level window that is not ours — ours when it is the only one,
//! the swap chain's when the process shows none — and it is asked again before
//! every press: the title makes its window after us and may re-make it (device
//! loss, display change).

use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use victory_core::launch::LaunchPolicy;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput, SetActiveWindow, SetFocus, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GUITHREADINFO, GetClassNameW, GetClientRect,
    GetForegroundWindow, GetGUIThreadInfo, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
};

/// How long a key stays down, in milliseconds: a tap this title misses at 0.
const HOLD_MS: u64 = 180;
/// How long the window gets to settle after the `AttachThreadInput` dance, in
/// milliseconds: the wait the script that was measured to work keeps between
/// the dance and its first key.
const SETTLE_MS: u64 = 250;
/// The longest class name or title [`Name`] keeps, in UTF-16 units
/// (`sgaWindowClass` is 13).
const NAME_MOST: usize = 64;
/// The class of the window the layer makes when the title has none, a literal
/// here because `host::make_window` needs `w!`.
const OUR_CLASS: &str = "VIctory";

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
///
/// This is the window the swap chain presents into, which is ours
/// ([`OUR_CLASS`]) whenever the title had no visible window yet. It is not
/// necessarily the window a press goes into: [`Choice`] decides that at every
/// press, from the process's windows as they are then.
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

/// A class name or title, copied out of a window into a fixed buffer: the
/// choice keeps its candidates as plain copies, so nothing in the press path
/// is allocated.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    /// The name's UTF-16 units.
    text: [u16; NAME_MOST],
    /// How many of them are the name (the rest are zero).
    len: u8,
}

impl Name {
    /// No name at all, which is what a window without a title answers.
    pub const EMPTY: Name = Name {
        text: [0; NAME_MOST],
        len: 0,
    };

    /// The first [`NAME_MOST`] units of `text`.
    pub fn of(text: &str) -> Self {
        let mut name = Name::EMPTY;
        for (index, unit) in text.encode_utf16().take(NAME_MOST).enumerate() {
            name.text[index] = unit;
            name.len = (index + 1) as u8;
        }
        name
    }

    /// The name `units` holds, cut at [`NAME_MOST`].
    fn from_utf16(units: &[u16]) -> Self {
        let mut name = Name::EMPTY;
        let kept = units.len().min(NAME_MOST);
        name.text[..kept].copy_from_slice(&units[..kept]);
        name.len = kept as u8;
        name
    }

    /// Whether this name is `text`.
    pub fn is(&self, text: &str) -> bool {
        let mut units = text.encode_utf16();
        for unit in &self.text[..usize::from(self.len)] {
            if units.next() != Some(*unit) {
                return false;
            }
        }
        units.next().is_none()
    }
}

impl fmt::Display for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&String::from_utf16_lossy(
            &self.text[..usize::from(self.len)],
        ))
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

/// One of the process's windows, as the choice sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The window handle.
    pub handle: isize,
    /// Its parent: 0 for a top-level window. `EnumWindows` walks the desktop's
    /// children only, so the press path always sees 0 here; the field is what
    /// lets the rule be tested with a child window in the list.
    pub parent: isize,
    /// Whether the window is visible.
    pub visible: bool,
    /// The client area's width and height, in pixels.
    pub client: (i32, i32),
    /// Its class name.
    pub class: Name,
    /// Its title.
    pub title: Name,
}

impl Window {
    /// The client area, in pixels: a title's window is the big one, an IME or
    /// status window a small one.
    fn area(&self) -> i64 {
        i64::from(self.client.0.max(0)) * i64::from(self.client.1.max(0))
    }
}

/// The window a press goes into, kept while the process's windows are looked
/// at one at a time: no list is collected and nothing is allocated.
#[derive(Clone, Copy, Debug)]
pub struct Choice {
    /// The largest visible top-level window whose class is not ours.
    theirs: Option<Window>,
    /// The largest visible top-level window of our own class.
    ours: Option<Window>,
    /// The window the swap chain was made for, answered when the process shows
    /// no visible top-level window at all: what `focus` was given before this
    /// rule existed.
    owner: isize,
}

impl Choice {
    /// A choice that has seen nothing yet, for the window the swap chain was
    /// made for.
    pub fn new(owner: isize) -> Self {
        Self {
            theirs: None,
            ours: None,
            owner,
        }
    }

    /// Takes one window of the process into account.
    ///
    /// A child window and an invisible one are not candidates: the title takes
    /// its input in a top-level visible window, and its IME windows are
    /// children of it. Of the two that remain, the largest of each class is
    /// kept, and a window that is not ours wins over one that is, whatever
    /// their sizes.
    pub fn see(&mut self, window: Window) {
        if window.parent != 0 || !window.visible {
            return;
        }
        let slot = if window.class.is(OUR_CLASS) {
            &mut self.ours
        } else {
            &mut self.theirs
        };
        if slot.is_none_or(|best| best.area() < window.area()) {
            *slot = Some(window);
        }
    }

    /// The window to press into.
    pub fn window(&self) -> isize {
        self.chosen().map_or(self.owner, |window| window.handle)
    }

    /// Its class name, empty when the choice fell back on the swap chain's
    /// window (which the enumeration did not state).
    pub fn class(&self) -> Name {
        self.chosen().map_or(Name::EMPTY, |window| window.class)
    }

    /// Its title, empty in the same case as [`Choice::class`].
    pub fn title(&self) -> Name {
        self.chosen().map_or(Name::EMPTY, |window| window.title)
    }

    /// The window the choice settled on, if any was a candidate.
    fn chosen(&self) -> Option<Window> {
        self.theirs.or(self.ours)
    }
}

/// What applying a policy needs from the world: the window, the foreground,
/// one key, the clock. Every effect is a call here, so the schedule runs
/// against a desk that counts in a test and against Windows in a run.
trait Stage {
    /// The window a press goes into, looked up now.
    fn target(&mut self) -> Choice;

    /// Makes `window` the one the keyboard goes to, and answers the window
    /// that had the foreground before (0 when there was none): what
    /// [`restore`] gives back.
    fn activate(&mut self, window: isize) -> Result<isize, String>;

    /// One Enter, into the focused window.
    fn press(&mut self) -> Result<(), String>;

    /// Waits until the schedule's `due`, measured from the window existing,
    /// has passed.
    fn wait(&mut self, due: Duration);

    /// Gives the foreground back to the window that had it.
    fn give_back(&mut self, previous: isize);
}

/// The real stage: this process's windows, the foreground, `SendInput`, the
/// clock.
struct Desktop {
    /// The window the swap chain was made for.
    owner: isize,
    /// When the window existed: the schedule's origin.
    started: Instant,
}

impl Stage for Desktop {
    fn target(&mut self) -> Choice {
        look(self.owner)
    }

    fn activate(&mut self, window: isize) -> Result<isize, String> {
        activate(window)
    }

    fn press(&mut self) -> Result<(), String> {
        press_enter()
    }

    fn wait(&mut self, due: Duration) {
        if let Some(rest) = due.checked_sub(self.started.elapsed())
            && !rest.is_zero()
        {
            std::thread::sleep(rest);
        }
    }

    fn give_back(&mut self, previous: isize) {
        restore(previous);
    }
}

/// The policy on one thread of its own: focus, every press the schedule asks
/// for, then the foreground back. A failure is a warning, never a panic.
fn apply(owner: isize, policy: LaunchPolicy) {
    let mut desk = Desktop {
        owner,
        started: Instant::now(),
    };
    run(&mut desk, policy);
}

/// Applies `policy` through `stage`: the window once when `focus` asks for it,
/// then one activation and one press per deadline the schedule names.
fn run(stage: &mut impl Stage, policy: LaunchPolicy) {
    /// The one line that says which window the presses go into, so a log can
    /// prove it: its class and title are what tell a reader that the window
    /// with the title's input was the one chosen.
    fn stated(choice: &Choice) {
        tracing::info!(
            window = format!("{:#x}", choice.window()),
            class = %choice.class(),
            title = %choice.title(),
            "launch: the window is in front"
        );
    }

    // The window that had the foreground before this run took it, kept from
    // the first time one was taken: the presses are what it is given back
    // after, never the window the schedule itself left in front.
    let mut previous: Option<isize> = None;
    // The chosen window is stated once per run, on the first activation that
    // worked, whichever feature asked for it.
    let mut told = false;
    if policy.focus {
        let choice = stage.target();
        match stage.activate(choice.window()) {
            Ok(was) => {
                previous.get_or_insert(was);
                told = true;
                stated(&choice);
            }
            Err(error) => tracing::warn!(
                window = format!("{:#x}", choice.window()),
                class = %choice.class(),
                title = %choice.title(),
                error = %error,
                "launch: the window did not come to the front"
            ),
        }
    }
    if policy.enter {
        let schedule = policy.schedule();
        let mut pressed = 0;
        for (index, due) in schedule.presses().enumerate() {
            stage.wait(due);
            let choice = stage.target();
            let window = choice.window();
            match stage.activate(window) {
                Ok(was) => {
                    previous.get_or_insert(was);
                    if !told {
                        told = true;
                        stated(&choice);
                    }
                    match stage.press() {
                        Ok(()) => {
                            pressed += 1;
                            tracing::info!(
                                count = index + 1,
                                of = schedule.times,
                                "launch: pressed Enter"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                count = index + 1,
                                error = %error,
                                "launch: Enter not pressed"
                            );
                        }
                    }
                }
                // A press goes to whatever has the keys, and a run holds no
                // right to put an Enter into the user's own window.
                Err(error) => tracing::warn!(
                    count = index + 1,
                    window = format!("{window:#x}"),
                    class = %choice.class(),
                    title = %choice.title(),
                    error = %error,
                    "launch: Enter not pressed: the window did not take the keys"
                ),
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
            stage.give_back(previous);
        }
    }
}

/// Every top-level window of this process, fed to a [`Choice`] one at a time.
fn look(owner: isize) -> Choice {
    extern "system" fn visit(hwnd: HWND, choice: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `choice` is the one
        // this call owns, and it outlives the enumeration.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == GetCurrentProcessId() {
                (*(choice.0 as *mut Choice)).see(describe(hwnd));
            }
        }
        true.into()
    }
    let mut choice = Choice::new(owner);
    // SAFETY: the callback only writes `choice`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut choice as isize)) };
    choice
}

/// What one window is, for the choice.
fn describe(hwnd: HWND) -> Window {
    let mut client = RECT::default();
    // SAFETY: plain queries into buffers this frame owns.
    unsafe {
        let area = GetClientRect(hwnd, &mut client)
            .map(|()| (client.right - client.left, client.bottom - client.top))
            .unwrap_or((0, 0));
        Window {
            handle: hwnd.0 as isize,
            // `EnumWindows` walks the desktop's children: these are top-level.
            parent: 0,
            visible: IsWindowVisible(hwnd).as_bool(),
            client: area,
            class: read_name(hwnd, false),
            title: read_name(hwnd, true),
        }
    }
}

/// A window's class name (`title` false) or its title, copied out.
fn read_name(hwnd: HWND, title: bool) -> Name {
    let mut buffer = [0u16; NAME_MOST];
    // SAFETY: a plain query into a buffer this frame owns.
    let len = unsafe {
        if title {
            GetWindowTextW(hwnd, &mut buffer)
        } else {
            GetClassNameW(hwnd, &mut buffer)
        }
    };
    Name::from_utf16(&buffer[..usize::try_from(len).unwrap_or(0).min(NAME_MOST)])
}

/// Makes `hwnd` the window the keyboard goes to, and answers the window that
/// had the foreground before it (0 when there was none), for [`restore`].
///
/// Plain `SetForegroundWindow` first. The `AttachThreadInput` dance — the one
/// measured to work — only when the window is still not the one with the keys
/// after it: a window in front whose thread's focused window is something else
/// takes no key at all, which is how 12 presses landed on nothing.
pub fn activate(hwnd: isize) -> Result<isize, String> {
    let target = HWND(hwnd as *mut _);
    // SAFETY: plain window calls on a handle the enumeration answered.
    unsafe {
        let was = GetForegroundWindow();
        let mut why = String::new();
        if keys_go_to(target, &mut why) {
            return Ok(was.0 as isize);
        }
        if IsIconic(target).as_bool() {
            let _ = ShowWindow(target, SW_RESTORE);
        }
        let _ = SetForegroundWindow(target);
        if keys_go_to(target, &mut why) {
            return Ok(was.0 as isize);
        }
        if !dance(target) {
            return Err(format!("window {hwnd:#x} did not take the keys: {why}"));
        }
        std::thread::sleep(Duration::from_millis(SETTLE_MS));
        Ok(was.0 as isize)
    }
}

/// Whether the keyboard's keys go to `hwnd`.
///
/// `SendInput` puts the keystrokes into the input queue of the *foreground*
/// window's thread, so the foreground window being the target is the whole
/// test. The focused window of that thread may be a child, or none at all,
/// and the title still receives the key: the first version of this function
/// also demanded that `GetGUIThreadInfo` named the target as active or
/// focused, and refused every press this title would have taken (measured on
/// `run-20261125-215517.log`: `the window did not take the keys` twelve
/// times while the same sequence from outside pressed it first try).
///
/// The thread's own figures are kept for the warning only, so a log says what
/// was in front and what had the focus when a press was refused.
fn keys_go_to(hwnd: HWND, why: &mut String) -> bool {
    // SAFETY: plain queries of this thread's windows and of another's state.
    unsafe {
        if hwnd.0.is_null() {
            *why = "no window".to_string();
            return false;
        }
        let front = GetForegroundWindow();
        if front != hwnd {
            *why = format!("the front window is {:#x}, not this one", front.0 as isize);
            return false;
        }
        let thread = GetWindowThreadProcessId(hwnd, None);
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..GUITHREADINFO::default()
        };
        if thread != 0 && GetGUIThreadInfo(thread, &mut info).is_ok() {
            *why = format!(
                "in front; focus {:#x}, active {:#x}",
                info.hwndFocus.0 as isize, info.hwndActive.0 as isize
            );
        } else {
            *why = "in front".to_string();
        }
        true
    }
}

/// The sequence that works when the plain call is refused: share input state
/// with the thread that owns the window, then make it the active, topmost,
/// focused and front window, then let go of that thread. Answers whether the
/// keys go to it afterwards.
fn dance(target: HWND) -> bool {
    // SAFETY: plain window calls, each on a handle from the enumeration.
    unsafe {
        let me = GetCurrentThreadId();
        // The window's own thread, never the foreground window's: the title's
        // thread is the one whose focus this sets.
        let owner = GetWindowThreadProcessId(target, None);
        let attached = owner != 0 && owner != me && AttachThreadInput(me, owner, true).as_bool();
        if !IsWindowVisible(target).as_bool() {
            let _ = ShowWindow(target, SW_SHOW);
        }
        let _ = SetActiveWindow(target);
        let _ = BringWindowToTop(target);
        let _ = SetFocus(Some(target));
        let _ = SetForegroundWindow(target);
        if attached {
            let _ = AttachThreadInput(me, owner, false);
        }
        let mut why = String::new();
        keys_go_to(target, &mut why)
    }
}

/// Puts back the window [`activate`] answered, giving focus back after the last
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One window as a test writes it.
    fn window(
        handle: isize,
        parent: isize,
        class: &str,
        title: &str,
        visible: bool,
        client: (i32, i32),
    ) -> Window {
        Window {
            handle,
            parent,
            visible,
            client,
            class: Name::of(class),
            title: Name::of(title),
        }
    }

    /// The process's windows as they were enumerated on 2026-11-25: the
    /// title's own window, and the two IME windows of it, both children.
    fn measured() -> Vec<Window> {
        vec![
            window(
                0x3e29de,
                0,
                "sgaWindowClass",
                "Americas | VIctory e2729827",
                true,
                (1280, 720),
            ),
            window(0x8f27d2, 0x3e29de, "IME", "Default IME", false, (0, 0)),
            window(0x732b6c, 0x8f27d2, "MSCTFIME UI", "", false, (0, 0)),
        ]
    }

    /// The choice `windows` gives, with `owner` as the swap chain's window.
    fn chosen(windows: &[Window], owner: isize) -> Choice {
        let mut choice = Choice::new(owner);
        for window in windows {
            choice.see(*window);
        }
        choice
    }

    // The measured tree, which the old rule answered with whatever the swap
    // chain was given: the window with the title's input is the top-level
    // visible one, and its IME windows are children, so neither is a
    // candidate.
    #[test]
    fn the_measured_tree_chooses_the_titles_window() {
        let choice = chosen(&measured(), 0);
        assert_eq!(choice.window(), 0x3e29de);
        assert!(choice.class().is("sgaWindowClass"), "{}", choice.class());
        assert!(
            choice.title().is("Americas | VIctory e2729827"),
            "{}",
            choice.title()
        );
        // The window the swap chain was made for is not the answer when the
        // process shows a window that is not ours.
        assert_eq!(chosen(&measured(), 0xdead).window(), 0x3e29de);
    }

    // The layer's own window is bigger and still not the one to press into:
    // the title made its window after the swap chain did, so the presses used
    // to go into ours.
    #[test]
    fn the_layer_window_is_passed_over_however_large() {
        let mut windows = vec![window(0x1111, 0, OUR_CLASS, "VIctory", true, (2560, 1440))];
        windows.extend(measured());
        assert_eq!(chosen(&windows, 0x1111).window(), 0x3e29de);
    }

    // Only ours: the old answer, which is the right one.
    #[test]
    fn our_window_alone_is_the_answer() {
        let windows = [window(0x1111, 0, OUR_CLASS, "VIctory", true, (1280, 720))];
        assert_eq!(chosen(&windows, 0x1111).window(), 0x1111);
        assert_eq!(chosen(&windows, 0).window(), 0x1111);
    }

    // No visible top-level window: what `focus` was given before the rule
    // existed. A hidden window, and a window of another process, are not
    // candidates either.
    #[test]
    fn no_visible_window_falls_back_on_the_swap_chains() {
        let windows = [window(
            0x3e29de,
            0,
            "sgaWindowClass",
            "Americas",
            false,
            (1280, 720),
        )];
        assert_eq!(chosen(&windows, 0x3e29de).window(), 0x3e29de);
    }

    // A window's class name and title are kept whole up to the buffer, and the
    // comparison is not fooled by a prefix.
    #[test]
    fn a_name_is_not_matched_by_its_prefix() {
        assert!(Name::of("sgaWindowClass").is("sgaWindowClass"));
        assert!(!Name::of("sgaWindowClass2").is("sgaWindowClass"));
        assert!(!Name::of("sga").is("sgaWindowClass"));
        assert!(Name::EMPTY.is(""));
    }

    /// A desk that counts what a policy did instead of doing it: the windows
    /// it could see, the presses it got, and the clock the schedule runs on.
    #[derive(Default)]
    struct Desk {
        /// The process's windows, as they are at this press.
        windows: Vec<Window>,
        /// A window to put in their place after each press (a re-made one).
        replaces: Vec<Window>,
        /// The clock: how long since the window existed.
        now: Duration,
        /// Every window a press was aimed at, in order.
        targets: Vec<isize>,
        /// When each press was due, in order.
        pressed: Vec<Duration>,
        /// Every foreground given back.
        gave_back: Vec<isize>,
        /// Whether activation is refused, as a window that will not come
        /// forward is.
        refuses: bool,
    }

    impl Stage for Desk {
        fn target(&mut self) -> Choice {
            chosen(&self.windows, 0)
        }

        fn activate(&mut self, window: isize) -> Result<isize, String> {
            self.targets.push(window);
            if self.refuses {
                return Err("window did not take the keys".to_owned());
            }
            Ok(0)
        }

        fn press(&mut self) -> Result<(), String> {
            self.pressed.push(self.now);
            if let Some(window) = self.replaces.pop() {
                self.windows = vec![window];
            }
            Ok(())
        }

        fn wait(&mut self, due: Duration) {
            self.now = self.now.max(due);
        }

        fn give_back(&mut self, previous: isize) {
            self.gave_back.push(previous);
        }
    }

    /// A desk showing the title's window, and `enter` set as written here.
    fn pressing(after_s: u64, every_s: u64, times: u32) -> (Desk, LaunchPolicy) {
        let desk = Desk {
            windows: vec![window(
                0x3e29de,
                0,
                "sgaWindowClass",
                "Americas",
                true,
                (1280, 720),
            )],
            ..Desk::default()
        };
        let policy = LaunchPolicy {
            enter: true,
            enter_after_s: after_s,
            enter_every_s: every_s,
            enter_times: times,
            ..LaunchPolicy::default()
        };
        (desk, policy)
    }

    // Every press still lands where the schedule puts it: `after`, then every
    // `every` after that, and the foreground is given back once, after the
    // last.
    #[test]
    fn the_schedule_presses_after_and_then_every() {
        let (mut desk, policy) = pressing(3, 5, 4);
        run(&mut desk, policy);
        assert_eq!(
            desk.pressed,
            vec![
                Duration::from_secs(3),
                Duration::from_secs(8),
                Duration::from_secs(13),
                Duration::from_secs(18)
            ]
        );
        assert_eq!(desk.targets, vec![0x3e29de; 4]);
        assert_eq!(desk.gave_back, vec![0]);
    }

    // A window re-made between presses (device loss, a display change) is the
    // one the next press goes into: the window is looked up at press time.
    #[test]
    fn a_window_remade_is_pressed_into_from_the_next_press() {
        let (mut desk, policy) = pressing(0, 1, 2);
        desk.replaces = vec![window(
            0x4000,
            0,
            "sgaWindowClass",
            "Americas",
            true,
            (1600, 900),
        )];
        run(&mut desk, policy);
        assert_eq!(desk.targets, vec![0x3e29de, 0x4000]);
    }

    // A deadline that saturates still presses its turn: nothing panics, and
    // the count the schedule names is the count made.
    #[test]
    fn a_saturated_schedule_still_presses_its_count() {
        let (mut desk, policy) = pressing(u64::MAX, u64::MAX, 3);
        run(&mut desk, policy);
        assert_eq!(desk.pressed.len(), 3);
        assert_eq!(desk.pressed[2], Duration::MAX);
    }

    // A window that will not take the keys gets no Enter: the press would land
    // in whatever has them, which may be the user's own window.
    #[test]
    fn a_window_that_does_not_take_the_keys_gets_no_enter() {
        let (mut desk, policy) = pressing(0, 1, 2);
        desk.refuses = true;
        run(&mut desk, policy);
        assert!(desk.pressed.is_empty());
        assert_eq!(desk.targets.len(), 2);
        assert!(desk.gave_back.is_empty(), "nothing was taken to give back");
    }

    // `focus` alone brings the window forward once and keeps it there: no
    // presses, and nothing given back.
    #[test]
    fn focus_alone_is_kept() {
        let (mut desk, _) = pressing(0, 1, 0);
        run(
            &mut desk,
            LaunchPolicy {
                focus: true,
                ..LaunchPolicy::default()
            },
        );
        assert_eq!(desk.targets, vec![0x3e29de]);
        assert!(desk.pressed.is_empty());
        assert!(desk.gave_back.is_empty());
    }

    // A policy that does nothing installs nothing, and a run whose policy is
    // absent never starts a worker: both features are off by default.
    #[test]
    fn a_policy_that_does_nothing_installs_nothing() {
        install(LaunchPolicy::default());
        assert!(
            POLICY
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_none()
        );
        window_ready(0x3e29de);
        assert!(
            !STARTED.load(Ordering::SeqCst),
            "no window is looked at for an empty policy"
        );

        let enter = LaunchPolicy {
            enter: true,
            ..LaunchPolicy::default()
        };
        install(enter);
        assert!(
            (*POLICY.lock().unwrap_or_else(|error| error.into_inner()))
                .is_some_and(|policy| policy.enter)
        );
        // Leave the statics as they were found.
        install(LaunchPolicy::default());
    }
}
