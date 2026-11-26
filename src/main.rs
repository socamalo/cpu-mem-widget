//! A square always-on-top frameless widget showing CPU and memory as filled
//! area ("integral") charts whose colour tracks the value: cool when idle, red
//! at the top. Samples once per second; repaints once per second.
//!
//! Drag anywhere to move it, drag the bottom-left grip to resize, click the
//! top-right cross to quit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::epaint::Vertex;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use windows_sys::Win32::Foundation::{FILETIME, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DwmEnableBlurBehindWindow, DwmSetWindowAttribute, DWM_BLURBEHIND,
    DWM_BB_BLURREGION, DWM_BB_ENABLE, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
    DWMWA_NCRENDERING_POLICY, DWMWA_WINDOW_CORNER_PREFERENCE, DWMNCRP_DISABLED, DWMWCP_DONOTROUND,
};
use windows_sys::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, SetWindowPos,
    CallWindowProcW, GetCursorPos, SystemParametersInfoW, GWL_EXSTYLE, GWL_STYLE, GWLP_WNDPROC,
    HWND_TOPMOST, SPI_GETWORKAREA, WNDPROC,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    WS_CAPTION, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP,
    WS_SYSMENU, WS_THICKFRAME,
};

// ---------------------------------------------------------------- geometry
//
// Every metric below is expressed for the 190 pt design size; the layout is
// scaled uniformly by k = width / SIDE so the widget looks the same at any
// size the user drags it to.

const SIDE: f32 = 190.0;
const MIN_SIDE: f32 = 130.0;
const MAX_SIDE: f32 = 400.0;
const PAD: f32 = 14.0;
const LABEL_H: f32 = 18.0;
const LABEL_TO_BARS: f32 = 6.0;
const BAR_H: f32 = 50.0;
const ROW_GAP: f32 = 14.0;
const CORNER: f32 = 14.0;
const GRIP: f32 = 26.0;
const HIST: usize = 60; // 60 samples x 1 s = the last minute
const TICK: Duration = Duration::from_secs(1);
const SAVE_EVERY: Duration = Duration::from_secs(5);

// ------------------------------------------------------------------ colors

/// Card fill. 96 % opaque: enough translucency to feel like it floats, not
/// enough for text underneath to ghost through. Drop the alpha toward 200 for
/// a glass look, raise to 255 for solid.
const BG: Color32 = Color32::from_rgba_premultiplied(13, 13, 15, 246);
const LABEL_FG: Color32 = Color32::from_rgb(150, 152, 162);
const HAIRLINE: Color32 = Color32::from_rgba_premultiplied(4, 4, 4, 14);

/// Value -> colour ramp: cold steel blue at 0 %, through a neutral hinge around
/// half load, into gold and finally red at 100 %. Interpolated linearly, so the
/// gradient stays continuous; banding is avoided by baking it into a texture
/// that the GPU samples with bilinear filtering.
const RAMP: [[f32; 4]; 6] = [
    [0.00, 62.0, 104.0, 148.0],
    [0.32, 96.0, 146.0, 176.0],
    [0.54, 164.0, 176.0, 180.0],
    [0.72, 224.0, 176.0, 100.0],
    [0.87, 231.0, 132.0, 72.0],
    [1.00, 226.0, 66.0, 64.0],
];

fn ramp_rgb(v: f32) -> [f32; 3] {
    let v = v.clamp(0.0, 1.0);
    for w in RAMP.windows(2) {
        let (a, b) = (w[0], w[1]);
        if v <= b[0] {
            let t = (v - a[0]) / (b[0] - a[0]);
            return [
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
                a[3] + (b[3] - a[3]) * t,
            ];
        }
    }
    [RAMP[5][1], RAMP[5][2], RAMP[5][3]]
}

/// Text version: the ramp lifted toward white so it stays legible on the card.
fn ramp_text(v: f32) -> Color32 {
    let c = ramp_rgb(v);
    let f = |x: f32| (x + (255.0 - x) * 0.32).round() as u8;
    Color32::from_rgb(f(c[0]), f(c[1]), f(c[2]))
}

/// One row of the baked gradient: colour by value, alpha rising with value so
/// the body of the area is present but the peak is what glows.
fn ramp_fill(v: f32) -> Color32 {
    let c = ramp_rgb(v);
    let a = 0.45 + 0.55 * v.clamp(0.0, 1.0).powf(0.7);
    Color32::from_rgba_unmultiplied(
        c[0].round() as u8,
        c[1].round() as u8,
        c[2].round() as u8,
        (a * 255.0).round() as u8,
    )
}

