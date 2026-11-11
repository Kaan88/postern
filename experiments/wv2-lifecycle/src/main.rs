use std::{thread, time::{Duration, Instant}};
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "destroy".into());
    let cycles: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let t0 = Instant::now();
    let app = tauri::Builder::default()
        .build(tauri::generate_context!())
        .expect("build app");
    let handle = app.handle().clone();
    println!("PID {} mode={} cycles={}", std::process::id(), mode, cycles);
    thread::spawn(move || {
        let log = |m: &str| println!("[{:>6.1}s] {}", t0.elapsed().as_secs_f32(), m);
        thread::sleep(Duration::from_secs(5));
        log("baseline-end");
        for i in 0..cycles {
            let label = format!("w{i}");
            WebviewWindowBuilder::new(&handle, &label, WebviewUrl::App("index.html".into()))
                .title(&label)
                .build()
                .expect("open window");
            log(&format!("open {label}"));
            thread::sleep(Duration::from_secs(12));
            let w = handle.get_webview_window(&label).expect("window");
            if mode == "close" { w.close().unwrap() } else { w.destroy().unwrap() }
            log(&format!("closed {label}"));
            thread::sleep(Duration::from_secs(25));
        }
        log("idle-start");
        thread::sleep(Duration::from_secs(60));
        log("exit");
        handle.exit(0);
    });
    app.run(|_, e| {
        if let RunEvent::ExitRequested { api, code, .. } = e {
            if code.is_none() { api.prevent_exit(); }
        }
    });
}
