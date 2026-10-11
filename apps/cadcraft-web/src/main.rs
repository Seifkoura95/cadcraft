//! CADCraft in the browser.
//!
//! Runs the same [`cadcraft_ui_egui::CadApp`] as the desktop app through eframe's web runner
//! (wgpu: WebGPU where available, WebGL2 otherwise). Build with `trunk build --release` from this
//! directory (output in `dist/web`). URL flags: `?webgl` forces WebGL2, `?sample` opens the sample
//! drawing.
//!
//! With the `simplehub` cargo feature the page does not start anything itself: it exports
//! `start_simple_cad(canvas, bridge)` for SimpleHub to mount Simple CAD on its own canvas (see
//! [`simplehub`]), and files go through the bridge instead of the browser.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

/// The eframe app around [`CadApp`].
#[cfg(target_arch = "wasm32")]
struct Shell(cadcraft_ui_egui::CadApp);

#[cfg(target_arch = "wasm32")]
impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.0.logic(ctx);
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.ui(ui);
    }
}

/// File format hooks for the engine (it stays I/O-agnostic). `sink` takes written files where
/// there is no file system (SimpleHub's bridge); without one, saving to a path fails on the web.
#[cfg(target_arch = "wasm32")]
fn install_io(sink: Option<cadcraft_engine::cmd::file::SinkHook>) {
    cadcraft_engine::cmd::file::set_io(cadcraft_engine::cmd::file::IoHooks {
        read: |b, name| cadcraft_io::read(b, name).map_err(|e| e.to_string()),
        write: |d, name| cadcraft_io::write(d, name).map_err(|e| e.to_string()),
        plot: Some(|d, space, opts| cadcraft_io::plot(d, space, opts).map_err(|e| e.to_string())),
        sink,
    });
}

/// The standalone page: the app on `<canvas id="cadcraft_canvas">`.
#[cfg(all(target_arch = "wasm32", not(feature = "simplehub")))]
mod web {
    use cadcraft_engine::Session;
    use cadcraft_ui_egui::{CadApp, Services};
    use wasm_bindgen::JsCast as _;

    use super::Shell;

    const CANVAS_ID: &str = "cadcraft_canvas";
    const LOADING_ID: &str = "cadcraft_loading";

    fn query() -> String {
        web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
    }

    pub fn start() {
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        super::install_io(None);
        wasm_bindgen_futures::spawn_local(async {
            let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
            let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
                log::error!("missing <canvas id=\"{CANVAS_ID}\">");
                return;
            };
            let mut options = eframe::WebOptions::default();
            if query().contains("webgl")
                && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
            {
                create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
            }
            let result = eframe::WebRunner::new()
                .start(
                    canvas,
                    options,
                    Box::new(move |cc| {
                        let mut app = CadApp::new(Session::new(), Services::default());
                        if let Some(rs) = &cc.wgpu_render_state {
                            app.set_wgpu(rs);
                        }
                        if query().contains("sample") {
                            let _ = app.run("ui.sample", serde_json::json!({}));
                        }
                        Ok(Box::new(Shell(app)))
                    }),
                )
                .await;
            if let Some(el) = document.get_element_by_id(LOADING_ID) {
                match result {
                    Ok(()) => el.remove(),
                    Err(e) => el.set_inner_html(&format!(
                        "<p>{} failed to start: {e:?}</p><p>A browser with WebGPU or WebGL2 is required.</p>",
                        cadcraft_ui_egui::APP_NAME
                    )),
                }
            }
        });
    }
}

/// Simple CAD inside SimpleHub. The page mounts the app on a canvas it owns and hands over a
/// bridge object (`open(purpose): Promise<{name, bytes}|null>`, `save(name, bytes): Promise<void>`)
/// that moves files to and from Simple Cloud; nothing here touches the browser's file picker or
/// downloads. `open` is called with `"open"` (a drawing: DXF, or DWG once the web build reads it).
/// Every written file (Save, Save As, Export to PDF, Print, WBLOCK) reaches `save` with the file
/// name and the bytes; the engine's `IoHooks::sink` is how they get here.
#[cfg(all(target_arch = "wasm32", feature = "simplehub"))]
mod simplehub {
    use std::cell::RefCell;

    use cadcraft_engine::Session;
    use cadcraft_ui_egui::{CadApp, Inbox, Services};
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;

    use super::Shell;

    const DEFAULT_NAME: &str = "Drawing.dxf";

    thread_local! {
        /// The bridge, for the engine's sink (a plain `fn`, so it cannot capture it).
        static BRIDGE: RefCell<Option<JsValue>> = const { RefCell::new(None) };
    }

