// Entry points are called from C with raw pointers.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use ic_plugin_api::{
    check_host, needs_up_to, HostCheck, IcBytes, IcFsSource, IcHost, IcViewVTable, IcViewerVTable,
    IC_ABI_VERSION, IC_ERR_HOST_TOO_OLD, IC_ERR_HOST_UNKNOWN, IC_ERR_INIT_FAILED, IC_HOST_CONSOLE,
    IC_OK, IC_OPEN_READ,
};
use pdfium_render::prelude::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../version.rs"));

ic_plugin_api::declare_about!(
    "ic-pdf-view",
    "Documents",
    plugins_version!(),
    "Shows documents page by page"
);

pub const ID: &str = "pdf-view";
pub const EXTENSIONS: &str = ".pdf";

const RENDERED_WIDTH: i32 = 1600;

static HOST: AtomicUsize = AtomicUsize::new(0);

fn host() -> *const IcHost {
    HOST.load(Ordering::Relaxed) as *const IcHost
}

struct Showing {
    name: String,
    local: Option<String>,
    held: Option<Vec<u8>>,
    pages: u16,
    at: u16,
    /// Backs the pointer `viewer_content` returns; must stay alive until the host asks again.
    drawn: Vec<u8>,
}

thread_local! {
    static SHOWING: RefCell<BTreeMap<u64, Showing>> = const { RefCell::new(BTreeMap::new()) };
    static DOCUMENT: RefCell<String> = const { RefCell::new(String::new()) };
    static ANSWER: RefCell<String> = const { RefCell::new(String::new()) };
}

fn pdfium() -> Option<&'static Pdfium> {
    static HELD: std::sync::OnceLock<Option<Pdfium>> = std::sync::OnceLock::new();
    HELD.get_or_init(|| {
        for at in looked_for() {
            if let Ok(bindings) = Pdfium::bind_to_library(&at) {
                return Some(Pdfium::new(bindings));
            }
        }
        Pdfium::bind_to_system_library().ok().map(Pdfium::new)
    })
    .as_ref()
}

fn looked_for() -> Vec<std::path::PathBuf> {
    let mut places = Vec::new();
    let beside_us = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    if let Some(beside) = beside_us {
        places.push(Pdfium::pdfium_platform_library_name_at_path(
            &beside.to_string_lossy().to_string(),
        ));
        // A macOS bundle keeps its libraries one level up.
        places.push(Pdfium::pdfium_platform_library_name_at_path(
            &beside.join("../Libs").to_string_lossy().to_string(),
        ));
    }
    if cfg!(target_os = "linux") {
        places.push("/usr/lib/ice-commander/libpdfium.so".into());
        places.push("/usr/lib/libpdfium.so".into());
    }
    places
}

fn read_whole(source: IcFsSource, path: &CStr) -> Option<Vec<u8>> {
    let host = host();
    if host.is_null() {
        return None;
    }
    let stream = unsafe { ((*host).fs_open)(source, path.as_ptr(), IC_OPEN_READ) };
    if stream.is_null() {
        return None;
    }
    let mut held = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = unsafe { ((*host).fs_read)(stream, buffer.as_mut_ptr(), buffer.len() as u64) };
        if read <= 0 {
            break;
        }
        held.extend_from_slice(&buffer[..read as usize]);
    }
    unsafe { ((*host).fs_close)(stream) };
    Some(held)
}

