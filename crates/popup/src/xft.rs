//! Xft text painting for the menu.
//!
//! The window itself is driven through the core protocol, but text goes through
//! Xft: core fonts have no antialiasing and nothing beyond ISO 8859-1, so a
//! Chinese preset name would degrade to `?`. `XftDraw` is an Xlib object, hence
//! the second connection; drawing is issued there and flushed once per frame.
//!
//! The font is matched by Fontconfig using the union of every character in the
//! menu as the required charset, so one font covers all labels. Fontconfig's
//! built-in substitution rules pick a CJK-capable family when the charset needs
//! one, which is what makes Chinese labels render.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_short, c_uint, c_ulong, c_ushort, c_void};
use std::ptr;

use x11_dl::xlib::{Display, Visual, XWindowAttributes, Xlib};

use sizer_platform::{BackendError, BackendResult};

const FC_MATCH_PATTERN: c_int = 0;
const MENU_FAMILY: &str = "sans-serif";
const MENU_SIZE: f64 = 10.0;

/// Menu palette, in sRGB.
const PALETTE: [(u8, u8, u8); Ink::COUNT] = [
    (0x1e, 0x1e, 0x1e),
    (0x5e, 0x5e, 0x5e),
    (0xf6, 0xf5, 0xf4),
    (0x35, 0x84, 0xe4),
    (0xff, 0xff, 0xff),
];

#[repr(C)]
struct FcPattern {
    _private: [u8; 0],
}

#[repr(C)]
struct FcCharSet {
    _private: [u8; 0],
}

#[repr(C)]
struct XftDraw {
    _private: [u8; 0],
}

/// The leading fields of `XftFont` from Xft.h; the rest is never touched.
#[repr(C)]
struct XftFont {
    ascent: c_int,
    descent: c_int,
    height: c_int,
    max_advance_width: c_int,
    charset: *mut c_void,
    pattern: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XRenderColor {
    red: c_ushort,
    green: c_ushort,
    blue: c_ushort,
    alpha: c_ushort,
}

#[repr(C)]
struct XftColor {
    pixel: c_ulong,
    color: XRenderColor,
}

/// `XGlyphInfo` from Xft.h, including the sign of the bearing fields.
#[repr(C)]
struct XGlyphInfo {
    width: c_ushort,
    height: c_ushort,
    x: c_short,
    y: c_short,
    x_off: c_short,
    y_off: c_short,
}

unsafe extern "C" {
    fn XftFontOpenPattern(display: *mut Display, pattern: *mut FcPattern) -> *mut XftFont;
    fn XftFontClose(display: *mut Display, font: *mut XftFont);
    fn XftDrawCreate(
        display: *mut Display,
        drawable: c_ulong,
        visual: *mut Visual,
        colormap: c_ulong,
    ) -> *mut XftDraw;
    fn XftDrawDestroy(draw: *mut XftDraw);
    fn XftDrawRect(
        draw: *mut XftDraw,
        color: *const XftColor,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
    );
    fn XftDrawStringUtf8(
        draw: *mut XftDraw,
        color: *const XftColor,
        font: *mut XftFont,
        x: c_int,
        y: c_int,
        text: *const u8,
        len: c_int,
    );
    fn XftTextExtentsUtf8(
        display: *mut Display,
        font: *mut XftFont,
        text: *const u8,
        len: c_int,
        extents: *mut XGlyphInfo,
    );
    fn XftColorAllocValue(
        display: *mut Display,
        visual: *mut Visual,
        colormap: c_ulong,
        color: *const XRenderColor,
        result: *mut XftColor,
    ) -> c_int;
    fn XftColorFree(
        display: *mut Display,
        visual: *mut Visual,
        colormap: c_ulong,
        color: *mut XftColor,
    );
}

unsafe extern "C" {
    fn FcPatternCreate() -> *mut FcPattern;
    fn FcPatternDestroy(pattern: *mut FcPattern);
    fn FcPatternAddString(
        pattern: *mut FcPattern,
        object: *const c_char,
        value: *const u8,
    ) -> c_int;
    fn FcPatternAddDouble(pattern: *mut FcPattern, object: *const c_char, value: f64) -> c_int;
    fn FcPatternAddCharSet(
        pattern: *mut FcPattern,
        object: *const c_char,
        charset: *const FcCharSet,
    ) -> c_int;
    fn FcCharSetCreate() -> *mut FcCharSet;
    fn FcCharSetAddChar(charset: *mut FcCharSet, code_point: u32) -> c_int;
    fn FcCharSetDestroy(charset: *mut FcCharSet);
    fn FcConfigSubstitute(config: *mut c_void, pattern: *mut FcPattern, kind: c_int) -> c_int;
    fn FcDefaultSubstitute(pattern: *mut FcPattern);
    fn FcFontMatch(
        config: *mut c_void,
        pattern: *mut FcPattern,
        result: *mut c_int,
    ) -> *mut FcPattern;
}

/// One slot of [`PALETTE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Ink {
    Background = 0,
    Border = 1,
    Text = 2,
    Accent = 3,
    SelectedText = 4,
}

