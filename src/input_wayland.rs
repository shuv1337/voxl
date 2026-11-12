//! Input injection on Wayland: a wlr-virtual-pointer bound to the captured output and a
//! virtual keyboard with its own keymap. Keys arrive as keysyms; the keymap is a US
//! layout, so the keys of that layout keep their real keycodes, and any other symbol is
//! put on a spare keycode by uploading the keymap again.

use crate::hub::Hub;
use crate::input::InputEvent;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::Instant;
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

/// X keycodes (evdev + 8) with no entry in Chromium's DOM code table and no role in the
/// base keymap's modifier map. The keymap is the virtual keyboard's own, so these are
/// free to carry whatever symbol is needed next.
const SCRATCH: &[u32] = &[
    109, 120, 126, 138, 147, 149, 153, 154, 155, 156, 157, 159, 161, 162, 165, 168, 170, 178, 183,
    184, 185, 186, 208, 210, 211, 213, 219, 220, 221, 222, 223, 224, 226, 227, 228, 229, 230, 231,
    234, 235, 244, 245, 246, 247, 248, 249, 250, 251, 253, 254, 255,
];

const SHIFT: u32 = 1 << 0;
const CONTROL: u32 = 1 << 2;
const MOD1: u32 = 1 << 3;
const MOD4: u32 = 1 << 6;

const CAPS_LOCK: u32 = 0xffe5;
const NUM_LOCK: u32 = 0xff7f;
const ALT_GRAPH: u32 = 0xfe03;

/// Keysyms a keymap can name: the classic sets, Unicode code points and the XF86
/// (media and browser key) range. Anything else would fail the keymap the compositor
/// compiles, and with it the virtual keyboard.
fn valid_keysym(keysym: u32) -> bool {
    matches!(keysym, 0x20..=0xffff | 0x0100_0100..=0x0110_ffff | 0x1008_0000..=0x1008_ffff)
}

/// Real modifier a keysym holds down, in the keymap's modifier map.
fn mod_bit(keysym: u32) -> u32 {
    match keysym {
        0xffe1 | 0xffe2 => SHIFT,
        0xffe3 | 0xffe4 => CONTROL,
        0xffe7..=0xffea => MOD1,
        0xffeb..=0xffee => MOD4,
        _ => 0,
    }
}

/// Keysym -> (evdev keycode, shift level) for the US layout the keymap includes. Keys
/// that type the same thing with and without Shift have no level.
fn us_layout() -> HashMap<u32, (u32, Option<u8>)> {
    let mut m = HashMap::new();
    for (first, plain, shifted) in [
        (41, "`", "~"),
        (2, "1234567890-=", "!@#$%^&*()_+"),
        (16, "qwertyuiop[]", "QWERTYUIOP{}"),
        (30, "asdfghjkl;'", "ASDFGHJKL:\""),
        (43, "\\", "|"),
        (44, "zxcvbnm,./", "ZXCVBNM<>?"),
    ] {
        for (i, (p, s)) in plain.chars().zip(shifted.chars()).enumerate() {
            m.insert(p as u32, (first + i as u32, Some(0)));
            m.insert(s as u32, (first + i as u32, Some(1)));
        }
    }
    for (keysym, code) in [
        (0x20, 57),    // space
        (0xff1b, 1),   // Escape
        (0xff08, 14),  // BackSpace
        (0xff09, 15),  // Tab
        (0xff0d, 28),  // Return
        (0xffe3, 29),  // Control_L
        (0xffe1, 42),  // Shift_L
        (0xffe2, 54),  // Shift_R
        (0xffaa, 55),  // KP_Multiply
        (0xffe9, 56),  // Alt_L
        (0xffe7, 56),  // Meta_L
        (0xff14, 70),  // Scroll_Lock
        (0xffad, 74),  // KP_Subtract
        (0xffab, 78),  // KP_Add
        (0xffc8, 87),  // F11
        (0xffc9, 88),  // F12
        (0xff8d, 96),  // KP_Enter
        (0xffe4, 97),  // Control_R
        (0xffaf, 98),  // KP_Divide
        (0xff61, 99),  // Print
        (0xffea, 100), // Alt_R
        (0xffe8, 100), // Meta_R
        (0xff50, 102), // Home
        (0xff52, 103), // Up
        (0xff55, 104), // Prior
        (0xff51, 105), // Left
        (0xff53, 106), // Right
        (0xff57, 107), // End
        (0xff54, 108), // Down
        (0xff56, 109), // Next
        (0xff63, 110), // Insert
        (0xffff, 111), // Delete
        (0xff13, 119), // Pause
        (0xffeb, 125), // Super_L
        (0xffec, 126), // Super_R
        (0xff67, 127), // Menu
    ] {
        m.insert(keysym, (code, None));
    }
    m.insert(0xfe20, (15, Some(1))); // ISO_Left_Tab
    for i in 0..10 {
        m.insert(0xffbe + i, (59 + i, None)); // F1..F10
    }
    m
}

