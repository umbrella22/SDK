#![cfg(target_os = "linux")]

use std::error::Error;
use std::ffi::CStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use uda_core::error::UdaError;
use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};
use uda_platform_linux::wallpaper::LinuxWallpaperManager;

// Each assertion runs in a fresh process so PATH and desktop selection never
// interfere with unrelated tests or the platform's shared asynchronous bridge.
static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

const CLI_STUB: &str = r#"#!/usr/bin/python3
import os
import sys
import time

name = os.path.basename(sys.argv[0])
args = sys.argv[1:]
with open(os.environ['UDA_WALLPAPER_LOG'], 'a') as log:
    log.write(name + '|' + '|'.join(args) + '\n')
mode = os.environ['UDA_WALLPAPER_CASE']

if name == 'gsettings':
    key = args[-1]
    if key == 'color-scheme':
        print("'prefer-dark'" if mode == 'dark' else "'default'")
    elif mode == 'none':
        # The removed FFI fallback would make a fourth call and wrongly expose
        # this value after the platform already returned None.
        with open(os.environ['UDA_WALLPAPER_LOG']) as log:
            calls = len(log.readlines())
        print("'file:///tmp/stale.jpg'" if calls > 3 else "''")
    elif mode == 'invalid_utf8' and key == 'picture-uri':
        sys.stdout.buffer.write(b"'file:///tmp/\xff.jpg'\n")
    elif mode == 'invalid_utf8':
        print("''")
    elif key == 'picture-uri-dark':
        print(os.environ.get('UDA_DARK_VALUE', "''"))
    else:
        print(os.environ.get('UDA_LIGHT_VALUE', "''"))

elif name == 'hyprctl':
    if args == ['--version']:
        print('Hyprland v0.49')
    elif args == ['monitors']:
        print('Monitor eDP-1 (ID 0):')
    elif args[:2] == ['hyprpaper', 'preload']:
        if mode == 'timeout':
            time.sleep(30)
        elif mode == 'preload_error':
            print('error: image does not exist')
        elif mode == 'empty' or mode == 'reject_only':
            pass
        elif mode == 'unknown_reply':
            print('not ok')
        elif mode == 'invalid_reply_utf8':
            sys.stdout.buffer.write(b'ok\xff\n')
        elif mode == 'nonzero':
            print('ok')
            sys.exit(1)
        else:
            print('ok')
    elif args[:2] == ['hyprpaper', 'wallpaper']:
        print('error: image not preloaded' if mode == 'wallpaper_error' else 'ok')

elif name == 'swww':
    sys.exit(0)
"#;

struct Fixture {
    directory: PathBuf,
}