impl Ink {
    const COUNT: usize = 5;
}

/// Antialiased UTF-8 text for the menu window.
pub struct TextPainter {
    xlib: Xlib,
    display: *mut Display,
    font: *mut XftFont,
    draw: *mut XftDraw,
    visual: *mut Visual,
    colormap: c_ulong,
    ink: [XftColor; Ink::COUNT],
    ink_count: usize,
    ascent: i32,
    descent: i32,
}

impl TextPainter {
    /// Open the display and match a font covering every label.
    ///
    /// Usable for measurement before the menu window exists; call [`Self::attach`]
    /// once it does.
    pub fn load(labels: &[String]) -> BackendResult<Self> {
        let xlib = Xlib::open()
            .map_err(|error| BackendError::Unavailable(format!("cannot load libX11: {error}")))?;
        let display = unsafe { (xlib.XOpenDisplay)(ptr::null()) };
        if display.is_null() {
            return Err(BackendError::Unavailable(
                "cannot open the X display for text rendering".to_string(),
            ));
        }
        let mut painter = Self {
            xlib,
            display,
            font: ptr::null_mut(),
            draw: ptr::null_mut(),
            visual: ptr::null_mut(),
            colormap: 0,
            ink: unsafe { std::mem::zeroed() },
            ink_count: 0,
            ascent: 0,
            descent: 0,
        };
        painter.font = unsafe { match_font(painter.display, labels)? };
        painter.ascent = unsafe { (*painter.font).ascent };
        painter.descent = unsafe { (*painter.font).descent };
        Ok(painter)
    }

    pub fn ascent(&self) -> i32 {
        self.ascent
    }

    pub fn descent(&self) -> i32 {
        self.descent
    }

    /// Advance width of `text` in pixels, as shaped by the matched font.
    pub fn width(&self, text: &str) -> i32 {
        if text.is_empty() {
            return 0;
        }
        let mut extents = XGlyphInfo {
            width: 0,
            height: 0,
            x: 0,
            y: 0,
            x_off: 0,
            y_off: 0,
        };
        unsafe {
            XftTextExtentsUtf8(
                self.display,
                self.font,
                text.as_ptr(),
                text.len() as c_int,
                &mut extents,
            );
        }
        i32::from(extents.x_off)
    }

    /// Bind the painter to a menu window.
    ///
    /// A menu with submenus paints several windows; rebinding destroys only the
    /// drawable and keeps the palette, which is allocated for the screen's
    /// visual and colormap and therefore valid for every level.
    pub fn attach(&mut self, window: u32) -> BackendResult<()> {
        self.release_draw();
        let mut attributes = std::mem::MaybeUninit::<XWindowAttributes>::uninit();
        let status = unsafe {
            (self.xlib.XGetWindowAttributes)(
                self.display,
                c_ulong::from(window),
                attributes.as_mut_ptr(),
            )
        };
        if status == 0 {
            return Err(BackendError::Protocol(
                "XGetWindowAttributes failed for the menu window".to_string(),
            ));
        }
        let attributes = unsafe { attributes.assume_init() };
        if self.ink_count > 0
            && (self.visual != attributes.visual || self.colormap != attributes.colormap)
        {
            self.release_ink();
        }
        self.visual = attributes.visual;
        self.colormap = attributes.colormap;
        self.draw = unsafe {
            XftDrawCreate(
                self.display,
                c_ulong::from(window),
                self.visual,
                self.colormap,
            )
        };
        if self.draw.is_null() {
            return Err(BackendError::Protocol(
                "XftDrawCreate failed for the menu window".to_string(),
            ));
        }
        if self.ink_count == 0 {
            self.allocate_ink()?;
        }
        Ok(())
    }

    fn allocate_ink(&mut self) -> BackendResult<()> {
        for (index, rgb) in PALETTE.iter().enumerate() {
            let requested = XRenderColor {
                red: u16::from(rgb.0) * 257,
                green: u16::from(rgb.1) * 257,
                blue: u16::from(rgb.2) * 257,
                alpha: u16::MAX,
            };
            let mut allocated = XftColor {
                pixel: 0,
                color: requested,
            };
            let allocated_ok = unsafe {
                XftColorAllocValue(
                    self.display,
                    self.visual,
                    self.colormap,
                    &requested,
                    &mut allocated,
                )
            };
            if allocated_ok == 0 {
                self.release_ink();
                return Err(BackendError::Protocol(
                    "XftColorAllocValue failed for the menu palette".to_string(),
                ));
            }
            self.ink[index] = allocated;
            self.ink_count += 1;
        }
        Ok(())
    }