struct Keymap {
    fixed: HashMap<u32, (u32, Option<u8>)>,
    /// keysym -> (index into SCRATCH, last use).
    scratch: HashMap<u32, (usize, Instant)>,
}

impl Keymap {
    fn new() -> Self {
        Self {
            fixed: us_layout(),
            scratch: HashMap::new(),
        }
    }

    /// The xkb source the compositor compiles. Function keys are single-level so that
    /// Ctrl+Alt+F<n> stays a shortcut for the desktop and cannot switch the virtual
    /// terminal away from the session being streamed.
    fn text(&self) -> String {
        let mut s = String::from(
            "xkb_keymap {\nxkb_keycodes \"voxl\" { include \"evdev+aliases(qwerty)\"\n",
        );
        for (i, kc) in SCRATCH.iter().enumerate() {
            s.push_str(&format!("<VX{i:02}> = {kc};\n"));
        }
        s.push_str(
            "};\nxkb_types \"voxl\" { include \"complete\" };\n\
             xkb_compatibility \"voxl\" { include \"complete\" };\n\
             xkb_symbols \"voxl\" { include \"pc+us+inet(evdev)\"\n",
        );
        for n in 1..=12 {
            s.push_str(&format!(
                "key <FK{n:02}> {{ type = \"ONE_LEVEL\", symbols[Group1] = [ F{n} ] }};\n"
            ));
        }
        for (keysym, (i, _)) in &self.scratch {
            s.push_str(&format!("key <VX{i:02}> {{ [ {keysym:#x} ] }};\n"));
        }
        s.push_str("};\n};\n");
        s
    }

    /// The evdev keycode and shift level that type `keysym`, and whether the keymap had
    /// to change to make room for it. `held` keycodes are not reassigned.
    fn resolve(&mut self, keysym: u32, held: &[u32]) -> Option<(u32, Option<u8>, bool)> {
        if let Some(&(code, level)) = self.fixed.get(&keysym) {
            return Some((code, level, false));
        }
        if let Some((i, used)) = self.scratch.get_mut(&keysym) {
            *used = Instant::now();
            return Some((SCRATCH[*i] - 8, None, false));
        }
        let i = if self.scratch.len() < SCRATCH.len() {
            let taken: Vec<usize> = self.scratch.values().map(|&(i, _)| i).collect();
            (0..SCRATCH.len()).find(|i| !taken.contains(i))?
        } else {
            let (&oldest, &(i, _)) = self
                .scratch
                .iter()
                .filter(|(_, (i, _))| !held.contains(&(SCRATCH[*i] - 8)))
                .min_by_key(|(_, (_, used))| *used)?;
            self.scratch.remove(&oldest);
            i
        };
        self.scratch.insert(keysym, (i, Instant::now()));
        Some((SCRATCH[i] - 8, None, true))
    }
}