fn ramp_handle(ctx: &egui::Context) -> egui::TextureHandle {
    const N: usize = 512;
    let pixels = (0..N)
        .map(|i| ramp_fill(i as f32 / (N - 1) as f32))
        .collect();
    ctx.load_texture(
        "ramp",
        egui::ColorImage::new([1, N], pixels),
        egui::TextureOptions::LINEAR,
    )
}

// ------------------------------------------------------------------- probe

fn filetime_ticks(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// (idle, kernel, user) in 100 ns ticks. Kernel time already includes idle.
fn system_times() -> Option<(u64, u64, u64)> {
    let mut idle: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    (ok != 0).then(|| {
        (
            filetime_ticks(&idle),
            filetime_ticks(&kernel),
            filetime_ticks(&user),
        )
    })
}

/// Physical memory in use, as a fraction. `dwMemoryLoad` is already a percent.
fn memory_load() -> Option<f32> {
    let mut ms: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    ms.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut ms) };
    (ok != 0).then(|| (ms.dwMemoryLoad as f32 / 100.0).clamp(0.0, 1.0))
}

// --------------------------------------------------------------------- win32

fn window_handle() -> Option<HWND> {
    let mut title: Vec<u16> = "cpumem-widget".encode_utf16().collect();
    title.push(0);
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    (!hwnd.is_null()).then_some(hwnd)
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    (unsafe { GetWindowRect(hwnd, &mut r) } != 0).then_some(r)
}

fn work_area() -> Option<RECT> {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut _ as *mut _, 0) };
    (ok != 0).then_some(r)
}

fn dpi_scale() -> f32 {
    unsafe { GetDpiForSystem() as f32 / 96.0 }
}

/// Cursor position in physical screen pixels — the one coordinate frame that
/// does not move when the widget itself is moved.
fn cursor() -> Option<(i32, i32)> {
    let mut p: POINT = unsafe { std::mem::zeroed() };
    (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
}

/// Where the widget parks on first run: top-right of the primary work area.
fn default_position() -> Option<Pos2> {
    let wa = work_area()?;
    let scale = dpi_scale();
    let margin = 24.0 * scale;
    Some(Pos2::new(
        (wa.right as f32 - SIDE * scale - margin) / scale,
        (wa.top as f32 + margin) / scale,
    ))
}

// Position and size are kept in physical pixels, written next to the app data.
// eframe's own window persistence round-trips through points and drifts by the
// scale factor on this machine, marching the widget off-screen, so it is off.

fn state_file() -> Option<PathBuf> {
    let dir = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(dir).join("cpu-mem-widget").join("pos.txt"))
}

fn load_state() -> Option<(i32, i32, i32, i32)> {
    let text = std::fs::read_to_string(state_file()?).ok()?;
    let n: Vec<i32> = text
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    (n.len() == 4).then(|| (n[0], n[1], n[2], n[3]))
}

fn save_state(x: i32, y: i32, w: i32, h: i32) {
    if let Some(path) = state_file() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, format!("{x},{y},{w},{h}"));
    }
}

