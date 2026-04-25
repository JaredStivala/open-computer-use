use agent_common::monotonic_ns;
use agent_proto::{BusMessage, CaptureFrame, FrameEncoding, Rect};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use std::{
    ffi::CString,
    os::raw::{c_char, c_int, c_long, c_short, c_uint, c_ulong, c_ushort, c_void},
    ptr,
    sync::atomic::{AtomicI32, Ordering},
    sync::mpsc,
    thread,
};
use tokio::sync::broadcast;

const ZPIXMAP: c_int = 2;
const ALL_PLANES: c_ulong = !0;
const X_DAMAGE_NOTIFY: c_int = 0;
const X_DAMAGE_REPORT_BOUNDING_BOX: c_int = 2;
const IPC_PRIVATE: c_int = 0;
const IPC_RMID: c_int = 0;
static LAST_X_ERROR: AtomicI32 = AtomicI32::new(0);

#[repr(C)]
struct Display {
    _private: [u8; 0],
}

#[repr(C)]
struct Visual {
    _private: [u8; 0],
}

type Window = c_ulong;
type Drawable = c_ulong;
type Damage = c_ulong;
type Bool = c_int;
type Status = c_int;
type Time = c_ulong;
type ShmSeg = c_ulong;

#[repr(C)]
struct XShmSegmentInfo {
    shmseg: ShmSeg,
    shmid: c_int,
    shmaddr: *mut c_char,
    read_only: Bool,
}

#[repr(C)]
struct XImage {
    width: c_int,
    height: c_int,
    xoffset: c_int,
    format: c_int,
    data: *mut c_char,
    byte_order: c_int,
    bitmap_unit: c_int,
    bitmap_bit_order: c_int,
    bitmap_pad: c_int,
    depth: c_int,
    bytes_per_line: c_int,
    bits_per_pixel: c_int,
    red_mask: c_ulong,
    green_mask: c_ulong,
    blue_mask: c_ulong,
    obdata: *mut c_char,
    funcs: [usize; 10],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XRectangle {
    x: c_short,
    y: c_short,
    width: c_ushort,
    height: c_ushort,
}

#[repr(C)]
union XEvent {
    type_: c_int,
    pad: [c_long; 24],
}

#[repr(C)]
struct XDamageNotifyEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: Bool,
    display: *mut Display,
    drawable: Drawable,
    damage: Damage,
    level: c_int,
    more: Bool,
    timestamp: Time,
    area: XRectangle,
    geometry: XRectangle,
}

#[repr(C)]
struct XErrorEvent {
    type_: c_int,
    display: *mut Display,
    resourceid: c_ulong,
    serial: c_ulong,
    error_code: u8,
    request_code: u8,
    minor_code: u8,
}

type XErrorHandler = Option<unsafe extern "C" fn(*mut Display, *mut XErrorEvent) -> c_int>;

#[link(name = "X11")]
unsafe extern "C" {
    fn XInitThreads() -> c_int;
    fn XOpenDisplay(display_name: *const c_char) -> *mut Display;
    fn XCloseDisplay(display: *mut Display) -> c_int;
    fn XDefaultScreen(display: *mut Display) -> c_int;
    fn XRootWindow(display: *mut Display, screen_number: c_int) -> Window;
    fn XDisplayWidth(display: *mut Display, screen_number: c_int) -> c_int;
    fn XDisplayHeight(display: *mut Display, screen_number: c_int) -> c_int;
    fn XDefaultVisual(display: *mut Display, screen_number: c_int) -> *mut Visual;
    fn XDefaultDepth(display: *mut Display, screen_number: c_int) -> c_int;
    fn XNextEvent(display: *mut Display, event_return: *mut XEvent) -> c_int;
    fn XSync(display: *mut Display, discard: Bool) -> c_int;
    fn XDestroyImage(ximage: *mut XImage) -> c_int;
    fn XGetImage(
        display: *mut Display,
        d: Drawable,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        plane_mask: c_ulong,
        format: c_int,
    ) -> *mut XImage;
    fn XSetErrorHandler(handler: XErrorHandler) -> XErrorHandler;
}

