use agent_proto::{ActionKind, ActionRequest, MouseButton};
use anyhow::{Context, Result, bail};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::Write,
    mem,
    os::raw::{c_char, c_int, c_uint, c_ulong},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    thread,
    time::Duration,
};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const KEY_ESC: u16 = 1;
const KEY_1: u16 = 2;
const KEY_2: u16 = 3;
const KEY_3: u16 = 4;
const KEY_4: u16 = 5;
const KEY_5: u16 = 6;
const KEY_6: u16 = 7;
const KEY_7: u16 = 8;
const KEY_8: u16 = 9;
const KEY_9: u16 = 10;
const KEY_0: u16 = 11;
const KEY_MINUS: u16 = 12;
const KEY_EQUAL: u16 = 13;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_Q: u16 = 16;
const KEY_W: u16 = 17;
const KEY_E: u16 = 18;
const KEY_R: u16 = 19;
const KEY_T: u16 = 20;
const KEY_Y: u16 = 21;
const KEY_U: u16 = 22;
const KEY_I: u16 = 23;
const KEY_O: u16 = 24;
const KEY_P: u16 = 25;
const KEY_LEFTBRACE: u16 = 26;
const KEY_RIGHTBRACE: u16 = 27;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
const KEY_A: u16 = 30;
const KEY_S: u16 = 31;
const KEY_D: u16 = 32;
const KEY_F: u16 = 33;
const KEY_G: u16 = 34;
const KEY_H: u16 = 35;
const KEY_J: u16 = 36;
const KEY_K: u16 = 37;
const KEY_L: u16 = 38;
const KEY_SEMICOLON: u16 = 39;
const KEY_APOSTROPHE: u16 = 40;
const KEY_GRAVE: u16 = 41;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_BACKSLASH: u16 = 43;
const KEY_Z: u16 = 44;
const KEY_X: u16 = 45;
const KEY_C: u16 = 46;
const KEY_V: u16 = 47;
const KEY_B: u16 = 48;
const KEY_N: u16 = 49;
const KEY_M: u16 = 50;
const KEY_COMMA: u16 = 51;
const KEY_DOT: u16 = 52;
const KEY_SLASH: u16 = 53;
const KEY_LEFTALT: u16 = 56;
const KEY_SPACE: u16 = 57;
const KEY_F1: u16 = 59;
const KEY_UP: u16 = 103;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_DOWN: u16 = 108;
const KEY_DELETE: u16 = 111;
const KEY_LEFTMETA: u16 = 125;

const BUS_USB: u16 = 0x03;
const UI_DEV_CREATE: libc::c_ulong = ioc(0, b'U', 1, 0);
const UI_DEV_DESTROY: libc::c_ulong = ioc(0, b'U', 2, 0);
const UI_SET_EVBIT: libc::c_ulong = ioc(1, b'U', 100, mem::size_of::<libc::c_int>());
const UI_SET_KEYBIT: libc::c_ulong = ioc(1, b'U', 101, mem::size_of::<libc::c_int>());
const UI_SET_RELBIT: libc::c_ulong = ioc(1, b'U', 102, mem::size_of::<libc::c_int>());
const UI_SET_ABSBIT: libc::c_ulong = ioc(1, b'U', 103, mem::size_of::<libc::c_int>());

#[repr(C)]
struct Display {
    _private: [u8; 0],
}

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(display_name: *const c_char) -> *mut Display;
    fn XCloseDisplay(display: *mut Display) -> c_int;
    fn XFlush(display: *mut Display) -> c_int;
}

#[link(name = "Xtst")]
unsafe extern "C" {
    fn XTestFakeMotionEvent(
        display: *mut Display,
        screen_number: c_int,
        x: c_int,
        y: c_int,
        delay: c_ulong,
    ) -> c_int;
    fn XTestFakeButtonEvent(
        display: *mut Display,
        button: c_uint,
        is_press: c_int,
        delay: c_ulong,
    ) -> c_int;
}