/// Keep the widget reachable: if window persistence ever restores it mostly
/// off-screen, park it back in the top-right corner.
fn ensure_on_screen(hwnd: HWND) {
    let (Some(wr), Some(wa)) = (window_rect(hwnd), work_area()) else {
        return;
    };
    let w = wr.right - wr.left;
    let h = wr.bottom - wr.top;
    let visible_x = wr.right.min(wa.right) - wr.left.max(wa.left);
    let visible_y = wr.bottom.min(wa.bottom) - wr.top.max(wa.top);
    if visible_x >= w / 2 && visible_y >= h / 2 {
        return;
    }
    let m = (24.0 * dpi_scale()) as i32;
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            wa.right - w - m,
            wa.top + m,
            w,
            h,
            SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

/// eframe/winit leaves a one-pixel non-client strip along the top of the
/// window; the shell paints its frame colour there, which shows up as a hard
/// line above the card. Returning 0 from WM_NCCALCSIZE makes the client area
/// cover the whole window, so there is nothing left to paint.
static ORIG_WNDPROC: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn no_nc_area(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    const WM_NCCALCSIZE: u32 = 0x0083;
    if msg == WM_NCCALCSIZE && wparam != 0 {
        return 0;
    }
    let prev = ORIG_WNDPROC.load(Ordering::Relaxed);
    if prev == 0 {
        return 0;
    }
    let proc: WNDPROC = std::mem::transmute(prev);
    CallWindowProcW(proc, hwnd, msg, wparam, lparam)
}

fn strip_nc_area(hwnd: HWND) {
    let hook: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT = no_nc_area;
    let hook_addr = hook as usize as isize;
    unsafe {
        let prev = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
        if prev == hook_addr {
            return;
        }
        ORIG_WNDPROC.store(prev, Ordering::Relaxed);
        SetWindowLongPtrW(hwnd, GWLP_WNDPROC, hook_addr);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

/// Two things winit leaves undone for a frameless widget on Windows:
///
/// * per-pixel transparency — winit never enables it, so DWM composites the
///   client area by its own alpha channel once blur-behind is on; without this
///   the rounded card sits on an opaque rectangle;
/// * hiding from the taskbar — WS_EX_APPWINDOW wins over WS_EX_TOOLWINDOW, so
///   both bits have to be touched. winit rewrites the ex-style while it applies
///   viewport options, hence the caller repeats this for a few frames.
fn dress_window(hwnd: HWND) {
    unsafe {
        // Per-pixel alpha needs DWM blur-behind, but the usual "whole window"
        // region also makes Windows outline the window with a 1 px frame that
        // shows as a hard line wherever the rounded card leaves the edge bare.
        // An empty blur region keeps the alpha composition without the frame.
        let rgn = CreateRectRgn(0, 0, 0, 0);
        let mut bb: DWM_BLURBEHIND = std::mem::zeroed();
        bb.dwFlags = DWM_BB_ENABLE | DWM_BB_BLURREGION;
        bb.fEnable = 1;
        bb.hRgnBlur = rgn;
        let _ = DwmEnableBlurBehindWindow(hwnd, &bb);
        if !rgn.is_null() {
            DeleteObject(rgn);
        }

        // A topmost window still gets a square drop shadow from the shell, which
        // shows up as a grey box hugging the rounded card on light backgrounds.
        // Turning off non-client rendering drops the frame and that shadow.
        let ncrp: i32 = DWMNCRP_DISABLED;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY as u32,
            &ncrp as *const _ as *const _,
            size_of::<i32>() as u32,
        );
        // (No DwmExtendFrameIntoClientArea: extending the frame is what leaves a
        // one-pixel non-client strip along the top that the shell paints.)

        // Windows 11 rounds and outlines every top-level window on its own.
        // Left alone it paints that shell corner and a 1 px border over the
        // card's transparent corners, so switch both off. Both calls fail
        // harmlessly on Windows 10.
        let corner: i32 = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &corner as *const _ as *const _,
            size_of::<i32>() as u32,
        );
        let border: u32 = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            &border as *const _ as *const _,
            size_of::<u32>() as u32,
        );

        // winit leaves a framed style on what should be a plain popup. Those bits
        // carve a 1 px non-client strip off the top of the window, and the shell
        // paints its own border colour there — the hard line that shows above the
        // card wherever the rounded corner leaves the edge bare.
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if style & WS_CAPTION as isize != 0 {
            let bare = style & !((WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX
                | WS_MAXIMIZEBOX) as isize)
                | WS_POPUP as isize;
            SetWindowLongPtrW(hwnd, GWL_STYLE, bare);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }

        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = (ex & !(WS_EX_APPWINDOW as isize)) | WS_EX_TOOLWINDOW as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
    }
}

// -------------------------------------------------------------------- state

struct Widget {
    cpu: f32,
    mem: f32,
    base: Option<(u64, u64, u64)>,
    cpu_hist: VecDeque<f32>,
    mem_hist: VecDeque<f32>,
    last: Instant,
    last_save: Instant,
    saved: Option<(i32, i32, i32, i32)>,
    hwnd: isize,
    dress_left: u8,
    resizing: bool,
    rs_right: i32,
    rs_top: i32,
    rs_side: i32,
    rs_bias: f32,
    ramp: Option<egui::TextureHandle>,
}

impl Widget {
    fn new() -> Self {
        Self {
            cpu: 0.0,
            mem: 0.0,
            // Seed the baseline now, so the first tick one second later already
            // has a real delta instead of reporting a bogus 0 %.
            base: system_times(),
            cpu_hist: VecDeque::with_capacity(HIST),
            mem_hist: VecDeque::with_capacity(HIST),
            last: Instant::now(),
            last_save: Instant::now(),
            saved: None,
            hwnd: 0,
            dress_left: 8,
            resizing: false,
            rs_right: 0,
            rs_top: 0,
            rs_side: 0,
            rs_bias: 0.0,
            ramp: None,
        }
    }