#[link(name = "Xext")]
unsafe extern "C" {
    fn XShmQueryExtension(display: *mut Display) -> Bool;
    fn XShmCreateImage(
        display: *mut Display,
        visual: *mut Visual,
        depth: c_uint,
        format: c_int,
        data: *mut c_char,
        shminfo: *mut XShmSegmentInfo,
        width: c_uint,
        height: c_uint,
    ) -> *mut XImage;
    fn XShmAttach(display: *mut Display, shminfo: *mut XShmSegmentInfo) -> Status;
    fn XShmDetach(display: *mut Display, shminfo: *mut XShmSegmentInfo) -> Status;
    fn XShmGetImage(
        display: *mut Display,
        d: Drawable,
        image: *mut XImage,
        x: c_int,
        y: c_int,
        plane_mask: c_ulong,
    ) -> Status;
}

#[link(name = "Xdamage")]
unsafe extern "C" {
    fn XDamageQueryExtension(
        display: *mut Display,
        event_base_return: *mut c_int,
        error_base_return: *mut c_int,
    ) -> Bool;
    fn XDamageCreate(display: *mut Display, drawable: Drawable, level: c_int) -> Damage;
    fn XDamageSubtract(display: *mut Display, damage: Damage, repair: c_ulong, parts: c_ulong);
    fn XDamageDestroy(display: *mut Display, damage: Damage);
}

#[link(name = "Xcomposite")]
unsafe extern "C" {
    fn XCompositeQueryExtension(
        display: *mut Display,
        event_base_return: *mut c_int,
        error_base_return: *mut c_int,
    ) -> Bool;
}

#[link(name = "turbojpeg")]
unsafe extern "C" {
    fn tjInitCompress() -> *mut c_void;
    fn tjCompress2(
        handle: *mut c_void,
        src_buf: *const u8,
        width: c_int,
        pitch: c_int,
        height: c_int,
        pixel_format: c_int,
        jpeg_buf: *mut *mut u8,
        jpeg_size: *mut c_ulong,
        jpeg_subsamp: c_int,
        jpeg_qual: c_int,
        flags: c_int,
    ) -> c_int;
    fn tjFree(buffer: *mut u8);
    fn tjDestroy(handle: *mut c_void) -> c_int;
    fn tjGetErrorStr2(handle: *mut c_void) -> *const c_char;
}

pub fn run_capture_loop(display_id: String, tx: broadcast::Sender<BusMessage>) -> Result<()> {
    static X_THREADS: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    X_THREADS.get_or_init(|| unsafe {
        XInitThreads();
    });

    if std::env::var("AGENT_CAPTURE_DISABLE_XSHM").as_deref() == Ok("1") {
        return run_xgetimage_capture_loop(display_id, tx);
    }

    let mut backend = match X11Capture::new(&display_id) {
        Ok(backend) => backend,
        Err(err) => {
            tracing::warn!(?err, "XShm setup failed; falling back to XGetImage");
            return run_xgetimage_capture_loop(display_id, tx);
        }
    };
    let raw_tx = spawn_encoder(tx.clone());
    if let Err(err) = backend.capture(
        Rect {
            x: 0,
            y: 0,
            width: backend.width as u32,
            height: backend.height as u32,
        },
        monotonic_ns(),
        &raw_tx,
    ) {
        tracing::warn!(
            ?err,
            "initial XShm capture failed; falling back to XGetImage"
        );
        return run_xgetimage_capture_loop(display_id, tx);
    }

    loop {
        let dirty = backend.next_damage()?;
        if let Err(err) = backend.capture(dirty, monotonic_ns(), &raw_tx) {
            tracing::warn!(?err, "XShm capture failed; falling back to XGetImage");
            return run_xgetimage_capture_loop(display_id, tx);
        }
    }
}