#[derive(Default)]
struct Globals {
    seat: Option<wl_seat::WlSeat>,
    pointers: Option<(ZwlrVirtualPointerManagerV1, u32)>,
    keyboards: Option<ZwpVirtualKeyboardManagerV1>,
    outputs: Vec<(wl_output::WlOutput, String)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" if s.seat.is_none() => s.seat = Some(registry.bind(name, 1, q, ())),
                "wl_output" if version >= 4 => s
                    .outputs
                    .push((registry.bind(name, 4, q, ()), String::new())),
                "zwlr_virtual_pointer_manager_v1" => {
                    let v = version.min(2);
                    s.pointers = Some((registry.bind(name, v, q, ()), v))
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    s.keyboards = Some(registry.bind(name, 1, q, ()))
                }
                _ => (),
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for Globals {
    fn event(
        s: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = s.outputs.iter_mut().find(|(o, _)| o == output) {
                o.1 = name;
            }
        }
    }
}
delegate_noop!(Globals: ignore wl_seat::WlSeat);
delegate_noop!(Globals: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(Globals: ignore ZwlrVirtualPointerV1);
delegate_noop!(Globals: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(Globals: ignore ZwpVirtualKeyboardV1);

struct Injector {
    conn: Connection,
    queue: EventQueue<Globals>,
    globals: Globals,
    pointer: ZwlrVirtualPointerV1,
    /// The output `pointer` is bound to.
    output: Option<String>,
    keyboard: ZwpVirtualKeyboardV1,
    hub: Arc<Hub>,
    start: Instant,
    last_ms: u32,
    keymap: Keymap,
    /// keysym -> evdev keycode it is holding down.
    pressed: HashMap<u32, u32>,
    mods: u32,
    buttons: [bool; 16],
    px_per_notch: f64,
    clipboard: Option<crate::desktop::ClipboardHandle>,
    failed: bool,
}

/// A virtual pointer bound to `output`, so absolute positions land on that output alone.
fn make_pointer(
    globals: &Globals,
    qh: &QueueHandle<Globals>,
    output: Option<&str>,
) -> ZwlrVirtualPointerV1 {
    let (pointers, version) = globals.pointers.as_ref().expect("checked in new");
    let seat = globals.seat.as_ref();
    let bound = globals
        .outputs
        .iter()
        .find(|(_, name)| output.is_none_or(|wanted| wanted == name));
    match bound {
        Some((o, _)) if *version >= 2 => {
            pointers.create_virtual_pointer_with_output(seat, Some(o), qh, ())
        }
        _ => {
            log::warn!("virtual pointer is not bound to an output; positions span every output");
            pointers.create_virtual_pointer(seat, qh, ())
        }
    }
}

/// X button numbers, as the client sends them, to evdev button codes.
fn evdev_button(button: u8) -> Option<u32> {
    Some(match button {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        8 => 0x113, // BTN_SIDE
        9 => 0x114, // BTN_EXTRA
        _ => return None,
    })
}

impl Injector {
    fn new(cfg: &crate::wayland::Config, hub: Arc<Hub>) -> Result<Self, String> {
        let conn = crate::wayland::connect(cfg.display.as_deref())?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut globals = Globals::default();
        // Once for the globals, once more for the output names.
        for _ in 0..2 {
            queue.roundtrip(&mut globals).map_err(|e| e.to_string())?;
        }
        let seat = globals.seat.as_ref().ok_or("compositor has no wl_seat")?;
        if globals.pointers.is_none() {
            return Err("compositor lacks wlr-virtual-pointer-unstable-v1".into());
        }
        let pointer = make_pointer(&globals, &qh, cfg.output.as_deref());
        let keyboard = globals
            .keyboards
            .as_ref()
            .ok_or("compositor lacks virtual-keyboard-unstable-v1")?
            .create_virtual_keyboard(seat, &qh, ());
        let mut me = Self {
            conn,
            queue,
            globals,
            pointer,
            output: cfg.output.clone(),
            keyboard,
            hub,
            start: Instant::now(),
            last_ms: 0,
            keymap: Keymap::new(),
            pressed: HashMap::new(),
            mods: 0,
            buttons: [false; 16],
            px_per_notch: 180.0,
            clipboard: None,
            failed: false,
        };
        me.upload_keymap()?;
        me.queue
            .roundtrip(&mut me.globals)
            .map_err(|e| e.to_string())?;
        Ok(me)
    }

    fn upload_keymap(&mut self) -> Result<(), String> {
        let mut text = self.keymap.text().into_bytes();
        text.push(0);
        let fd = unsafe { libc::memfd_create(c"voxl-keymap".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&text).map_err(|e| e.to_string())?;
        // 1: xkb_v1.
        self.keyboard.keymap(1, file.as_fd(), text.len() as u32);
        // A new keymap starts from a clean state.
        self.keyboard.modifiers(self.mods, 0, 0, 0);
        Ok(())
    }

    /// Milliseconds for an event. Presses and releases each get their own millisecond:
    /// clients treat equal timestamps as one event (no double-click, merged wheel clicks).
    fn time(&mut self, discrete: bool) -> u32 {
        let now = self.start.elapsed().as_millis() as u32;
        self.last_ms = now.max(self.last_ms + u32::from(discrete));
        self.last_ms
    }

    fn key(&mut self, code: u32, down: bool) {
        let t = self.time(true);
        self.keyboard.key(t, code, u32::from(down));
    }

    fn held_mods(&self) -> u32 {
        self.pressed.keys().fold(0, |m, &k| m | mod_bit(k))
    }

    /// The source follows its axis: compositors file it under the axis named last, and
    /// Hyprland resets it when a plain axis value arrives.
    fn wheel(&mut self, axis: wl_pointer::Axis, ticks: i32) {
        if ticks == 0 {
            return;
        }
        let ticks = ticks.clamp(-20, 20);
        let t = self.time(true);
        self.pointer
            .axis_discrete(t, axis, f64::from(ticks) * 15.0, ticks);
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        self.pointer.frame();
    }

    /// Rebinds the pointer when the video switched to another output, letting go of
    /// any buttons first so nothing stays pressed on the old one.
    fn follow_output(&mut self) {
        let current = self.hub.output.lock().clone();
        if current.is_none() || current == self.output {
            return;
        }
        for b in 0..self.buttons.len() {
            if std::mem::take(&mut self.buttons[b]) {
                if let Some(code) = evdev_button(b as u8) {
                    let t = self.time(true);
                    self.pointer
                        .button(t, code, wl_pointer::ButtonState::Released);
                    self.pointer.frame();
                }
            }
        }
        self.pointer.destroy();
        let qh = self.queue.handle();
        self.pointer = make_pointer(&self.globals, &qh, current.as_deref());
        self.output = current;
    }

    fn handle(&mut self, ev: InputEvent) {
        if matches!(
            ev,
            InputEvent::Move { .. }
                | InputEvent::Button { .. }
                | InputEvent::Scroll { .. }
                | InputEvent::ScrollPx { .. }
        ) {
            self.follow_output();
        }
        match ev {
            InputEvent::Move { x, y } => {
                let w = self.hub.width.load(Ordering::Relaxed);
                let h = self.hub.height.load(Ordering::Relaxed);
                if w == 0 || h == 0 {
                    return;
                }
                let t = self.time(false);
                self.pointer.motion_absolute(
                    t,
                    x.clamp(0, w as i32 - 1) as u32,
                    y.clamp(0, h as i32 - 1) as u32,
                    w,
                    h,
                );
                self.pointer.frame();
            }
            InputEvent::Button { button, down } => {
                let Some(code) = evdev_button(button) else {
                    return;
                };
                if self.buttons[button as usize] == down {
                    return;
                }
                self.buttons[button as usize] = down;
                let t = self.time(true);
                self.pointer.button(
                    t,
                    code,
                    if down {
                        wl_pointer::ButtonState::Pressed
                    } else {
                        wl_pointer::ButtonState::Released
                    },
                );
                self.pointer.frame();
            }
            InputEvent::Scroll { dx, dy } => {
                self.wheel(wl_pointer::Axis::VerticalScroll, dy);
                self.wheel(wl_pointer::Axis::HorizontalScroll, dx);
            }
            InputEvent::ScrollPx { dx, dy } => {
                // Continuous scrolling is in logical pixels, where a wheel notch is
                // about 120; --scroll-px-per-notch says how many desktop pixels that is.
                let k = 120.0 / self.px_per_notch;
                let t = self.time(false);
                for (axis, d) in [
                    (wl_pointer::Axis::VerticalScroll, dy),
                    (wl_pointer::Axis::HorizontalScroll, dx),
                ] {
                    if d != 0.0 {
                        self.pointer.axis(t, axis, d * k);
                        self.pointer.axis_source(wl_pointer::AxisSource::Continuous);
                    }
                }
                self.pointer.frame();
            }
            InputEvent::Key { keysym, down } => {
                // Latin-1 arrives either directly or as a Unicode keysym.
                let keysym = match keysym {
                    0x0100_0020..=0x0100_007e | 0x0100_00a0..=0x0100_00ff => keysym - 0x0100_0000,
                    k => k,
                };
                // The viewer's own lock state already chose each character; a lock here
                // as well would undo it. AltGr likewise only picks the viewer's symbol.
                if matches!(keysym, CAPS_LOCK | NUM_LOCK | ALT_GRAPH) || !valid_keysym(keysym) {
                    return;
                }
                if !down {
                    let Some(code) = self.pressed.remove(&keysym) else {
                        return;
                    };
                    if !self.pressed.values().any(|&c| c == code) {
                        self.key(code, false);
                    }
                    if mod_bit(keysym) != 0 {
                        self.mods = self.held_mods();
                        self.keyboard.modifiers(self.mods, 0, 0, 0);
                    }
                    return;
                }
                // The compositor repeats a held key for its clients.
                if self.pressed.contains_key(&keysym) {
                    return;
                }
                let held: Vec<u32> = self.pressed.values().copied().collect();
                let Some((code, level, changed)) = self.keymap.resolve(keysym, &held) else {
                    return;
                };
                if changed {
                    if let Err(e) = self.upload_keymap() {
                        log::warn!("keymap upload failed: {e}");
                        return;
                    }
                }
                self.pressed.insert(keysym, code);
                if held.contains(&code) {
                    return;
                }
                if mod_bit(keysym) != 0 {
                    self.key(code, true);
                    self.mods = self.held_mods();
                    self.keyboard.modifiers(self.mods, 0, 0, 0);
                    return;
                }
                // The symbol sits on the other shift level than the one being held:
                // flip Shift for this one press.
                let shifted = self.mods & SHIFT != 0;
                let flip = level.is_some_and(|l| (l == 1) != shifted);
                if flip {
                    self.keyboard.modifiers(self.mods ^ SHIFT, 0, 0, 0);
                }
                self.key(code, true);
                if flip {
                    self.keyboard.modifiers(self.mods, 0, 0, 0);
                }
            }
            InputEvent::Clipboard(content) => {
                if let Some(c) = &self.clipboard {
                    c.set_blocking(content);
                }
            }
            InputEvent::ReleaseAll | InputEvent::Shutdown => {
                let mut codes: Vec<u32> = std::mem::take(&mut self.pressed).into_values().collect();
                codes.sort_unstable();
                codes.dedup();
                for code in codes {
                    self.key(code, false);
                }
                self.mods = 0;
                self.keyboard.modifiers(0, 0, 0, 0);
                for b in 0..self.buttons.len() {
                    if std::mem::take(&mut self.buttons[b]) {
                        if let Some(code) = evdev_button(b as u8) {
                            let t = self.time(true);
                            self.pointer
                                .button(t, code, wl_pointer::ButtonState::Released);
                            self.pointer.frame();
                        }
                    }
                }
            }
        }
    }

    /// Sends what was queued and drains what the compositor sent, so its side of the
    /// socket never fills.
    fn flush(&mut self) {
        let r = crate::wayland::dispatch(&self.conn, &mut self.queue, &mut self.globals, 0);
        if let Err(e) = r {
            if !self.failed {
                log::error!("Wayland input connection: {e}");
            }
            self.failed = true;
        }
    }
}

/// The cursor message for a small dot. The compositor paints the real cursor into the
/// capture, so the viewer's own pointer only has to show where it is ahead of the stream.
pub fn dot_cursor() -> bytes::Bytes {
    const SIZE: u16 = 24;
    let mut msg = vec![0x03, 0];
    for v in [SIZE, SIZE, SIZE / 2, SIZE / 2] {
        msg.extend_from_slice(&v.to_be_bytes());
    }
    msg.extend_from_slice(&1u32.to_be_bytes());
    let c = f32::from(SIZE) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = (f32::from(x) + 0.5 - c).hypot(f32::from(y) + 0.5 - c);
            // A white dot with a dark rim, each edge one pixel soft.
            let white = (3.0 - d).clamp(0.0, 1.0);
            let alpha = (4.5 - d).clamp(0.0, 1.0);
            let v = (white * 255.0) as u8;
            msg.extend_from_slice(&[v, v, v, (alpha * 200.0) as u8]);
        }
    }
    msg.into()
}

pub fn spawn(
    cfg: &crate::wayland::Config,
    hub: Arc<Hub>,
    clipboard: Option<crate::desktop::ClipboardHandle>,
    px_per_notch: f64,
) -> Result<mpsc::Sender<InputEvent>, String> {
    let mut inj = Injector::new(cfg, hub)?;
    inj.clipboard = clipboard;
    inj.px_per_notch = px_per_notch.max(1.0);
    log::info!(
        "Wayland input: virtual pointer and keyboard, {} spare keycodes",
        SCRATCH.len()
    );
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut pending_move: Option<(i32, i32)> = None;
                let handle =
                    |inj: &mut Injector, ev: InputEvent, pm: &mut Option<(i32, i32)>| match ev {
                        InputEvent::Move { x, y } => *pm = Some((x, y)),
                        other => {
                            if let Some((x, y)) = pm.take() {
                                inj.handle(InputEvent::Move { x, y });
                            }
                            inj.handle(other);
                        }
                    };
                handle(&mut inj, first, &mut pending_move);
                // Coalesce whatever queued up meanwhile; moves collapse to the latest.
                while let Ok(ev) = rx.try_recv() {
                    handle(&mut inj, ev, &mut pending_move);
                }
                if let Some((x, y)) = pending_move {
                    inj.handle(InputEvent::Move { x, y });
                }
                inj.flush();
            }
        })
        .unwrap();
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_keys_keep_their_keycodes_and_levels() {
        let mut k = Keymap::new();
        assert_eq!(k.resolve('a' as u32, &[]), Some((30, Some(0), false)));
        assert_eq!(k.resolve('A' as u32, &[]), Some((30, Some(1), false)));
        assert_eq!(k.resolve('?' as u32, &[]), Some((53, Some(1), false)));
        assert_eq!(k.resolve(0xff0d, &[]), Some((28, None, false)));
        assert_eq!(k.resolve(0xffc9, &[]), Some((88, None, false)));
    }

    #[test]
    fn other_symbols_take_spare_keycodes_and_reuse_the_oldest() {
        let mut k = Keymap::new();
        let euro = 0x0100_20ac;
        assert_eq!(k.resolve(euro, &[]), Some((SCRATCH[0] - 8, None, true)));
        assert_eq!(k.resolve(euro, &[]), Some((SCRATCH[0] - 8, None, false)));
        assert!(k.text().contains("key <VX00> { [ 0x10020ac ] };"));
        for i in 1..SCRATCH.len() as u32 {
            assert!(k.resolve(0x0100_4e00 + i, &[]).unwrap().2);
        }
        // Full: the euro sign is the oldest, unless its key is held down.
        let held = [SCRATCH[0] - 8];
        assert_eq!(
            k.resolve(0x0100_5000, &held),
            Some((SCRATCH[1] - 8, None, true))
        );
        assert_eq!(
            k.resolve(0x0100_5001, &[]),
            Some((SCRATCH[0] - 8, None, true))
        );
        assert!(!k.scratch.contains_key(&euro));
    }

    #[test]
    fn scratch_keycodes_avoid_the_us_layout() {
        let used: Vec<u32> = us_layout().values().map(|&(c, _)| c + 8).collect();
        assert!(SCRATCH.iter().all(|kc| !used.contains(kc)));
    }
}