    fn hwnd(&mut self) -> Option<HWND> {
        if self.hwnd == 0 {
            self.hwnd = window_handle().map_or(0, |h| h as isize);
        }
        (self.hwnd != 0).then_some(self.hwnd as HWND)
    }

    fn sample(&mut self) {
        if let (Some((i0, k0, u0)), Some((i1, k1, u1))) = (self.base, system_times()) {
            let total = k1.wrapping_sub(k0) + u1.wrapping_sub(u0);
            if total > 0 {
                let busy = total.saturating_sub(i1.wrapping_sub(i0));
                self.cpu = (busy as f32 / total as f32).clamp(0.0, 1.0);
            }
            self.base = Some((i1, k1, u1));
        }
        if let Some(m) = memory_load() {
            self.mem = m;
        }
        push(&mut self.cpu_hist, self.cpu);
        push(&mut self.mem_hist, self.mem);
    }
}

fn push(hist: &mut VecDeque<f32>, v: f32) {
    if hist.len() == HIST {
        hist.pop_front();
    }
    hist.push_back(v);
}

// ------------------------------------------------------------------ curves

/// Fritsch-Carlson style tangents: central differences, zeroed at local
/// extrema so the interpolated curve can never overshoot the samples.
fn tangents(ys: &[f32]) -> Vec<f32> {
    let n = ys.len();
    let mut m = vec![0.0f32; n];
    for i in 1..n - 1 {
        let d0 = ys[i] - ys[i - 1];
        let d1 = ys[i + 1] - ys[i];
        m[i] = if d0 * d1 <= 0.0 { 0.0 } else { (d0 + d1) * 0.5 };
    }
    m
}

/// Hermite-interpolated curve through the samples, newest anchored right.
fn curve(hist: &VecDeque<f32>, bars: Rect) -> Vec<Pos2> {
    let n = hist.len();
    if n < 2 {
        return Vec::new();
    }
    let step = bars.width() / (HIST - 1) as f32;
    let ys: Vec<f32> = hist.iter().map(|v| v.clamp(0.0, 1.0)).collect();
    let m = tangents(&ys);
    let x_at = |i: f32| bars.right() - (n as f32 - 1.0 - i) * step;
    let y_at = |v: f32| bars.bottom() - v.clamp(0.0, 1.0) * bars.height();

    const SUB: usize = 3;
    let mut pts = Vec::with_capacity((n - 1) * SUB + 1);
    for seg in 0..n - 1 {
        for s in 0..SUB {
            let t = s as f32 / SUB as f32;
            let (t2, t3) = (t * t, t * t * t);
            let v = (2.0 * t3 - 3.0 * t2 + 1.0) * ys[seg]
                + (t3 - 2.0 * t2 + t) * m[seg]
                + (-2.0 * t3 + 3.0 * t2) * ys[seg + 1]
                + (t3 - t2) * m[seg + 1];
            pts.push(Pos2::new(x_at(seg as f32 + t), y_at(v)));
        }
    }
    pts.push(Pos2::new(x_at((n - 1) as f32), y_at(ys[n - 1])));
    pts
}

/// Area under the curve, textured with the value ramp so the colour of every
/// pixel follows its own height. uv.y = 0 sits on the baseline, 1 at the top.
fn area_mesh(points: &[Pos2], bars: Rect, tex: egui::TextureId) -> egui::Mesh {
    let mut mesh = egui::Mesh::with_texture(tex);
    let v_at = |y: f32| ((bars.bottom() - y) / bars.height()).clamp(0.0, 1.0);
    let base_uv = egui::pos2(0.5, 0.0);
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b.x <= a.x {
            continue;
        }
        let i = mesh.vertices.len() as u32;
        mesh.vertices.push(Vertex { pos: a, uv: egui::pos2(0.5, v_at(a.y)), color: Color32::WHITE });
        mesh.vertices.push(Vertex { pos: Pos2::new(a.x, bars.bottom()), uv: base_uv, color: Color32::WHITE });
        mesh.vertices.push(Vertex { pos: Pos2::new(b.x, bars.bottom()), uv: base_uv, color: Color32::WHITE });
        mesh.vertices.push(Vertex { pos: b, uv: egui::pos2(0.5, v_at(b.y)), color: Color32::WHITE });
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i, i + 2, i + 3);
    }
    mesh
}

// ---------------------------------------------------------------- rendering