const fn ioc(dir: libc::c_ulong, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    const NRSHIFT: libc::c_ulong = 0;
    const TYPESHIFT: libc::c_ulong = 8;
    const SIZESHIFT: libc::c_ulong = 16;
    const DIRSHIFT: libc::c_ulong = 30;
    (dir << DIRSHIFT)
        | ((ty as libc::c_ulong) << TYPESHIFT)
        | ((nr as libc::c_ulong) << NRSHIFT)
        | ((size as libc::c_ulong) << SIZESHIFT)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UInputUserDev {
    name: [u8; 80],
    id: InputId,
    ff_effects_max: u32,
    absmax: [i32; 64],
    absmin: [i32; 64],
    absfuzz: [i32; 64],
    absflat: [i32; 64],
}

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

pub struct UInputInjector {
    file: File,
    screen_width: i32,
    screen_height: i32,
}

impl UInputInjector {
    pub fn new(screen_width: i32, screen_height: i32) -> Result<Self> {
        let mut file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")
            .context("open /dev/uinput")?;

        set_bit(&file, UI_SET_EVBIT, EV_KEY)?;
        set_bit(&file, UI_SET_EVBIT, EV_REL)?;
        set_bit(&file, UI_SET_EVBIT, EV_ABS)?;
        for key in 1..=255 {
            set_bit(&file, UI_SET_KEYBIT, key)?;
        }
        set_bit(&file, UI_SET_KEYBIT, BTN_LEFT)?;
        set_bit(&file, UI_SET_KEYBIT, BTN_RIGHT)?;
        set_bit(&file, UI_SET_KEYBIT, BTN_MIDDLE)?;
        set_bit(&file, UI_SET_RELBIT, REL_X)?;
        set_bit(&file, UI_SET_RELBIT, REL_Y)?;
        set_bit(&file, UI_SET_RELBIT, REL_WHEEL)?;
        set_bit(&file, UI_SET_RELBIT, REL_HWHEEL)?;
        set_bit(&file, UI_SET_ABSBIT, ABS_X)?;
        set_bit(&file, UI_SET_ABSBIT, ABS_Y)?;

        let mut dev = UInputUserDev {
            name: [0; 80],
            id: InputId {
                bustype: BUS_USB,
                vendor: 0x0A11,
                product: 0x0001,
                version: 1,
            },
            ff_effects_max: 0,
            absmax: [0; 64],
            absmin: [0; 64],
            absfuzz: [0; 64],
            absflat: [0; 64],
        };
        let name = b"os-agent-uinput";
        dev.name[..name.len()].copy_from_slice(name);
        dev.absmax[ABS_X as usize] = 65_535;
        dev.absmax[ABS_Y as usize] = 65_535;

        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&dev as *const UInputUserDev).cast::<u8>(),
                mem::size_of::<UInputUserDev>(),
            )
        };
        file.write_all(bytes).context("write uinput_user_dev")?;
        ioctl_none(&file, UI_DEV_CREATE).context("UI_DEV_CREATE")?;
        thread::sleep(Duration::from_millis(100));

        Ok(Self {
            file,
            screen_width: screen_width.max(1),
            screen_height: screen_height.max(1),
        })
    }

    pub fn handle(&mut self, req: &ActionRequest) -> Result<()> {
        match &req.kind {
            ActionKind::MovePointer { x, y, absolute } => self.move_pointer(*x, *y, *absolute),
            ActionKind::Click {
                button,
                count,
                x,
                y,
            } => self.click(req.display_id.as_str(), button, *count, *x, *y),
            ActionKind::Scroll { dx, dy } => {
                if *dx != 0 {
                    self.emit(EV_REL, REL_HWHEEL, *dx)?;
                }
                if *dy != 0 {
                    self.emit(EV_REL, REL_WHEEL, *dy)?;
                }
                self.syn()
            }
            ActionKind::TypeText { text } => self.type_text(text),
            ActionKind::KeyCombo { keys } => self.key_combo(keys),
            ActionKind::Drag { from, to } => {
                self.move_pointer(from.0, from.1, true)?;
                self.key(BTN_LEFT, true)?;
                self.move_pointer(to.0, to.1, true)?;
                self.key(BTN_LEFT, false)
            }
            ActionKind::Finish { .. } => Ok(()),
            ActionKind::Noop => Ok(()),
        }
    }

    fn move_pointer(&mut self, x: i32, y: i32, absolute: bool) -> Result<()> {
        if absolute {
            let nx = normalize_abs(x, self.screen_width);
            let ny = normalize_abs(y, self.screen_height);
            self.emit(EV_ABS, ABS_X, nx)?;
            self.emit(EV_ABS, ABS_Y, ny)?;
        } else {
            self.emit(EV_REL, REL_X, x)?;
            self.emit(EV_REL, REL_Y, y)?;
        }
        self.syn()
    }

    fn click(
        &mut self,
        display_id: &str,
        button: &MouseButton,
        count: u8,
        x: Option<i32>,
        y: Option<i32>,
    ) -> Result<()> {
        if let (Some(x), Some(y)) = (x, y) {
            if xtest_click(display_id, button, count, x, y).is_ok() {
                return Ok(());
            }
            self.move_pointer(x, y, true)?;
            thread::sleep(Duration::from_millis(5));
        }
        let code = match button {
            MouseButton::Left => BTN_LEFT,
            MouseButton::Middle => BTN_MIDDLE,
            MouseButton::Right => BTN_RIGHT,
        };
        for _ in 0..count.max(1) {
            self.key(code, true)?;
            self.key(code, false)?;
        }
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            let (code, shift) = char_key(ch)?;
            if shift {
                self.key(KEY_LEFTSHIFT, true)?;
            }
            self.key(code, true)?;
            self.key(code, false)?;
            if shift {
                self.key(KEY_LEFTSHIFT, false)?;
            }
        }
        Ok(())
    }

    fn key_combo(&mut self, keys: &[String]) -> Result<()> {
        let mut parsed = Vec::with_capacity(keys.len());
        for key in keys {
            parsed.push(named_key(key)?);
        }
        for key in &parsed {
            self.key(*key, true)?;
        }
        for key in parsed.iter().rev() {
            self.key(*key, false)?;
        }
        Ok(())
    }

    fn key(&mut self, code: u16, down: bool) -> Result<()> {
        self.emit(EV_KEY, code, i32::from(down))?;
        self.syn()
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) -> Result<()> {
        let event = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&event as *const InputEvent).cast::<u8>(),
                mem::size_of::<InputEvent>(),
            )
        };
        self.file.write_all(bytes)?;
        Ok(())
    }

    fn syn(&mut self) -> Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }
}