fn run_xgetimage_capture_loop(display_id: String, tx: broadcast::Sender<BusMessage>) -> Result<()> {
    let mut backend = X11FallbackCapture::new(&display_id)?;
    let raw_tx = spawn_encoder(tx);
    backend.capture(
        Rect {
            x: 0,
            y: 0,
            width: backend.width as u32,
            height: backend.height as u32,
        },
        monotonic_ns(),
        &raw_tx,
    )?;

    loop {
        let dirty = backend.next_damage()?;
        backend.capture(dirty, monotonic_ns(), &raw_tx)?;
    }
}

struct RawFrame {
    display_id: String,
    dirty: Rect,
    width: u32,
    height: u32,
    stride_bytes: u32,
    bits_per_pixel: c_int,
    bytes: Vec<u8>,
    monotonic_ns: u128,
}

fn spawn_encoder(tx: broadcast::Sender<BusMessage>) -> mpsc::Sender<RawFrame> {
    let (raw_tx, raw_rx) = mpsc::channel::<RawFrame>();
    thread::spawn(move || {
        let mut encoder = JpegEncoder::new().ok();
        while let Ok(raw) = raw_rx.recv() {
            let (encoding, bytes, stride_bytes) = if raw.bits_per_pixel == 32 {
                match encoder.as_mut().and_then(|enc| enc.encode_bgra(&raw).ok()) {
                    Some(jpeg) => (FrameEncoding::Jpeg, jpeg, 0),
                    None => (FrameEncoding::RawBgra, raw.bytes.clone(), raw.stride_bytes),
                }
            } else {
                (FrameEncoding::RawBgra, raw.bytes.clone(), raw.stride_bytes)
            };

            let frame = CaptureFrame {
                ts: Utc::now(),
                display_id: raw.display_id,
                dirty: raw.dirty,
                width: raw.width,
                height: raw.height,
                stride_bytes,
                encoding,
                bytes,
                monotonic_ns: raw.monotonic_ns,
            };
            let _ = tx.send(BusMessage::Capture(frame));
        }
    });
    raw_tx
}

unsafe extern "C" fn record_x_error(_display: *mut Display, error: *mut XErrorEvent) -> c_int {
    if !error.is_null() {
        LAST_X_ERROR.store(unsafe { (*error).error_code as i32 }, Ordering::Relaxed);
    }
    0
}

fn begin_x_error_trap() -> XErrorHandler {
    LAST_X_ERROR.store(0, Ordering::Relaxed);
    unsafe { XSetErrorHandler(Some(record_x_error)) }
}

fn end_x_error_trap(display: *mut Display, previous: XErrorHandler) -> i32 {
    unsafe {
        XSync(display, 0);
        XSetErrorHandler(previous);
    }
    LAST_X_ERROR.load(Ordering::Relaxed)
}

struct X11FallbackCapture {
    display_id: String,
    display: *mut Display,
    root: Window,
    damage: Damage,
    damage_event_base: c_int,
    width: c_int,
    height: c_int,
}

impl X11FallbackCapture {
    fn new(display_id: &str) -> Result<Self> {
        let display_name = CString::new(display_id)?;
        let display = unsafe { XOpenDisplay(display_name.as_ptr()) };
        if display.is_null() {
            bail!("XOpenDisplay failed for fallback capture on {display_id}");
        }

        let screen = unsafe { XDefaultScreen(display) };
        let root = unsafe { XRootWindow(display, screen) };
        let width = unsafe { XDisplayWidth(display, screen) };
        let height = unsafe { XDisplayHeight(display, screen) };

        let mut damage_event_base = 0;
        let mut damage_error_base = 0;
        if unsafe { XDamageQueryExtension(display, &mut damage_event_base, &mut damage_error_base) }
            == 0
        {
            unsafe {
                XCloseDisplay(display);
            }
            bail!("XDamage extension is not available");
        }

        let mut composite_event_base = 0;
        let mut composite_error_base = 0;
        if unsafe {
            XCompositeQueryExtension(
                display,
                &mut composite_event_base,
                &mut composite_error_base,
            )
        } == 0
        {
            unsafe {
                XCloseDisplay(display);
            }
            bail!("XComposite extension is not available");
        }

        let damage = unsafe { XDamageCreate(display, root, X_DAMAGE_REPORT_BOUNDING_BOX) };
        if damage == 0 {
            unsafe {
                XCloseDisplay(display);
            }
            bail!("XDamageCreate failed");
        }

        Ok(Self {
            display_id: display_id.to_string(),
            display,
            root,
            damage,
            damage_event_base,
            width,
            height,
        })
    }