struct Row<'a> {
    label: &'a str,
    value: f32,
    hist: &'a VecDeque<f32>,
    label_y: f32,
}

fn draw_row(p: &egui::Painter, left: f32, bars: Rect, row: &Row, k: f32, tex: egui::TextureId) {
    let cy = row.label_y + LABEL_H * k * 0.5;

    p.text(
        Pos2::new(left, cy),
        Align2::LEFT_CENTER,
        row.label,
        FontId::proportional(11.0 * k),
        LABEL_FG,
    );
    p.text(
        Pos2::new(left + 30.0 * k, cy),
        Align2::LEFT_CENTER,
        format!("{:.0}%", row.value * 100.0),
        FontId::monospace(17.0 * k),
        ramp_text(row.value),
    );

    let pts = curve(row.hist, bars);
    if pts.len() < 2 {
        return;
    }

    p.add(egui::Shape::mesh(area_mesh(&pts, bars, tex)));
    p.hline(
        bars.x_range(),
        bars.bottom(),
        Stroke::new((1.0 * k).max(1.0), HAIRLINE),
    );
    // One polyline rather than one stroke per segment: per-segment strokes leave
    // a dotted look wherever the curve is flat. The crest takes the current
    // value's colour, which is the bit of the ramp the eye is already on.
    p.add(egui::Shape::line(
        pts,
        Stroke::new((1.2 * k).max(1.0), ramp_text(row.value)),
    ));
}

impl eframe::App for Widget {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Fully transparent clear so only the rounded card carries alpha.
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let hwnd = self.hwnd();

        if let Some(hwnd) = hwnd {
            if self.dress_left > 0 {
                dress_window(hwnd);
                if self.dress_left == 8 {
                    if let Some((x, y, w, h)) = load_state() {
                        unsafe {
                            SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
                        }
                    }
                }
                ensure_on_screen(hwnd);
                strip_nc_area(hwnd);
                self.dress_left -= 1;
            }
            if self.last_save.elapsed() >= SAVE_EVERY {
                self.last_save = Instant::now();
                if let Some(wr) = window_rect(hwnd) {
                    let now = (wr.left, wr.top, wr.right - wr.left, wr.bottom - wr.top);
                    if self.saved != Some(now) {
                        self.saved = Some(now);
                        save_state(now.0, now.1, now.2, now.3);
                    }
                }
            }
        }

        if self.last.elapsed() >= TICK {
            self.last = Instant::now();
            self.sample();
        }

        if self.ramp.is_none() {
            self.ramp = Some(ramp_handle(&ctx));
        }
        let tex = self.ramp.as_ref().unwrap().id();

        let full = ui.max_rect();
        let k = (full.width() / SIDE).clamp(MIN_SIDE / SIDE, MAX_SIDE / SIDE);
        let corner = (CORNER * k).round().clamp(6.0, 30.0) as u8;

        // Bottom-left resize grip. Dragging it re-sizes by moving the window with
        // SetWindowPos, so the top-right corner stays put and every unit stays in
        // real pixels. The grip is handled positionally rather than as an egui
        // widget: a press here must not start the native move loop.
        let grip_rect = Rect::from_min_max(
            Pos2::new(full.left(), full.bottom() - GRIP * k),
            Pos2::new(full.left() + GRIP * k, full.bottom()),
        );
        let grip_hover = ctx.input(|i| {
            i.pointer
                .hover_pos()
                .map_or(false, |p| grip_rect.contains(p))
        });
        let press_in_grip = ctx.input(|i| {
            i.pointer.primary_pressed()
                && i.pointer
                    .interact_pos()
                    .map_or(false, |p| grip_rect.contains(p))
        });