impl Drop for UInputInjector {
    fn drop(&mut self) {
        let _ = ioctl_none(&self.file, UI_DEV_DESTROY);
    }
}

fn set_bit(file: &File, request: libc::c_ulong, value: u16) -> Result<()> {
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, value as libc::c_int) };
    if result < 0 {
        return Err(std::io::Error::last_os_error()).context("uinput ioctl set bit");
    }
    Ok(())
}

fn ioctl_none(file: &File, request: libc::c_ulong) -> Result<()> {
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request) };
    if result < 0 {
        return Err(std::io::Error::last_os_error()).context("uinput ioctl");
    }
    Ok(())
}

fn normalize_abs(value: i32, extent: i32) -> i32 {
    let value = value.clamp(0, extent.saturating_sub(1));
    ((value as i64 * 65_535) / extent.max(1) as i64) as i32
}

fn xtest_click(display_id: &str, button: &MouseButton, count: u8, x: i32, y: i32) -> Result<()> {
    let display_name = CString::new(display_id)?;
    let display = unsafe { XOpenDisplay(display_name.as_ptr()) };
    if display.is_null() {
        bail!("XOpenDisplay failed for {display_id}");
    }

    let result = (|| {
        let button_number = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
        };
        if unsafe { XTestFakeMotionEvent(display, -1, x, y, 0) } == 0 {
            bail!("XTestFakeMotionEvent failed");
        }
        for _ in 0..count.max(1) {
            if unsafe { XTestFakeButtonEvent(display, button_number, 1, 0) } == 0 {
                bail!("XTestFakeButtonEvent press failed");
            }
            if unsafe { XTestFakeButtonEvent(display, button_number, 0, 0) } == 0 {
                bail!("XTestFakeButtonEvent release failed");
            }
        }
        unsafe {
            XFlush(display);
        }
        Ok(())
    })();

    unsafe {
        XCloseDisplay(display);
    }
    result
}