fn local_path(source: IcFsSource, path: &CStr) -> Option<String> {
    let host = host();
    if host.is_null() {
        return None;
    }
    let answered = unsafe { ((*host).fs_local_path)(source, path.as_ptr()) };
    if answered.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(answered) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Not cached: pdfium keeps the file open, and an open window must not hold a lock on it.
fn with_document<R>(showing: &Showing, work: impl FnOnce(&PdfDocument) -> R) -> Option<R> {
    let pdfium = pdfium()?;
    if let Some(at) = &showing.local {
        if let Ok(document) = pdfium.load_pdf_from_file(at, None) {
            return Some(work(&document));
        }
    }
    let held = showing.held.as_ref()?;
    let document = pdfium.load_pdf_from_byte_slice(held, None).ok()?;
    Some(work(&document))
}

extern "C" fn viewer_open(
    instance: u64,
    source: IcFsSource,
    path: *const c_char,
    _user_data: *mut c_void,
) -> c_int {
    if path.is_null() {
        return IC_ERR_INIT_FAILED;
    }
    let held = unsafe { CStr::from_ptr(path) }.to_owned();
    let name = held.to_string_lossy().into_owned();
    let local = local_path(source, &held);
    let whole = match local {
        Some(_) => None,
        None => read_whole(source, &held),
    };
    let mut showing = Showing {
        name,
        local,
        held: whole,
        pages: 0,
        at: 0,
        drawn: Vec::new(),
    };
    let counted = with_document(&showing, |document| document.pages().len()).unwrap_or(0);
    if counted <= 0 {
        return IC_ERR_INIT_FAILED;
    }
    showing.pages = counted as u16;
    SHOWING.with(|held| held.borrow_mut().insert(instance, showing));
    IC_OK
}

extern "C" fn viewer_closed(instance: u64, _user_data: *mut c_void) {
    SHOWING.with(|held| held.borrow_mut().remove(&instance));
}

pub fn document_for(name: &str, at: u16, pages: u16) -> String {
    serde_json::json!({
        "schema": 1,
        "data": {
            "at": at + 1,
            "pages": pages,
            "can_back": at > 0,
            "can_go_on": at + 1 < pages
        },
        "fields": [],
        "form": {
            "t": "view",
            "surface": "window",
            "spacing": 8,
            "padding": 8,
            "children": [
                {
                    "t": "image",
                    "id": "page",
                    "src": format!("part:page/{at}"),
                    "fit": "contain",
                    "zoom": true,
                    "weight": 1
                },
                {
                    "t": "row",
                    "spacing": 8,
                    "children": [
                        {
                            "t": "button",
                            "id": "back",
                            "label": { "literal": "\u{25c0}" },
                            "accel": "Left",
                            "sensitive": { "truthy": "data.can_back" },
                            "intent": { "do": "emit", "node": "back" }
                        },
                        {
                            "t": "button",
                            "id": "on",
                            "label": { "literal": "\u{25b6}" },
                            "accel": "Right",
                            "sensitive": { "truthy": "data.can_go_on" },
                            "intent": { "do": "emit", "node": "on" }
                        },
                        {
                            "t": "text",
                            "id": "where",
                            "role": "dim",
                            "weight": 1,
                            "text": { "literal": format!("{name} — {} / {pages}", at + 1) }
                        }
                    ]
                }
            ]
        }
    })
    .to_string()
}

fn asking_about(raw: *const u8, len: u64) -> u64 {
    if raw.is_null() || len == 0 {
        return 0;
    }
    let held = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(raw, len as usize) });
    serde_json::from_str::<serde_json::Value>(&held)
        .ok()
        .and_then(|held| held.get("instance").and_then(|at| at.as_u64()))
        .unwrap_or(0)
}

extern "C" fn viewer_describe(ctx: *const u8, len: u64, _user_data: *mut c_void) -> IcBytes {
    let asked = asking_about(ctx, len);
    let drawn = SHOWING.with(|held| {
        let held = held.borrow();
        let showing = held.get(&asked)?;
        Some(document_for(&showing.name, showing.at, showing.pages))
    });
    let Some(drawn) = drawn else {
        return IcBytes::EMPTY;
    };
    DOCUMENT.with(|held| {
        let mut held = held.borrow_mut();
        *held = drawn;
        IcBytes {
            data: held.as_ptr(),
            len: held.len() as u64,
        }
    })
}

extern "C" fn viewer_event(raw: *const u8, len: u64, _user_data: *mut c_void) -> IcBytes {
    let asked = asking_about(raw, len);
    let event: serde_json::Value = if raw.is_null() || len == 0 {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(unsafe { std::slice::from_raw_parts(raw, len as usize) })
            .unwrap_or(serde_json::Value::Null)
    };
    let node = event
        .get("node")
        .and_then(|node| node.as_str())
        .unwrap_or("");
    let turned = SHOWING.with(|held| {
        let mut held = held.borrow_mut();
        let Some(showing) = held.get_mut(&asked) else {
            return false;
        };
        let was = showing.at;
        showing.at = match node {
            "on" => showing.at.saturating_add(1).min(showing.pages - 1),
            "back" => showing.at.saturating_sub(1),
            _ => showing.at,
        };
        showing.at != was
    });
    answer_with(if turned {
        r#"{"redescribe":true}"#
    } else {
        "{}"
    })
}

fn answer_with(text: &str) -> IcBytes {
    ANSWER.with(|held| {
        let mut held = held.borrow_mut();
        *held = text.to_string();
        IcBytes {
            data: held.as_ptr(),
            len: held.len() as u64,
        }
    })
}

extern "C" fn viewer_content(
    instance: u64,
    name: *const c_char,
    _user_data: *mut c_void,
) -> IcBytes {
    if name.is_null() {
        return IcBytes::EMPTY;
    }
    let asked = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    let Some(wanted) = asked.strip_prefix("page/").and_then(|at| at.parse().ok()) else {
        return IcBytes::EMPTY;
    };
    SHOWING.with(|held| {
        let mut held = held.borrow_mut();
        let Some(showing) = held.get_mut(&instance) else {
            return IcBytes::EMPTY;
        };
        let Some(made) = rendered(showing, wanted) else {
            return IcBytes::EMPTY;
        };
        showing.drawn = made;
        IcBytes {
            data: showing.drawn.as_ptr(),
            len: showing.drawn.len() as u64,
        }
    })
}

fn rendered(showing: &Showing, at: u16) -> Option<Vec<u8>> {
    with_document(showing, |document| {
        let page = document.pages().get(at as i32).ok()?;
        let config = PdfRenderConfig::new()
            .set_target_width(RENDERED_WIDTH)
            .set_clear_color(PdfColor::new(255, 255, 255, 255));
        let bitmap = page.render_with_config(&config).ok()?;
        let drawn = bitmap.as_image().ok()?;
        let mut held = std::io::Cursor::new(Vec::new());
        drawn
            .write_to(&mut held, image::ImageFormat::Png)
            .ok()
            .map(|()| held.into_inner())
    })
    .flatten()
}