impl Fixture {
    fn new(tools: &[&str]) -> Result<Self, Box<dyn Error>> {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "uda-wallpaper-regression-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&directory)?;
        let fixture = Self { directory };
        for tool in tools {
            let path = fixture.directory.join(tool);
            fs::write(&path, CLI_STUB)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
        Ok(fixture)
    }

    fn run(
        &self,
        mode: &str,
        desktop: &str,
        expected: &str,
        values: &[(&str, &str)],
    ) -> Result<Vec<String>, Box<dyn Error>> {
        let log = self.directory.join("calls");
        fs::write(&log, "")?;
        let output = Command::new(std::env::current_exe()?)
            .args(["--exact", "wallpaper_cli_child", "--ignored", "--nocapture"])
            .env("PATH", &self.directory)
            .env("XDG_CURRENT_DESKTOP", desktop)
            .env("XDG_SESSION_TYPE", "wayland")
            .env_remove("DESKTOP_SESSION")
            .env("UDA_WALLPAPER_LOG", &log)
            .env("UDA_WALLPAPER_CASE", mode)
            .env("UDA_EXPECTED_PATH", expected)
            .envs(values.iter().copied())
            .output()?;
        assert!(
            output.status.success(),
            "{mode} child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(fs::read_to_string(log)?
            .lines()
            .map(str::to_owned)
            .collect())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
#[ignore = "invoked by parent tests with isolated environment and CLI fixtures"]
fn wallpaper_cli_child() -> Result<(), Box<dyn Error>> {
    let mode = std::env::var("UDA_WALLPAPER_CASE")?;
    if std::env::var("XDG_CURRENT_DESKTOP")? == "Hyprland" {
        let result = LinuxWallpaperManager::new().set_wallpaper(
            "/tmp/wall.jpg",
            &WallpaperOptions {
                fill_mode: FillMode::Fill,
                monitor_index: None,
                dark_mode: false,
            },
        );
        if mode == "reject_only" {
            // An explicit Hyprland session must not fall through to the X11
            // tools, and an empty hyprpaper reply (a rejection behind a zero
            // exit) must not be reported as success. With no second tool on
            // the PATH, the daemon's own diagnosis is the final answer.
            let error = result.expect_err("a rejected preload must not report success");
            assert!(
                matches!(error, UdaError::DetectionFailed(_)),
                "unexpected error: {error}"
            );
            assert!(
                error.to_string().contains("hyprpaper rejected"),
                "the daemon's diagnosis must reach the caller: {error}"
            );
        } else {
            result?;
        }
        return Ok(());
    }

    let mut path = std::ptr::null_mut();
    // SAFETY: path is a writable out-parameter; the returned allocation remains
    // live until uda_free_string below.
    let status = unsafe { uda_ffi::uda_get_wallpaper(&mut path) };
    assert_eq!(status, uda_ffi::UDA_OK);
    let actual = if path.is_null() {
        None
    } else {
        let value = unsafe { CStr::from_ptr(path) }.to_str().map(str::to_owned);
        unsafe { uda_ffi::uda_free_string(path) };
        Some(value?)
    };
    let expected = std::env::var("UDA_EXPECTED_PATH")?;
    assert_eq!(
        actual.as_deref(),
        (!expected.is_empty()).then_some(expected.as_str())
    );
    Ok(())
}

#[test]
fn ffi_does_not_probe_again_after_platform_returns_none() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new(&["gsettings"])?;
    for desktop in ["GNOME", "Unity", "ubuntu:GNOME", "GNOME-Flashback"] {
        let calls = fixture.run("none", desktop, "", &[])?;
        assert_eq!(
            calls,
            [
                "gsettings|get|org.gnome.desktop.interface|color-scheme",
                "gsettings|get|org.gnome.desktop.background|picture-uri",
                "gsettings|get|org.gnome.desktop.background|picture-uri-dark",
            ],
            "{desktop}"
        );
    }
    for desktop in ["KDE", "Hyprland:other", "unity-session", ""] {
        assert!(
            fixture.run("none", desktop, "", &[])?.is_empty(),
            "{desktop}"
        );
    }
    Ok(())
}

#[test]
fn ffi_uses_the_platform_dark_key_and_gvariant_decoder() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new(&["gsettings"])?;
    for desktop in ["GNOME", "Unity"] {
        let calls = fixture.run(
            "dark",
            desktop,
            "/tmp/it's\\中 a%#?.jpg",
            &[
                ("UDA_LIGHT_VALUE", "'file:///tmp/light.jpg'"),
                (
                    "UDA_DARK_VALUE",
                    r#"'file://localhost/tmp/it\'s\\\u4e2d%20a%25%23%3F.jpg'"#,
                ),
            ],
        )?;
        assert_eq!(calls.len(), 2);
        assert!(calls[1].ends_with("|picture-uri-dark"));
    }
    Ok(())
}

#[test]
fn malformed_primary_uri_falls_back_without_exposing_a_remote_path() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new(&["gsettings"])?;
    let calls = fixture.run(
        "light",
        "GNOME",
        "/tmp/local.jpg",
        &[
            ("UDA_LIGHT_VALUE", "'file://remote/tmp/wall.jpg'"),
            ("UDA_DARK_VALUE", "'file:///tmp/local.jpg'"),
        ],
    )?;
    assert_eq!(calls.len(), 3);
    assert_eq!(fixture.run("invalid_utf8", "GNOME", "", &[])?.len(), 3);
    Ok(())
}

#[test]
fn hyprpaper_requires_ok_and_otherwise_tries_swww() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new(&["hyprctl", "swww"])?;
    for mode in [
        "preload_error",
        "wallpaper_error",
        "empty",
        "unknown_reply",
        "invalid_reply_utf8",
        "nonzero",
    ] {
        let calls = fixture.run(mode, "Hyprland", "", &[])?;
        assert!(
            calls.iter().any(|call| call == "swww|img|/tmp/wall.jpg"),
            "{mode}: {calls:?}"
        );
        let applied = calls
            .iter()
            .any(|call| call.starts_with("hyprctl|hyprpaper|wallpaper|"));
        assert_eq!(applied, mode == "wallpaper_error", "{mode}: {calls:?}");
    }
    let calls = fixture.run("ok", "Hyprland", "", &[])?;
    assert_eq!(
        calls,
        [
            "hyprctl|--version",
            "hyprctl|hyprpaper|preload|/tmp/wall.jpg",
            "hyprctl|monitors",
            "hyprctl|hyprpaper|wallpaper|eDP-1,/tmp/wall.jpg",
        ]
    );
    Ok(())
}

#[test]
fn hyprpaper_timeout_is_bounded_and_continues_to_swww() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new(&["hyprctl", "swww"])?;
    let started = Instant::now();
    let calls = fixture.run("timeout", "Hyprland", "", &[])?;
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(calls.iter().any(|call| call == "swww|img|/tmp/wall.jpg"));
    assert!(!calls
        .iter()
        .any(|call| call.starts_with("hyprctl|hyprpaper|wallpaper|")));
    Ok(())
}

#[test]
fn hyprpaper_error_without_swww_is_not_reported_as_success() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new(&["hyprctl"])?;
    // An empty IPC reply is a rejection even when hyprctl exits zero, and with
    // no swww on the PATH nothing may paper over it: the daemon's diagnosis is
    // the final answer, and swww is never invoked.
    let calls = fixture.run("reject_only", "Hyprland", "", &[])?;
    assert!(
        !calls.iter().any(|call| call.starts_with("swww|")),
        "the rejected hyprpaper attempt must not fall through to swww: {calls:?}"
    );
    Ok(())
}