fn char_key(ch: char) -> Result<(u16, bool)> {
    let mapped = match ch {
        'a' => (KEY_A, false),
        'b' => (KEY_B, false),
        'c' => (KEY_C, false),
        'd' => (KEY_D, false),
        'e' => (KEY_E, false),
        'f' => (KEY_F, false),
        'g' => (KEY_G, false),
        'h' => (KEY_H, false),
        'i' => (KEY_I, false),
        'j' => (KEY_J, false),
        'k' => (KEY_K, false),
        'l' => (KEY_L, false),
        'm' => (KEY_M, false),
        'n' => (KEY_N, false),
        'o' => (KEY_O, false),
        'p' => (KEY_P, false),
        'q' => (KEY_Q, false),
        'r' => (KEY_R, false),
        's' => (KEY_S, false),
        't' => (KEY_T, false),
        'u' => (KEY_U, false),
        'v' => (KEY_V, false),
        'w' => (KEY_W, false),
        'x' => (KEY_X, false),
        'y' => (KEY_Y, false),
        'z' => (KEY_Z, false),
        'A' => (KEY_A, true),
        'B' => (KEY_B, true),
        'C' => (KEY_C, true),
        'D' => (KEY_D, true),
        'E' => (KEY_E, true),
        'F' => (KEY_F, true),
        'G' => (KEY_G, true),
        'H' => (KEY_H, true),
        'I' => (KEY_I, true),
        'J' => (KEY_J, true),
        'K' => (KEY_K, true),
        'L' => (KEY_L, true),
        'M' => (KEY_M, true),
        'N' => (KEY_N, true),
        'O' => (KEY_O, true),
        'P' => (KEY_P, true),
        'Q' => (KEY_Q, true),
        'R' => (KEY_R, true),
        'S' => (KEY_S, true),
        'T' => (KEY_T, true),
        'U' => (KEY_U, true),
        'V' => (KEY_V, true),
        'W' => (KEY_W, true),
        'X' => (KEY_X, true),
        'Y' => (KEY_Y, true),
        'Z' => (KEY_Z, true),
        '1' => (KEY_1, false),
        '2' => (KEY_2, false),
        '3' => (KEY_3, false),
        '4' => (KEY_4, false),
        '5' => (KEY_5, false),
        '6' => (KEY_6, false),
        '7' => (KEY_7, false),
        '8' => (KEY_8, false),
        '9' => (KEY_9, false),
        '0' => (KEY_0, false),
        '!' => (KEY_1, true),
        '@' => (KEY_2, true),
        '#' => (KEY_3, true),
        '$' => (KEY_4, true),
        '%' => (KEY_5, true),
        '^' => (KEY_6, true),
        '&' => (KEY_7, true),
        '*' => (KEY_8, true),
        '(' => (KEY_9, true),
        ')' => (KEY_0, true),
        '-' => (KEY_MINUS, false),
        '_' => (KEY_MINUS, true),
        '=' => (KEY_EQUAL, false),
        '+' => (KEY_EQUAL, true),
        '[' => (KEY_LEFTBRACE, false),
        '{' => (KEY_LEFTBRACE, true),
        ']' => (KEY_RIGHTBRACE, false),
        '}' => (KEY_RIGHTBRACE, true),
        ';' => (KEY_SEMICOLON, false),
        ':' => (KEY_SEMICOLON, true),
        '\'' => (KEY_APOSTROPHE, false),
        '"' => (KEY_APOSTROPHE, true),
        '`' => (KEY_GRAVE, false),
        '~' => (KEY_GRAVE, true),
        '\\' => (KEY_BACKSLASH, false),
        '|' => (KEY_BACKSLASH, true),
        ',' => (KEY_COMMA, false),
        '<' => (KEY_COMMA, true),
        '.' => (KEY_DOT, false),
        '>' => (KEY_DOT, true),
        '/' => (KEY_SLASH, false),
        '?' => (KEY_SLASH, true),
        ' ' => (KEY_SPACE, false),
        '\n' => (KEY_ENTER, false),
        '\t' => (KEY_TAB, false),
        _ => bail!("unsupported character for uinput typing: {ch:?}"),
    };
    Ok(mapped)
}

fn named_key(name: &str) -> Result<u16> {
    let key = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "leftctrl" => KEY_LEFTCTRL,
        "alt" | "option" | "leftalt" => KEY_LEFTALT,
        "shift" | "leftshift" => KEY_LEFTSHIFT,
        "super" | "meta" | "cmd" | "win" | "leftmeta" => KEY_LEFTMETA,
        "enter" | "return" => KEY_ENTER,
        "tab" => KEY_TAB,
        "esc" | "escape" => KEY_ESC,
        "space" => KEY_SPACE,
        "backspace" => KEY_BACKSPACE,
        "delete" | "del" => KEY_DELETE,
        "left" | "arrowleft" | "leftarrow" => KEY_LEFT,
        "right" | "arrowright" | "rightarrow" => KEY_RIGHT,
        "up" | "arrowup" | "uparrow" => KEY_UP,
        "down" | "arrowdown" | "downarrow" => KEY_DOWN,
        "f1" => KEY_F1,
        single if single.chars().count() == 1 => {
            let ch = single
                .chars()
                .next()
                .ok_or_else(|| anyhow::anyhow!("empty key name"))?;
            char_key(ch)?.0
        }
        _ => bail!("unsupported key name: {name}"),
    };
    Ok(key)
}