    /// Unbind from the current window before that window is destroyed.
    ///
    /// X destroys a Picture together with its drawable, so freeing it later would
    /// be a fatal `RenderBadPicture`. The window lives on the x11rb connection
    /// while this draw lives on the Xlib one, so the free must not only be queued
    /// but processed: hence the sync.
    pub fn detach(&mut self) {
        if self.draw.is_null() {
            return;
        }
        self.release_draw();
        unsafe {
            (self.xlib.XSync)(self.display, 0);
        }
    }

    fn release_draw(&mut self) {
        unsafe {
            if !self.draw.is_null() {
                XftDrawDestroy(self.draw);
                self.draw = ptr::null_mut();
            }
        }
    }

    fn release_ink(&mut self) {
        unsafe {
            for index in 0..self.ink_count {
                XftColorFree(
                    self.display,
                    self.visual,
                    self.colormap,
                    &mut self.ink[index],
                );
            }
        }
        self.ink_count = 0;
    }

    /// Fill a rectangle in window coordinates.
    pub fn fill(&self, ink: Ink, x: i32, y: i32, w: i32, h: i32) {
        if self.draw.is_null() || w <= 0 || h <= 0 {
            return;
        }
        let color = &self.ink[ink as usize];
        unsafe {
            XftDrawRect(self.draw, color, x, y, w as c_uint, h as c_uint);
        }
    }

    /// Draw `text` in window coordinates with `baseline` as its baseline.
    pub fn draw_text(&self, ink: Ink, x: i32, baseline: i32, text: &str) {
        if self.draw.is_null() || text.is_empty() {
            return;
        }
        let color = &self.ink[ink as usize];
        unsafe {
            XftDrawStringUtf8(
                self.draw,
                color,
                self.font,
                x,
                baseline,
                text.as_ptr(),
                text.len() as c_int,
            );
        }
    }

    /// Send the queued drawing requests to the server.
    pub fn flush(&self) {
        if self.display.is_null() {
            return;
        }
        unsafe {
            (self.xlib.XFlush)(self.display);
        }
    }
}

impl Drop for TextPainter {
    fn drop(&mut self) {
        self.release_draw();
        self.release_ink();
        unsafe {
            if !self.font.is_null() {
                XftFontClose(self.display, self.font);
            }
            if !self.display.is_null() {
                (self.xlib.XCloseDisplay)(self.display);
            }
        }
    }
}

/// Fontconfig match followed by Xft font open, taking the charset of `labels`.
unsafe fn match_font(display: *mut Display, labels: &[String]) -> BackendResult<*mut XftFont> {
    unsafe {
        let charset = FcCharSetCreate();
        if charset.is_null() {
            return Err(BackendError::Protocol("FcCharSetCreate failed".to_string()));
        }
        for label in labels {
            for character in label.chars() {
                FcCharSetAddChar(charset, character as u32);
            }
        }
        let pattern = FcPatternCreate();
        if pattern.is_null() {
            FcCharSetDestroy(charset);
            return Err(BackendError::Protocol("FcPatternCreate failed".to_string()));
        }
        let family = CString::new(MENU_FAMILY).expect("static family name");
        FcPatternAddString(pattern, c"family".as_ptr(), family.as_ptr() as *const u8);
        FcPatternAddDouble(pattern, c"size".as_ptr(), MENU_SIZE);
        FcPatternAddCharSet(pattern, c"charset".as_ptr(), charset);
        FcCharSetDestroy(charset);
        FcConfigSubstitute(ptr::null_mut(), pattern, FC_MATCH_PATTERN);
        FcDefaultSubstitute(pattern);
        let mut result: c_int = 0;
        let matched = FcFontMatch(ptr::null_mut(), pattern, &mut result);
        FcPatternDestroy(pattern);
        if matched.is_null() {
            return Err(BackendError::Protocol(
                "FcFontMatch found no font for the menu".to_string(),
            ));
        }
        // Xft takes ownership of the pattern handed to XftFontOpenPattern.
        let font = XftFontOpenPattern(display, matched);
        if font.is_null() {
            FcPatternDestroy(matched);
            return Err(BackendError::Protocol(
                "XftFontOpenPattern failed for the matched font".to_string(),
            ));
        }
        Ok(font)
    }
}
