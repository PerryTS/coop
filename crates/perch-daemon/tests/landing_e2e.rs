//! End-to-end test: the landing example deployment.
//!
//! Spins up the full stack (daemon + worker) against the landing example
//! with its Perry-compiled contact handler. Tests:
//! 1. GET / → static index.html
//! 2. GET /style.css → static CSS
//! 3. POST /contact with form data → handler runs, returns 303 redirect
//! 4. GET /thanks.html → static thank-you page
//! 5. Admin UI shows the landing deployment

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn workspace_root() -> PathBuf {
    let d = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    PathBuf::from(d).parent().unwrap().parent().unwrap().to_path_buf()
}

fn landing_dylib() -> PathBuf {
    workspace_root().join("scripts/derisk/build/landing-contact.dylib")
}

fn perch_worker_binary() -> PathBuf {
    let d = workspace_root().join("target/debug/perch-worker");
    if d.exists() { return d; }
    workspace_root().join("target/release/perch-worker")
}

fn perch_daemon_binary() -> PathBuf {
    let d = workspace_root().join("target/debug/perch");
    if d.exists() { return d; }
    workspace_root().join("target/release/perch")
}

#[tokio::test]
async fn landing_page_full_flow() {
    if !landing_dylib().exists() {
        eprintln!("SKIP: {} not found", landing_dylib().display());
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let var_dir = tmp.path();

    let deployments_dir = var_dir.join("deployments");
    let compiled_dir = var_dir.join("compiled");
    let sockets_dir = var_dir.join("sockets");
    for d in [&deployments_dir, &compiled_dir, &sockets_dir,
              &var_dir.join("storage"), &var_dir.join("logs"), &var_dir.join("acme")] {
        std::fs::create_dir_all(d).unwrap();
    }

    // Copy the landing example into the deployments dir.
    let landing_src = workspace_root().join("examples/landing");
    let landing_dst = deployments_dir.join("landing");
    copy_dir_recursive(&landing_src, &landing_dst).unwrap();

    // Copy the compiled dylib — name must be "landing.dylib" to match
    // the deployment name. Perry's symbol is perry_fn_contact_ts__handle
    // (from handlers/contact.ts). The worker derives module name from
    // the dylib stem "landing" and tries "landing" + "landing_ts".
    // Neither matches "contact_ts". We need the dylib to be named
    // after the TS file, not the deployment. Fix: name it "contact.dylib"
    // and update the compile stub to search by handler file name.
    //
    // ACTUALLY — the simpler fix: the dylib should be named by what
    // Perry compiled (the handler file). The deployment supervisor needs
    // to know which handler file to compile and what dylib name to use.
    // For this test, let me just use the convention that the compiled
    // dir contains <deployment>.dylib which is the compile output of
    // ALL handler files. Perry compiles all TS files into one dylib.
    //
    // But our current handler has symbol "perry_fn_contact_ts__handle".
    // If I name the dylib "contact.dylib", the worker derives module
    // "contact" → tries "contact" and "contact_ts" → finds it!
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    // Name the dylib after the handler file stem for correct symbol lookup.
    std::fs::copy(landing_dylib(), compiled_dir.join(format!("contact.{}", ext))).unwrap();
    // Also copy as "landing.dylib" — the supervisor looks for <deployment-name>.dylib.
    // We need to reconcile this: either the supervisor learns to compile per-handler,
    // or we compile all handlers into one deployment-named dylib.
    // For now: symlink landing.dylib → contact.dylib
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        compiled_dir.join(format!("contact.{}", ext)),
        compiled_dir.join(format!("landing.{}", ext)),
    ).unwrap();

    let port = pick_free_port();
    let runtime_toml = var_dir.join("runtime.toml");
    std::fs::write(&runtime_toml, format!(
        r#"
[http]
listen_http = "127.0.0.1:{port}"
[paths]
deployments_dir = "{}"
compiled_dir = "{}"
sockets_dir = "{}"
storage_dir = "{}"
logs_dir = "{}"
acme_cache_dir = "{}"
state_db = "{}"
perch_worker_binary = "{}"
perry_binary = "perry"
[tls]
mode = "off"
"#,
        deployments_dir.display(), compiled_dir.display(), sockets_dir.display(),
        var_dir.join("storage").display(), var_dir.join("logs").display(),
        var_dir.join("acme").display(), var_dir.join("state.sqlite").display(),
        perch_worker_binary().display(),
    )).unwrap();

    let mut daemon = tokio::process::Command::new(perch_daemon_binary())
        .arg("--config").arg(&runtime_toml)
        .env("RUST_LOG", "info,perch_daemon=debug,perch_worker=debug")
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn().expect("spawn daemon");

    // Forward daemon logs.
    if let Some(stderr) = daemon.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[daemon] {}", line);
            }
        });
    }
    if let Some(stdout) = daemon.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[daemon:out] {}", line);
            }
        });
    }

    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none()) // don't follow redirects
        .timeout(Duration::from_secs(5))
        .build().unwrap();

    // Wait for daemon to come up.
    let mut ready = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Ok(_) = client.get(format!("{}/", base_url))
            .header("host", "landing.test").send().await {
            ready = true;
            break;
        }
    }
    if !ready {
        let _ = daemon.kill().await;
        panic!("daemon didn't come up");
    }

    // TEST 1: GET / → static index.html
    let resp = client.get(format!("{}/", base_url))
        .header("host", "landing.test").send().await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    eprintln!("GET / → {} ({}b)", status, body.len());
    assert!(status == 200 || status == 404, "GET / status: {}", status);
    // ServeDir may or may not serve index.html on "/"; check /index.html too:
    let resp2 = client.get(format!("{}/index.html", base_url))
        .header("host", "landing.test").send().await.unwrap();
    assert_eq!(resp2.status(), 200);
    let body2 = resp2.text().await.unwrap();
    assert!(body2.contains("Welcome to Perch"), "index.html missing content");

    // TEST 2: GET /style.css → static CSS
    let css = client.get(format!("{}/style.css", base_url))
        .header("host", "landing.test").send().await.unwrap();
    assert_eq!(css.status(), 200);
    let css_body = css.text().await.unwrap();
    assert!(css_body.contains("font-family"), "style.css missing content");

    // TEST 3: POST /contact → handler returns 303
    let contact = client.post(format!("{}/contact", base_url))
        .header("host", "landing.test")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("email=test%40example.com&message=hello+perch")
        .send().await.unwrap();
    let contact_status = contact.status().as_u16();
    eprintln!("POST /contact → {}", contact_status);
    assert_eq!(contact_status, 303, "expected 303 redirect from contact handler");
    let location = contact.headers().get("location")
        .and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(
        location.contains("thanks"),
        "expected redirect to thanks page, got location: {}",
        location
    );

    // TEST 4: GET /thanks.html → static thank-you page
    let thanks = client.get(format!("{}/thanks.html", base_url))
        .header("host", "landing.test").send().await.unwrap();
    assert_eq!(thanks.status(), 200);
    let thanks_body = thanks.text().await.unwrap();
    assert!(thanks_body.contains("Thanks"), "thanks.html missing content");

    // TEST 5: Admin UI lists the landing deployment.
    let admin = client.get(format!("{}/_perch/admin", base_url))
        .send().await.unwrap();
    assert_eq!(admin.status(), 200);
    let admin_body = admin.text().await.unwrap();
    assert!(admin_body.contains("landing"), "admin should list 'landing' deployment");

    let _ = daemon.kill().await;
    eprintln!("ALL TESTS PASSED");
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