    fn next_damage(&mut self) -> Result<Rect> {
        loop {
            let mut event = XEvent { pad: [0; 24] };
            unsafe {
                XNextEvent(self.display, &mut event);
            }
            let event_type = unsafe { event.type_ };
            if event_type == self.damage_event_base + X_DAMAGE_NOTIFY {
                let damage_event =
                    unsafe { ptr::read((&event as *const XEvent).cast::<XDamageNotifyEvent>()) };
                unsafe {
                    XDamageSubtract(self.display, self.damage, 0, 0);
                }
                return Ok(rect_from_xrectangle(
                    damage_event.area,
                    self.width,
                    self.height,
                ));
            }
        }
    }

    fn capture(
        &mut self,
        dirty: Rect,
        monotonic_ns: u128,
        raw_tx: &mpsc::Sender<RawFrame>,
    ) -> Result<()> {
        let image = unsafe {
            XGetImage(
                self.display,
                self.root,
                0,
                0,
                self.width as c_uint,
                self.height as c_uint,
                ALL_PLANES,
                ZPIXMAP,
            )
        };
        if image.is_null() {
            return Err(anyhow!("XGetImage fallback failed"));
        }

        let image_ref = unsafe { &*image };
        let len = image_ref.bytes_per_line as usize * image_ref.height as usize;
        let bytes =
            unsafe { std::slice::from_raw_parts(image_ref.data.cast::<u8>(), len) }.to_vec();
        let raw = RawFrame {
            display_id: self.display_id.clone(),
            dirty,
            width: image_ref.width as u32,
            height: image_ref.height as u32,
            stride_bytes: image_ref.bytes_per_line as u32,
            bits_per_pixel: image_ref.bits_per_pixel,
            bytes,
            monotonic_ns,
        };
        unsafe {
            XDestroyImage(image);
        }
        raw_tx.send(raw)?;
        Ok(())
    }
}

impl Drop for X11FallbackCapture {
    fn drop(&mut self) {
        unsafe {
            if self.damage != 0 {
                XDamageDestroy(self.display, self.damage);
            }
            if !self.display.is_null() {
                XCloseDisplay(self.display);
            }
        }
    }
}

struct X11Capture {
    display_id: String,
    display: *mut Display,
    root: Window,
    damage: Damage,
    damage_event_base: c_int,
    image: *mut XImage,
    shminfo: XShmSegmentInfo,
    back_buffer: Vec<u8>,
    width: c_int,
    height: c_int,
}