        let resp = ui.allocate_rect(full, Sense::click_and_drag());
        if press_in_grip {
            if let (Some(_), Some(wr), Some((cx, cy))) = (hwnd, hwnd.and_then(window_rect), cursor()) {
                let side0 = (wr.right - wr.left) as f32;
                let ideal0 = ((wr.right - cx) as f32 + (cy - wr.top) as f32) * 0.5;
                self.rs_right = wr.right;
                self.rs_top = wr.top;
                self.rs_side = side0 as i32;
                // The press usually lands a few px inside the corner; remember the
                // offset so grabbing the grip does not resize on the very first frame.
                self.rs_bias = side0 - ideal0;
                self.resizing = true;
            }
        }
        if ctx.input(|i| !i.pointer.primary_down()) {
            self.resizing = false;
        }
        if self.resizing {
            // Absolute screen coordinates only: deriving the size from the window's
            // own position feeds back into the next frame and makes it oscillate.
            if let (Some(hwnd), Some((cx, cy))) = (hwnd, cursor()) {
                let ppp = ctx.pixels_per_point();
                let ideal = ((self.rs_right - cx) as f32 + (cy - self.rs_top) as f32) * 0.5;
                let side = (ideal + self.rs_bias)
                    .clamp(MIN_SIDE * ppp, MAX_SIDE * ppp)
                    .round() as i32;
                if side != self.rs_side && side > 0 {
                    self.rs_side = side;
                    unsafe {
                        SetWindowPos(
                            hwnd,
                            std::ptr::null_mut(),
                            self.rs_right - side,
                            self.rs_top,
                            side,
                            side,
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                        );
                    }
                }
            }
        }

        // Quiet close affordance: always present but barely there, brightening
        // when the pointer is on it. Gating on the window's hover state is
        // unreliable for an always-on-top, unfocused window, so it is not.
        let close = Rect::from_center_size(
            Pos2::new(full.right() - 15.0 * k, full.top() + 15.0 * k),
            Vec2::splat(18.0 * k),
        );
        let hit = ui.interact(close, ui.scope_id().with("close"), Sense::click());

        if resp.drag_started_by(egui::PointerButton::Primary) && !self.resizing {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        let p = ui.painter();
        p.rect_filled(full, CornerRadius::same(corner), BG);

        let left = full.left() + PAD * k;
        let bars_w = full.width() - PAD * 2.0 * k;
        let y0 = full.top() + PAD * k;
        let bars1_y = y0 + (LABEL_H + LABEL_TO_BARS) * k;
        let label2_y = bars1_y + (BAR_H + ROW_GAP) * k;
        let bars2_y = label2_y + (LABEL_H + LABEL_TO_BARS) * k;

        let cpu_bars = Rect::from_min_size(Pos2::new(left, bars1_y), Vec2::new(bars_w, BAR_H * k));
        let mem_bars = Rect::from_min_size(Pos2::new(left, bars2_y), Vec2::new(bars_w, BAR_H * k));

        draw_row(
            p,
            left,
            cpu_bars,
            &Row { label: "CPU", value: self.cpu, hist: &self.cpu_hist, label_y: y0 },
            k,
            tex,
        );
        draw_row(
            p,
            left,
            mem_bars,
            &Row { label: "MEM", value: self.mem, hist: &self.mem_hist, label_y: label2_y },
            k,
            tex,
        );

        let ink = if hit.hovered() {
            Color32::from_rgb(235, 235, 240)
        } else {
            Color32::from_rgba_premultiplied(34, 34, 38, 120)
        };
        let g = close.shrink(6.0 * k);
        let s = Stroke::new(1.4 * k, ink);
        p.line_segment([g.left_top(), g.right_bottom()], s);
        p.line_segment([g.right_top(), g.left_bottom()], s);
        if hit.clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Grip mark: two diagonals hugging the bottom-left corner.
        let gi = if grip_hover || self.resizing {
            Color32::from_rgba_premultiplied(150, 152, 162, 200)
        } else {
            Color32::from_rgba_premultiplied(48, 48, 54, 110)
        };
        let gs = Stroke::new((1.0 * k).max(1.0), gi);
        let b = full.left_bottom();
        let a1 = 4.0 * k;
        let a2 = 11.0 * k;
        let a3 = 18.0 * k;
        p.line_segment(
            [Pos2::new(b.x + a1, b.y - a2), Pos2::new(b.x + a2, b.y - a1)],
            gs,
        );
        p.line_segment(
            [Pos2::new(b.x + a1, b.y - a3), Pos2::new(b.x + a3, b.y - a1)],
            gs,
        );

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        ctx.request_repaint_after(TICK);
    }
}

// --------------------------------------------------------------------- main

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("cpumem-widget")
        .with_inner_size(Vec2::splat(SIDE))
        // Keep the OS from drawing a resize frame; the grip does it manually.
        .with_resizable(false)
        .with_decorations(false)
        .with_transparent(true)
        .with_window_level(egui::WindowLevel::AlwaysOnTop);
    if let Some(pos) = default_position() {
        viewport = viewport.with_position(pos);
    }

    let options = eframe::NativeOptions {
        viewport,
        persist_window: false,
        ..Default::default()
    };

    eframe::run_native(
        "cpumem-widget",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(Widget::new()))
        }),
    )
}