    /// A running instance. SimpleHub calls `destroy` when the module unmounts.
    #[wasm_bindgen]
    pub struct CadHandle {
        runner: eframe::WebRunner,
    }

    #[wasm_bindgen]
    impl CadHandle {
        pub fn destroy(&self) {
            self.runner.destroy();
        }

        pub fn has_panicked(&self) -> bool {
            self.runner.has_panicked()
        }
    }

    /// Starts Simple CAD on `canvas`, with `bridge` providing open and save.
    #[wasm_bindgen]
    pub async fn start_simple_cad(canvas: web_sys::HtmlCanvasElement, bridge: JsValue) -> Result<CadHandle, JsValue> {
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        BRIDGE.with(|b| *b.borrow_mut() = Some(bridge.clone()));
        super::install_io(Some(sink));
        let runner = eframe::WebRunner::new();
        runner
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(move |cc| {
                    let inbox: Inbox = Inbox::default();
                    let mut app = CadApp::new(Session::new(), services(bridge, inbox, cc.egui_ctx.clone()));
                    if let Some(rs) = &cc.wgpu_render_state {
                        app.set_wgpu(rs);
                    }
                    Ok(Box::new(Shell(app)))
                }),
            )
            .await?;
        Ok(CadHandle { runner })
    }

    fn services(bridge: JsValue, inbox: Inbox, ctx: egui::Context) -> Services {
        let open_inbox = inbox.clone();
        Services {
            open_async: Some(Box::new(move |purpose: &str| {
                let bridge = bridge.clone();
                let inbox = open_inbox.clone();
                let ctx = ctx.clone();
                let purpose = purpose.to_string();
                wasm_bindgen_futures::spawn_local(async move {
                    match call_open(&bridge, &purpose).await {
                        Ok(Some((name, bytes))) => {
                            inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                            ctx.request_repaint();
                        }
                        Ok(None) => {}
                        Err(e) => log::error!("SimpleHub open failed: {e:?}"),
                    }
                });
            })),
            // Save As and exports ask for a name; the bridge decides where it goes.
            pick_save: Some(Box::new(|name: &str| Some(name.to_string()))),
            inbox: Some(inbox),
            ..Default::default()
        }
    }

    /// The engine's sink: hands `bytes` to the bridge as the file named by the last component of
    /// `path`. Fire and forget; a rejected promise is logged.
    fn sink(path: &str, bytes: &[u8]) -> Result<(), String> {
        let Some(bridge) = BRIDGE.with(|b| b.borrow().clone()) else { return Err("the SimpleHub bridge is not installed".into()) };
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| DEFAULT_NAME.into());
        let bytes = js_sys::Uint8Array::from(bytes);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = call_save(&bridge, &name, &bytes).await {
                log::error!("SimpleHub save of {name} failed: {e:?}");
            }
        });
        Ok(())
    }

    fn method(bridge: &JsValue, name: &str) -> Result<js_sys::Function, JsValue> {
        js_sys::Reflect::get(bridge, &JsValue::from_str(name))?
            .dyn_into::<js_sys::Function>()
            .map_err(|_| JsValue::from_str(&format!("bridge.{name} is not a function")))
    }

    async fn call_open(bridge: &JsValue, purpose: &str) -> Result<Option<(String, Vec<u8>)>, JsValue> {
        let promise: js_sys::Promise = method(bridge, "open")?.call1(bridge, &JsValue::from_str(purpose))?.dyn_into()?;
        let result = JsFuture::from(promise).await?;
        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }
        let name = js_sys::Reflect::get(&result, &JsValue::from_str("name"))?.as_string().unwrap_or_else(|| DEFAULT_NAME.into());
        let bytes: js_sys::Uint8Array = js_sys::Reflect::get(&result, &JsValue::from_str("bytes"))?.dyn_into()?;
        Ok(Some((name, bytes.to_vec())))
    }

    async fn call_save(bridge: &JsValue, name: &str, bytes: &js_sys::Uint8Array) -> Result<(), JsValue> {
        let ret = method(bridge, "save")?.call2(bridge, &JsValue::from_str(name), bytes)?;
        if let Ok(promise) = ret.dyn_into::<js_sys::Promise>() {
            JsFuture::from(promise).await?;
        }
        Ok(())
    }
}

#[cfg(all(target_arch = "wasm32", not(feature = "simplehub")))]
fn main() {
    web::start();
}

/// SimpleHub mounts the app itself through [`simplehub::start_simple_cad`].
#[cfg(all(target_arch = "wasm32", feature = "simplehub"))]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("cadcraft-web only runs in the browser: build it with `trunk build --release` in apps/cadcraft-web");
}