impl X11Capture {
    fn new(display_id: &str) -> Result<Self> {
        let display_name = CString::new(display_id)?;
        let display = unsafe { XOpenDisplay(display_name.as_ptr()) };
        if display.is_null() {
            bail!("XOpenDisplay failed for {display_id}");
        }

        let screen = unsafe { XDefaultScreen(display) };
        let root = unsafe { XRootWindow(display, screen) };
        let width = unsafe { XDisplayWidth(display, screen) };
        let height = unsafe { XDisplayHeight(display, screen) };
        let visual = unsafe { XDefaultVisual(display, screen) };
        let depth = unsafe { XDefaultDepth(display, screen) };

        if unsafe { XShmQueryExtension(display) } == 0 {
            bail!("MIT-SHM extension is not available");
        }

        let mut damage_event_base = 0;
        let mut damage_error_base = 0;
        if unsafe { XDamageQueryExtension(display, &mut damage_event_base, &mut damage_error_base) }
            == 0
        {
            bail!("XDamage extension is not available");
        }

        let mut composite_event_base = 0;
        let mut composite_error_base = 0;
        if unsafe {
            XCompositeQueryExtension(
                display,
                &mut composite_event_base,
                &mut composite_error_base,
            )
        } == 0
        {
            bail!("XComposite extension is not available");
        }

        let mut shminfo = XShmSegmentInfo {
            shmseg: 0,
            shmid: -1,
            shmaddr: ptr::null_mut(),
            read_only: 0,
        };
        let image = unsafe {
            XShmCreateImage(
                display,
                visual,
                depth as c_uint,
                ZPIXMAP,
                ptr::null_mut(),
                &mut shminfo,
                width as c_uint,
                height as c_uint,
            )
        };
        if image.is_null() {
            bail!("XShmCreateImage failed");
        }

        let shm_size = unsafe { ((*image).bytes_per_line as usize) * ((*image).height as usize) };
        let shmid = unsafe { libc::shmget(IPC_PRIVATE, shm_size, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            unsafe {
                XDestroyImage(image);
                XCloseDisplay(display);
            }
            return Err(std::io::Error::last_os_error()).context("shmget");
        }
        let shmaddr = unsafe { libc::shmat(shmid, ptr::null(), 0) };
        if shmaddr == (-1_isize as *mut c_void) {
            unsafe {
                libc::shmctl(shmid, IPC_RMID, ptr::null_mut());
                XDestroyImage(image);
                XCloseDisplay(display);
            }
            return Err(std::io::Error::last_os_error()).context("shmat");
        }

        shminfo.shmid = shmid;
        shminfo.shmaddr = shmaddr.cast::<c_char>();
        unsafe {
            (*image).data = shminfo.shmaddr;
        }
        let previous_handler = begin_x_error_trap();
        let attach_status = unsafe { XShmAttach(display, &mut shminfo) };
        let attach_error = end_x_error_trap(display, previous_handler);
        if attach_status == 0 || attach_error != 0 {
            unsafe {
                libc::shmdt(shminfo.shmaddr.cast::<c_void>());
                libc::shmctl(shmid, IPC_RMID, ptr::null_mut());
                XDestroyImage(image);
                XCloseDisplay(display);
            }
            bail!("XShmAttach failed status={attach_status} x_error={attach_error}");
        }
        let damage = unsafe { XDamageCreate(display, root, X_DAMAGE_REPORT_BOUNDING_BOX) };
        if damage == 0 {
            bail!("XDamageCreate failed");
        }

        Ok(Self {
            display_id: display_id.to_string(),
            display,
            root,
            damage,
            damage_event_base,
            image,
            shminfo,
            back_buffer: vec![0_u8; shm_size],
            width,
            height,
        })
    }

    fn next_damage(&mut self) -> Result<Rect> {
        loop {
            let mut event = XEvent { pad: [0; 24] };
            unsafe {
                XNextEvent(self.display, &mut event);
            }
            let event_type = unsafe { event.type_ };
            if event_type == self.damage_event_base + X_DAMAGE_NOTIFY {
                let damage_event =
                    unsafe { ptr::read((&event as *const XEvent).cast::<XDamageNotifyEvent>()) };
                unsafe {
                    XDamageSubtract(self.display, self.damage, 0, 0);
                }
                return Ok(rect_from_xrectangle(
                    damage_event.area,
                    self.width,
                    self.height,
                ));
            }
        }
    }

    fn capture(
        &mut self,
        dirty: Rect,
        monotonic_ns: u128,
        raw_tx: &mpsc::Sender<RawFrame>,
    ) -> Result<()> {
        if dirty.width == 0 || dirty.height == 0 {
            return Ok(());
        }

        let image = unsafe { &mut *self.image };
        image.width = dirty.width as c_int;
        image.height = dirty.height as c_int;

        let previous_handler = begin_x_error_trap();
        let ok = unsafe {
            XShmGetImage(
                self.display,
                self.root,
                self.image,
                dirty.x,
                dirty.y,
                ALL_PLANES,
            )
        };
        let x_error = end_x_error_trap(self.display, previous_handler);
        if ok == 0 || x_error != 0 {
            return Err(anyhow!("XShmGetImage failed status={ok} x_error={x_error}"));
        }

        let pitch = image.bytes_per_line as usize;
        let bytes_per_pixel = (image.bits_per_pixel as usize).div_ceil(8).max(1);
        let dirty_width_bytes = dirty.width as usize * bytes_per_pixel;
        let src_len = pitch * dirty.height as usize;
        let src = unsafe { std::slice::from_raw_parts(image.data.cast::<u8>(), src_len) };
        for row in 0..dirty.height as usize {
            let src_start = row * pitch;
            let src_end = src_start + dirty_width_bytes;
            let dst_start =
                ((dirty.y as usize + row) * pitch) + (dirty.x as usize * bytes_per_pixel);
            let dst_end = dst_start + dirty_width_bytes;
            self.back_buffer[dst_start..dst_end].copy_from_slice(&src[src_start..src_end]);
        }

        let raw = RawFrame {
            display_id: self.display_id.clone(),
            dirty,
            width: self.width as u32,
            height: self.height as u32,
            stride_bytes: image.bytes_per_line as u32,
            bits_per_pixel: image.bits_per_pixel,
            bytes: self.back_buffer.clone(),
            monotonic_ns,
        };
        raw_tx.send(raw)?;
        Ok(())
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        unsafe {
            if self.damage != 0 {
                XDamageDestroy(self.display, self.damage);
            }
            XShmDetach(self.display, &mut self.shminfo);
            if !self.shminfo.shmaddr.is_null() {
                libc::shmdt(self.shminfo.shmaddr.cast::<c_void>());
            }
            if self.shminfo.shmid >= 0 {
                libc::shmctl(self.shminfo.shmid, IPC_RMID, ptr::null_mut());
            }
            if !self.image.is_null() {
                XDestroyImage(self.image);
            }
            if !self.display.is_null() {
                XCloseDisplay(self.display);
            }
        }
    }
}

fn rect_from_xrectangle(rect: XRectangle, max_width: c_int, max_height: c_int) -> Rect {
    let x = i32::from(rect.x).clamp(0, max_width.max(0));
    let y = i32::from(rect.y).clamp(0, max_height.max(0));
    let width = u32::from(rect.width).min(max_width.saturating_sub(x) as u32);
    let height = u32::from(rect.height).min(max_height.saturating_sub(y) as u32);
    Rect {
        x,
        y,
        width,
        height,
    }
}

struct JpegEncoder {
    handle: *mut c_void,
}

impl JpegEncoder {
    fn new() -> Result<Self> {
        let handle = unsafe { tjInitCompress() };
        if handle.is_null() {
            bail!("tjInitCompress failed");
        }
        Ok(Self { handle })
    }