pub fn view_vtable() -> IcViewVTable {
    IcViewVTable {
        struct_size: std::mem::size_of::<IcViewVTable>() as u32,
        describe: viewer_describe,
        on_event: Some(viewer_event),
        closed: None,
    }
}

pub fn viewer_vtable(view: *const IcViewVTable) -> IcViewerVTable {
    IcViewerVTable {
        struct_size: std::mem::size_of::<IcViewerVTable>() as u32,
        view,
        open: viewer_open,
        closed: Some(viewer_closed),
        content: Some(viewer_content),
        closing: None,
        canvas_ready: None,
        canvas_draw: None,
        canvas_gone: None,
    }
}

fn in_console(kind: *const c_char) -> bool {
    !kind.is_null() && unsafe { CStr::from_ptr(kind) }.to_bytes() == IC_HOST_CONSOLE.as_bytes()
}

#[cfg_attr(feature = "export-abi", no_mangle)]
pub extern "C" fn ic_plugin_init(host: *const IcHost, kind: *const c_char) -> c_int {
    if in_console(kind) {
        return ic_plugin_api::IC_ERR_NOT_THIS_HOST;
    }
    match check_host(
        host,
        IC_ABI_VERSION,
        needs_up_to(std::mem::offset_of!(IcHost, register_viewer)),
    ) {
        HostCheck::Ok => {}
        HostCheck::WrongMagic => return IC_ERR_HOST_UNKNOWN,
        HostCheck::TooOld { .. } | HostCheck::Truncated { .. } => return IC_ERR_HOST_TOO_OLD,
    }
    HOST.store(host as usize, Ordering::Relaxed);
    let (Ok(id), Ok(extensions)) = (CString::new(ID), CString::new(EXTENSIONS)) else {
        return IC_ERR_INIT_FAILED;
    };
    let window = view_vtable();
    let viewer = viewer_vtable(&window);
    unsafe {
        ((*host).register_viewer)(
            id.as_ptr(),
            extensions.as_ptr(),
            0,
            &viewer,
            std::ptr::null_mut(),
        )
    }
}

#[cfg_attr(feature = "export-abi", no_mangle)]
pub extern "C" fn ic_plugin_shutdown() {
    SHOWING.with(|held| held.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_turn_asks_for_the_window_to_be_described_again() {
        SHOWING.with(|held| {
            held.borrow_mut().insert(
                4,
                Showing {
                    name: "report.pdf".to_string(),
                    local: None,
                    held: None,
                    pages: 3,
                    at: 0,
                    drawn: Vec::new(),
                },
            )
        });

        assert_eq!(pressed(4, "back"), "{}", "the first page stays put");
        assert_eq!(pressed(4, "on"), r#"{"redescribe":true}"#);
        assert!(described(4).contains("part:page/1"));
        assert_eq!(pressed(4, "on"), r#"{"redescribe":true}"#);
        assert_eq!(pressed(4, "on"), "{}", "and the last page stays put");
        assert!(described(4).contains("part:page/2"));

        assert_eq!(pressed(4, "something-else"), "{}");
        assert_eq!(pressed(99, "on"), "{}", "a window nobody opened");
        viewer_closed(4, std::ptr::null_mut());
        assert!(described(4).is_empty());
    }

    fn pressed(instance: u64, node: &str) -> String {
        let event = serde_json::json!({
            "type": "activate",
            "node": node,
            "values": {},
            "instance": instance
        })
        .to_string();
        answered(viewer_event(
            event.as_ptr(),
            event.len() as u64,
            std::ptr::null_mut(),
        ))
    }

    fn described(instance: u64) -> String {
        let context = format!(r#"{{"host":{{"kind":"gtk"}},"instance":{instance}}}"#);
        answered(viewer_describe(
            context.as_ptr(),
            context.len() as u64,
            std::ptr::null_mut(),
        ))
    }

    fn answered(bytes: IcBytes) -> String {
        if bytes.data.is_null() {
            return String::new();
        }
        String::from_utf8_lossy(unsafe {
            std::slice::from_raw_parts(bytes.data, bytes.len as usize)
        })
        .into_owned()
    }

    #[test]
    fn a_part_that_is_not_a_page_answers_nothing() {
        let asked = CString::new("something/else").expect("a name");
        assert!(viewer_content(1, asked.as_ptr(), std::ptr::null_mut())
            .data
            .is_null());
        let page = CString::new("page/0").expect("a name");
        assert!(viewer_content(404, page.as_ptr(), std::ptr::null_mut())
            .data
            .is_null());
    }

    #[test]
    fn a_terminal_has_nothing_to_render_pages_onto() {
        let host = ic_plugin_api::testing::silent_host();
        let console = CString::new(IC_HOST_CONSOLE).expect("a kind");
        assert_ne!(ic_plugin_init(&host, console.as_ptr()), IC_OK);
    }
}