    fn encode_bgra(&mut self, raw: &RawFrame) -> Result<Vec<u8>> {
        const TJPF_BGRA: c_int = 8;
        const TJSAMP_420: c_int = 2;
        const TJFLAG_FASTDCT: c_int = 2048;

        let mut jpeg_buf: *mut u8 = ptr::null_mut();
        let mut jpeg_size: c_ulong = 0;
        let rc = unsafe {
            tjCompress2(
                self.handle,
                raw.bytes.as_ptr(),
                raw.width as c_int,
                raw.stride_bytes as c_int,
                raw.height as c_int,
                TJPF_BGRA,
                &mut jpeg_buf,
                &mut jpeg_size,
                TJSAMP_420,
                75,
                TJFLAG_FASTDCT,
            )
        };
        if rc != 0 {
            let msg = unsafe {
                let ptr = tjGetErrorStr2(self.handle);
                if ptr.is_null() {
                    "turbojpeg compression failed".to_string()
                } else {
                    std::ffi::CStr::from_ptr(ptr).to_string_lossy().to_string()
                }
            };
            bail!(msg);
        }
        let bytes = unsafe { std::slice::from_raw_parts(jpeg_buf, jpeg_size as usize).to_vec() };
        unsafe {
            tjFree(jpeg_buf);
        }
        Ok(bytes)
    }
}

impl Drop for JpegEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = tjDestroy(self.handle);
        }
    }
}
