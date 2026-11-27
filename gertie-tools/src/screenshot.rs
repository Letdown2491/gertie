//! The `screenshot` tool (#50): lets the model look at what it built.
//! Two sources. A URL renders in headless Chromium, driven over the
//! DevTools protocol on a pipe (`--remote-debugging-pipe`): viewport
//! size, light or dark, and full page, with no port opened and no file
//! shared, which is also what makes a Flatpak Chromium work. A window
//! or the whole screen comes from the compositor's own tools (niri,
//! Hyprland and Sway through `grim`, GNOME and KDE through their
//! screenshot commands, X11 through ImageMagick).
//!
//! Every capture is saved under the cache dir (the text names the
//! file, so a model that can't see still knows where it is) and goes
//! to the model through [`crate::images::fit`].

use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gertie_core::{Tool, ToolCall, ToolResult, WorkerProgress};
use serde::Deserialize;

/// A full-page capture stops after this many viewport-sized tiles.
const MAX_TILES: u32 = 6;

/// How long a whole capture may take, browser start to last byte.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(45);

pub struct Screenshot {
    root: PathBuf,
}

impl Screenshot {
    pub fn new(root: &Path) -> Self {
        Self { root: root.into() }
    }
}

#[derive(Deserialize, Default)]
struct ShotArgs {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    window: Option<String>,
    #[serde(default)]
    screen: Option<bool>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    color_scheme: Option<String>,
    #[serde(default)]
    full_page: Option<bool>,
    #[serde(default)]
    wait_ms: Option<u64>,
}

/// One capture's answer: the text the model reads, and the images.
type Shot = Result<(String, Vec<String>), String>;

impl Tool for Screenshot {
    fn name(&self) -> &'static str {
        "screenshot"
    }

    fn description(&self) -> String {
        "Take a screenshot and look at it. Give exactly one of: url (a web page, \
         including a local dev server like http://localhost:5173 or a workspace \
         HTML file), window (part of a window's title or app id), or screen: true. \
         For a url: width and height set the viewport (default 1280x800), \
         color_scheme is light or dark, full_page captures the whole page as \
         viewport-sized tiles, wait_ms waits after load for animations. Use it \
         to check UI work you did: layout, spacing, contrast, both color schemes."
            .into()
    }

    fn terse_description(&self) -> String {
        "Screenshot a url, a window, or the screen, and see it.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "page to render: http(s)://, file://, or a workspace-relative .html path"},
                "window": {"type": "string", "description": "part of a window's title or app id"},
                "screen": {"type": "boolean", "description": "the whole screen"},
                "width": {"type": "integer", "description": "url only: viewport width, default 1280"},
                "height": {"type": "integer", "description": "url only: viewport height, default 800"},
                "color_scheme": {"type": "string", "enum": ["light", "dark"], "description": "url only: prefers-color-scheme"},
                "full_page": {"type": "boolean", "description": "url only: the whole page, as tiles"},
                "wait_ms": {"type": "integer", "description": "url only: extra wait after load"}
            }
        })
    }

    /// A capture can show anything on screen, or reach any URL: it
    /// asks, like `bash`, wherever the mode asks before acting.
    fn asks(&self) -> bool {
        true
    }

    /// The auto mode's pass: a page in the workspace, or a dev server
    /// on this machine, shows nothing the project doesn't. A window,
    /// the screen, or a remote URL still asks.
    fn contained(&self, arguments: &str) -> bool {
        let Ok(args) = serde_json::from_str::<ShotArgs>(arguments) else {
            return false;
        };
        let Some(url) = args
            .url
            .filter(|_| args.window.is_none() && args.screen != Some(true))
        else {
            return false;
        };
        match page_url(&self.root, &url) {
            Ok((_, Some(_))) => true,
            Ok((url, None)) => is_loopback(&url),
            Err(_) => false,
        }
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move { self.shoot(arguments).await.map(|(text, _)| text) })
    }

    fn run_with_images<'a>(
        &'a self,
        call: &'a ToolCall,
        _progress: &'a WorkerProgress,
        _turn_token: Option<gertie_core::TurnToken>,
    ) -> Pin<Box<dyn Future<Output = (ToolResult, Vec<String>)> + Send + 'a>> {
        Box::pin(async move {
            match self.shoot(&call.arguments).await {
                Ok((text, images)) => (Ok(text), images),
                Err(error) => (Err(error), Vec::new()),
            }
        })
    }
}

impl Screenshot {
    async fn shoot(&self, arguments: &str) -> Shot {
        let root = self.root.clone();
        let arguments = arguments.to_string();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = sender.send(capture(&root, &arguments));
        });
        receiver
            .await
            .unwrap_or_else(|_| Err("screenshot thread panicked".into()))
    }
}

fn capture(root: &Path, arguments: &str) -> Shot {
    let args: ShotArgs = if arguments.trim().is_empty() {
        ShotArgs::default()
    } else {
        serde_json::from_str(arguments)
            .map_err(|error| format!("screenshot: bad arguments ({error})"))?
    };
    let targets = [
        args.url.is_some(),
        args.window.is_some(),
        args.screen == Some(true),
    ]
    .iter()
    .filter(|set| **set)
    .count();
    if targets != 1 {
        return Err("screenshot: give exactly one of url, window, or screen: true".into());
    }
    let dir = shots_dir()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let base = dir.join(format!("shot-{stamp}"));
    if let Some(url) = &args.url {
        let (url, folder) = page_url(root, url)?;
        return capture_url(&url, folder.as_deref(), &args, &base);
    }
    let path = base.with_extension("png");
    let (what, note) = match &args.window {
        Some(pattern) => (
            format!("window matching \"{pattern}\""),
            capture_window(pattern, &path)?,
        ),
        None => ("the screen".to_string(), capture_screen(&path)?),
    };
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("screenshot: the capture wrote nothing ({error})"))?;
    let fitted = crate::images::fit(&bytes)?;
    let mut text = format!(
        "Screenshot of {what} ({}), saved to {}.",
        fitted.describe(),
        path.display()
    );
    if let Some(note) = note {
        text.push(' ');
        text.push_str(&note);
    }
    Ok((text, vec![fitted.data_url]))
}

/// Where captures are kept: `$XDG_CACHE_HOME/gertie/screenshots`, out
/// of every workspace, since a capture is evidence, not project work.
/// Created on every capture, so the age sweep rides it: files older
/// than 30 days go, keeping a chatty screenshot habit from growing
/// the cache without bound.
fn shots_dir() -> Result<PathBuf, String> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or("screenshot: no cache directory (HOME is unset)")?;
    let dir = cache.join("gertie").join("screenshots");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("screenshot: can't create {} ({error})", dir.display()))?;
    sweep_stale(&dir);
    Ok(dir)
}

/// Deletes capture files older than 30 days, best effort: a sweep
/// failure never blocks a capture. Runs once per capture; the dir is
/// small, so the scan is cheap next to a browser launch.
fn sweep_stale(dir: &Path) {
    let cutoff = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(now) => now.as_secs() - 30 * 24 * 60 * 60,
        Err(_) => return,
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.path().is_file() {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|secs| secs.as_secs() < cutoff);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A URL the browser can open, and the folder a file page lives in.
/// http(s) passes as it is. A workspace path, or a file URL, must
/// resolve inside the workspace, the same containment the read tools
/// keep: a capture of `file:///etc/...` would read what `read_file`
/// refuses.
fn page_url(root: &Path, url: &str) -> Result<(String, Option<PathBuf>), String> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok((url.to_string(), None));
    }
    let path = if lower.starts_with("file://") {
        &url["file://".len()..]
    } else if lower.contains("://")
        || lower.starts_with("javascript:")
        || lower.starts_with("data:")
    {
        return Err(format!(
            "screenshot: {url} isn't a page gertie renders (http, https, or a workspace file)"
        ));
    } else {
        url
    };
    let resolved = crate::resolve(root, path)?;
    Ok((
        format!("file://{}", file_url_path(&resolved)),
        Some(root.to_path_buf()),
    ))
}

/// The path as a file URL's path component: percent-encoded per RFC
/// 3986. Unreserved bytes and `/` pass (the separator keeps the path's
/// structure); anything else, space and `#` and `?` and quotes and
/// non-ASCII included, goes hex, so `my page.html` survives the trip
/// through the browser's argv and URL parser.
fn file_url_path(path: &Path) -> String {
    let raw = path.as_os_str().to_string_lossy();
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Whether an http(s) URL's host is this machine: localhost, a
/// 127.x address, or ::1. The authority is cut at the first `/`, `?`
/// or `#` and any userinfo dropped, so `http://localhost@evil.example`
/// is evil.example.
fn is_loopback(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    }
    .to_ascii_lowercase();
    host == "localhost"
        || host == "::1"
        || host.strip_prefix("127.").is_some_and(|tail| {
            tail.split('.').count() == 3 && tail.split('.').all(|part| part.parse::<u8>().is_ok())
        })
}

// ---------------------------------------------------------------- url

/// The command that starts a Chromium: `$GERTIE_BROWSER` when set, a
/// Chromium-family browser on PATH, else one installed as a Flatpak.
fn browser() -> Option<Vec<String>> {
    if let Some(browser) = std::env::var_os("GERTIE_BROWSER").filter(|value| !value.is_empty()) {
        return Some(vec![browser.to_string_lossy().into_owned()]);
    }
    for name in [
        "chromium",
        "chromium-browser",
        "google-chrome-stable",
        "google-chrome",
        "brave-browser",
        "microsoft-edge-stable",
        "microsoft-edge",
    ] {
        if let Some(path) = on_path(name) {
            return Some(vec![path.to_string_lossy().into_owned()]);
        }
    }
    on_path("flatpak")?;
    for app in [
        "org.chromium.Chromium",
        "com.google.Chrome",
        "com.brave.Browser",
        "com.microsoft.Edge",
    ] {
        let installed = Command::new("flatpak")
            .args(["info", app])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if installed {
            return Some(vec!["flatpak".into(), "run".into(), app.into()]);
        }
    }
    None
}

fn on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

fn capture_url(url: &str, folder: Option<&Path>, args: &ShotArgs, base: &Path) -> Shot {
    let mut command = browser().ok_or(
        "screenshot: no Chromium-family browser found for rendering a url. Install \
         chromium (or Chrome, Brave, Edge), natively or as a Flatpak, or set \
         GERTIE_BROWSER to its path",
    )?;
    let width = args.width.unwrap_or(1280).clamp(200, 3840);
    let height = args.height.unwrap_or(800).clamp(200, 2400);
    let scheme = match args.color_scheme.as_deref() {
        None => None,
        Some("light") => Some("light"),
        Some("dark") => Some("dark"),
        Some(other) => {
            return Err(format!(
                "screenshot: color_scheme is light or dark, not {other}"
            ));
        }
    };
    // A Flatpak browser sees no host files: a file page gets its
    // folder, read-only, and nothing else.
    if command[0] == "flatpak"
        && let Some(folder) = folder
    {
        command.insert(2, format!("--filesystem={}:ro", folder.display()));
    }
    let mut browser = Browser::start(&command)?;
    let tiles = browser.render(url, width, height, scheme, args)?;
    browser.close();

    let mut images = Vec::new();
    let mut saved = Vec::new();
    let mut shown = String::new();
    for (index, png) in tiles.pages.iter().enumerate() {
        let path = if tiles.pages.len() == 1 {
            base.with_extension("png")
        } else {
            PathBuf::from(format!("{}-{}.png", base.display(), index + 1))
        };
        std::fs::write(&path, png)
            .map_err(|error| format!("screenshot: can't save {} ({error})", path.display()))?;
        let fitted = crate::images::fit(png)?;
        if index == 0 {
            shown = fitted.describe();
        }
        images.push(fitted.data_url);
        saved.push(path.display().to_string());
    }
    let scheme = scheme
        .map(|scheme| format!(", {scheme}"))
        .unwrap_or_default();
    let mut text = if saved.len() == 1 {
        format!(
            "Screenshot of {url} ({shown}{scheme}), saved to {}.",
            saved[0]
        )
    } else {
        format!(
            "Full-page screenshot of {url} ({width}px wide, {}px tall{scheme}) in {} tiles, \
             top to bottom, saved to {}.",
            tiles.page_height,
            saved.len(),
            saved.join(", ")
        )
    };
    if tiles.truncated {
        text.push_str(&format!(
            " The page is taller than {MAX_TILES} tiles; the rest isn't shown."
        ));
    }
    Ok((text, images))
}

/// What a render produced: one PNG, or the tiles of a full page.
struct Tiles {
    pages: Vec<Vec<u8>>,
    page_height: u32,
    truncated: bool,
}

/// A headless Chromium on a DevTools pipe: commands go in on the
/// child's fd 3, answers and events come out of its fd 4, each a JSON
/// object ended by a NUL byte.
struct Browser {
    child: std::process::Child,
    commands: std::fs::File,
    answers: std::io::BufReader<std::fs::File>,
    next_id: u64,
    events: Vec<serde_json::Value>,
    /// Kills the browser when the capture runs out of time, which ends
    /// any read blocked on its pipe.
    watchdog: Option<std::sync::mpsc::Sender<()>>,
    profile: Option<PathBuf>,
}

impl Browser {
    fn start(command: &[String]) -> Result<Self, String> {
        use std::os::fd::FromRawFd as _;
        use std::os::unix::process::CommandExt as _;
        let pipe = || -> Result<(i32, i32), String> {
            let mut fds = [0; 2];
            // SAFETY: pipe2 fills the two-element array it is given.
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                return Err(format!(
                    "screenshot: can't open a pipe ({})",
                    std::io::Error::last_os_error()
                ));
            }
            Ok((fds[0], fds[1]))
        };
        let (child_reads, we_write) = pipe()?;
        let (we_read, child_writes) = pipe()?;
        // A native browser gets a throwaway profile, so it never meets
        // a running one; a Flatpak's private /tmp gives it that anyway.
        let profile = (command[0] != "flatpak").then(|| {
            std::env::temp_dir().join(format!(
                "gertie-browser-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or_default()
            ))
        });
        let mut process = Command::new(&command[0]);
        process
            .args(&command[1..])
            .args([
                "--headless",
                "--remote-debugging-pipe",
                "--disable-gpu",
                "--hide-scrollbars",
                "--mute-audio",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(profile) = &profile {
            process.arg(format!("--user-data-dir={}", profile.display()));
        }
        process.arg("about:blank");
        // SAFETY: only async-signal-safe calls between fork and exec.
        // The pipe ends are first copied above fd 10, so placing them
        // on 3 and 4 can't overwrite one with the other, and the
        // copies dup2 makes are not close-on-exec.
        unsafe {
            process.pre_exec(move || {
                let reads = libc::fcntl(child_reads, libc::F_DUPFD, 10);
                let writes = libc::fcntl(child_writes, libc::F_DUPFD, 10);
                if reads < 0 || writes < 0 || libc::dup2(reads, 3) < 0 || libc::dup2(writes, 4) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let spawned = process.spawn();
        // SAFETY: the child's ends are ours to close once it has them.
        unsafe {
            libc::close(child_reads);
            libc::close(child_writes);
        }
        let child = match spawned {
            Ok(child) => child,
            Err(error) => {
                // SAFETY: our ends, never handed to anything else.
                unsafe {
                    libc::close(we_write);
                    libc::close(we_read);
                }
                return Err(format!(
                    "screenshot: can't start {} ({error})",
                    command.join(" ")
                ));
            }
        };
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let pid = child.id() as libc::pid_t;
        std::thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                stopped.recv_timeout(CAPTURE_TIMEOUT)
            {
                // SAFETY: a plain signal to the child we started.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        });
        // SAFETY: both fds are open and owned by nothing else.
        let (commands, answers) = unsafe {
            (
                std::fs::File::from_raw_fd(we_write),
                std::fs::File::from_raw_fd(we_read),
            )
        };
        Ok(Self {
            child,
            commands,
            answers: std::io::BufReader::new(answers),
            next_id: 0,
            events: Vec::new(),
            watchdog: Some(stop),
            profile,
        })
    }

    /// One message off the pipe.
    fn read_message(&mut self) -> Result<serde_json::Value, String> {
        let mut buffer = Vec::new();
        let read = self
            .answers
            .read_until(0, &mut buffer)
            .map_err(|error| format!("screenshot: the browser pipe failed ({error})"))?;
        if read == 0 || buffer.last() != Some(&0) {
            return Err(format!(
                "screenshot: the browser stopped answering (it exited, or the capture took \
                 longer than {}s)",
                CAPTURE_TIMEOUT.as_secs()
            ));
        }
        buffer.pop();
        serde_json::from_slice(&buffer)
            .map_err(|error| format!("screenshot: the browser sent garbage ({error})"))
    }

    /// Sends a command and waits for its answer, keeping the events
    /// that arrive meanwhile.
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
        session: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = serde_json::json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = serde_json::json!(session);
        }
        let mut bytes = serde_json::to_vec(&message).map_err(|error| error.to_string())?;
        bytes.push(0);
        self.commands
            .write_all(&bytes)
            .map_err(|error| format!("screenshot: the browser pipe failed ({error})"))?;
        loop {
            let message = self.read_message()?;
            if message["id"].as_u64() == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(format!(
                        "screenshot: {method} failed ({})",
                        error["message"].as_str().unwrap_or("no reason given")
                    ));
                }
                return Ok(message["result"].clone());
            }
            if message.get("method").is_some() {
                self.events.push(message);
            }
        }
    }

    /// Waits for an event, from those already seen or still to come.
    fn wait_for(&mut self, method: &str, session: &str) -> Result<(), String> {
        let matches = |event: &serde_json::Value| {
            event["method"] == method && event["sessionId"].as_str() == Some(session)
        };
        if let Some(index) = self.events.iter().position(matches) {
            self.events.remove(index);
            return Ok(());
        }
        loop {
            let message = self.read_message()?;
            if matches(&message) {
                return Ok(());
            }
        }
    }

    fn render(
        &mut self,
        url: &str,
        width: u32,
        height: u32,
        scheme: Option<&str>,
        args: &ShotArgs,
    ) -> Result<Tiles, String> {
        let target = self.call(
            "Target.createTarget",
            serde_json::json!({ "url": "about:blank" }),
            None,
        )?;
        let target_id = target["targetId"]
            .as_str()
            .ok_or("screenshot: the browser opened no page")?
            .to_string();
        let attached = self.call(
            "Target.attachToTarget",
            serde_json::json!({ "targetId": target_id, "flatten": true }),
            None,
        )?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or("screenshot: the browser gave no session")?
            .to_string();
        let session = session.as_str();
        self.call("Page.enable", serde_json::json!({}), Some(session))?;
        self.call(
            "Emulation.setDeviceMetricsOverride",
            serde_json::json!({
                "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false,
            }),
            Some(session),
        )?;
        if let Some(scheme) = scheme {
            self.call(
                "Emulation.setEmulatedMedia",
                serde_json::json!({
                    "features": [{ "name": "prefers-color-scheme", "value": scheme }],
                }),
                Some(session),
            )?;
        }
        let navigated = self.call(
            "Page.navigate",
            serde_json::json!({ "url": url }),
            Some(session),
        )?;
        if let Some(error) = navigated["errorText"]
            .as_str()
            .filter(|error| !error.is_empty())
        {
            return Err(format!("screenshot: {url} didn't load ({error})"));
        }
        self.wait_for("Page.loadEventFired", session)?;
        // Fonts settle and two frames paint before the capture, so a
        // web font isn't caught mid-swap.
        self.call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "document.fonts.ready.then(() => new Promise(done => \
                    requestAnimationFrame(() => requestAnimationFrame(done))))",
                "awaitPromise": true,
            }),
            Some(session),
        )?;
        if let Some(wait) = args.wait_ms.filter(|wait| *wait > 0) {
            std::thread::sleep(Duration::from_millis(wait.min(10_000)));
        }
        let shoot = |browser: &mut Self, clip: Option<(u32, u32)>| -> Result<Vec<u8>, String> {
            let mut params = serde_json::json!({ "format": "png" });
            if let Some((top, tall)) = clip {
                params["clip"] = serde_json::json!({
                    "x": 0, "y": top, "width": width, "height": tall, "scale": 1,
                });
                params["captureBeyondViewport"] = serde_json::json!(true);
            }
            let shot = browser.call("Page.captureScreenshot", params, Some(session))?;
            let data = shot["data"]
                .as_str()
                .ok_or("screenshot: the browser returned no image")?;
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|error| format!("screenshot: the image didn't decode ({error})"))
        };
        if args.full_page != Some(true) {
            return Ok(Tiles {
                pages: vec![shoot(self, None)?],
                page_height: height,
                truncated: false,
            });
        }
        let metrics = self.call(
            "Page.getLayoutMetrics",
            serde_json::json!({}),
            Some(session),
        )?;
        let page_height = metrics["cssContentSize"]["height"]
            .as_f64()
            .or_else(|| metrics["contentSize"]["height"].as_f64())
            .map(|tall| tall.ceil() as u32)
            .unwrap_or(height)
            .max(1);
        let shown = page_height.min(height * MAX_TILES);
        let mut pages = Vec::new();
        let mut top = 0;
        while top < shown {
            let tall = height.min(shown - top);
            pages.push(shoot(self, Some((top, tall)))?);
            top += tall;
        }
        Ok(Tiles {
            pages,
            page_height,
            truncated: page_height > shown,
        })
    }

    /// Asks the browser to quit, then makes sure it did.
    fn close(mut self) {
        let _ = self.call("Browser.close", serde_json::json!({}), None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // A render that failed partway leaves the browser running:
        // kill it, then stop the watchdog and drop the profile.
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.watchdog.take();
        if let Some(profile) = self.profile.take() {
            let _ = std::fs::remove_dir_all(profile);
        }
    }
}

// ------------------------------------------------------ window, screen

fn run_quiet(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("screenshot: can't run {program} ({error})"))
        .and_then(|output| {
            if output.status.success() {
                Ok(output)
            } else {
                Err(format!(
                    "screenshot: {program} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        })
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// The whole screen. Returns a note for the model when the backend
/// did something it should know about.
fn capture_screen(path: &Path) -> Result<Option<String>, String> {
    let out = path.to_string_lossy().into_owned();
    if env_set("WAYLAND_DISPLAY") {
        if on_path("grim").is_some()
            && (env_set("NIRI_SOCKET")
                || env_set("SWAYSOCK")
                || env_set("HYPRLAND_INSTANCE_SIGNATURE")
                || !desktop_is(&["gnome", "kde"]))
        {
            run_quiet("grim", &[&out])?;
            return Ok(None);
        }
        if on_path("gnome-screenshot").is_some() {
            run_quiet("gnome-screenshot", &["-f", &out])?;
            return Ok(None);
        }
        if on_path("spectacle").is_some() {
            run_quiet("spectacle", &["-b", "-n", "-f", "-o", &out])?;
            return Ok(None);
        }
        return Err(no_backend());
    }
    if env_set("DISPLAY") && on_path("import").is_some() {
        run_quiet("import", &["-window", "root", &out])?;
        return Ok(None);
    }
    Err(no_backend())
}

fn desktop_is(names: &[&str]) -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|desktop| desktop.to_ascii_lowercase())
        .is_ok_and(|desktop| names.iter().any(|name| desktop.contains(name)))
}

fn no_backend() -> String {
    "screenshot: no screen capture backend here. On a wlroots compositor (niri, Sway, \
     Hyprland) install grim; on GNOME, gnome-screenshot; on KDE, spectacle; on X11, \
     ImageMagick (import). A url needs no screen access: render it instead"
        .into()
}

/// One window, picked by part of its title or app id.
fn capture_window(pattern: &str, path: &Path) -> Result<Option<String>, String> {
    let needle = pattern.to_lowercase();
    let out = path.to_string_lossy().into_owned();
    if env_set("NIRI_SOCKET") {
        let listed = run_quiet("niri", &["msg", "--json", "windows"])?;
        let windows: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout)
            .map_err(|error| format!("screenshot: niri's window list didn't parse ({error})"))?;
        let found = windows
            .iter()
            .filter(|window| window_matches(window, &needle, &["title", "app_id"]))
            .max_by_key(|window| window["focus_timestamp"]["secs"].as_u64().unwrap_or(0))
            .ok_or_else(|| no_window(pattern, &windows, &["title", "app_id"]))?;
        let id = found["id"]
            .as_u64()
            .ok_or("screenshot: niri listed a window without an id")?
            .to_string();
        run_quiet(
            "niri",
            &[
                "msg",
                "action",
                "screenshot-window",
                "--id",
                &id,
                "--path",
                &out,
                "--write-to-disk",
                "true",
            ],
        )?;
        // niri writes the file after the command returns.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() || std::fs::metadata(path).is_ok_and(|meta| meta.len() == 0) {
            if Instant::now() > deadline {
                return Err("screenshot: niri didn't write the window capture".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(100));
        return Ok(Some(
            "(niri also put it on the clipboard; it has no way not to.)".into(),
        ));
    }
    if env_set("HYPRLAND_INSTANCE_SIGNATURE") {
        let listed = run_quiet("hyprctl", &["clients", "-j"])?;
        let windows: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout)
            .map_err(|error| format!("screenshot: hyprctl's list didn't parse ({error})"))?;
        let found = windows
            .iter()
            .find(|window| window_matches(window, &needle, &["title", "class"]))
            .ok_or_else(|| no_window(pattern, &windows, &["title", "class"]))?;
        let geometry = format!(
            "{},{} {}x{}",
            found["at"][0], found["at"][1], found["size"][0], found["size"][1]
        );
        run_quiet("grim", &["-g", &geometry, &out])?;
        return Ok(Some(
            "(Only the part of the window on screen is captured.)".into(),
        ));
    }
    if env_set("SWAYSOCK") {
        let tree = run_quiet("swaymsg", &["-t", "get_tree"])?;
        let tree: serde_json::Value = serde_json::from_slice(&tree.stdout)
            .map_err(|error| format!("screenshot: sway's tree didn't parse ({error})"))?;
        let mut windows = Vec::new();
        sway_windows(&tree, &mut windows);
        let found = windows
            .iter()
            .find(|window| window_matches(window, &needle, &["name", "app_id"]))
            .ok_or_else(|| no_window(pattern, &windows, &["name", "app_id"]))?;
        let rect = &found["rect"];
        let geometry = format!(
            "{},{} {}x{}",
            rect["x"], rect["y"], rect["width"], rect["height"]
        );
        run_quiet("grim", &["-g", &geometry, &out])?;
        return Ok(Some(
            "(Only the part of the window on screen is captured.)".into(),
        ));
    }
    if !env_set("WAYLAND_DISPLAY")
        && env_set("DISPLAY")
        && on_path("xdotool").is_some()
        && on_path("import").is_some()
    {
        let found = Command::new("xdotool")
            .args(["search", "--name", pattern])
            .output()
            .map_err(|error| format!("screenshot: can't run xdotool ({error})"))?;
        let id = String::from_utf8_lossy(&found.stdout)
            .lines()
            .next()
            .map(str::to_string)
            .ok_or_else(|| format!("screenshot: no window's title matches \"{pattern}\""))?;
        run_quiet("import", &["-window", &id, &out])?;
        return Ok(None);
    }
    Err(
        "screenshot: capturing one window needs niri, Hyprland, or Sway (with grim), or \
         X11 with xdotool and ImageMagick. screen: true may still work here, and a url \
         needs neither"
            .into(),
    )
}

fn window_matches(window: &serde_json::Value, needle: &str, fields: &[&str]) -> bool {
    fields.iter().any(|field| {
        window[*field]
            .as_str()
            .is_some_and(|value| value.to_lowercase().contains(needle))
    })
}

/// The refusal that lists what is open, so the model can try again.
fn no_window(pattern: &str, windows: &[serde_json::Value], fields: &[&str]) -> String {
    let open: Vec<String> = windows
        .iter()
        .take(20)
        .map(|window| {
            fields
                .iter()
                .filter_map(|field| window[*field].as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .collect();
    format!(
        "screenshot: no window matches \"{pattern}\". Open windows: {}",
        if open.is_empty() {
            "none".to_string()
        } else {
            open.join("; ")
        }
    )
}

fn sway_windows(node: &serde_json::Value, found: &mut Vec<serde_json::Value>) {
    let is_window = node["type"] == "con" || node["type"] == "floating_con";
    if is_window && node["pid"].is_number() && node["visible"] == true {
        found.push(node.clone());
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            for child in children {
                sway_windows(child, found);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_target() {
        let root = std::env::temp_dir();
        for arguments in [
            "{}",
            r#"{"url":"http://localhost","screen":true}"#,
            r#"{"window":"a","url":"http://b"}"#,
        ] {
            let refused = capture(&root, arguments).err().unwrap();
            assert!(refused.contains("exactly one"), "{arguments}: {refused}");
        }
    }

    #[test]
    fn page_urls_stay_in_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("index.html"), "<p>hi</p>").unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let inside = format!("file://{}", file_url_path(&root.join("index.html")));
        assert_eq!(
            page_url(&root, "index.html").unwrap(),
            (inside.clone(), Some(root.clone()))
        );
        assert_eq!(page_url(&root, &inside).unwrap().0, inside);
        assert_eq!(
            page_url(&root, "http://localhost:5173").unwrap(),
            ("http://localhost:5173".to_string(), None)
        );
        assert!(page_url(&root, "../outside.html").is_err());
        assert!(page_url(&root, "file:///etc/passwd").is_err());
        assert!(page_url(&root, "javascript:alert(1)").is_err());
        assert!(page_url(&root, "chrome://settings").is_err());
    }

    #[test]
    fn file_urls_percent_encode_path_specials() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        std::fs::write(root.join("my page.html"), "<p>hi</p>").unwrap();
        let (url, _) = page_url(&root, "my page.html").unwrap();
        assert_eq!(
            url,
            format!("file://{}", file_url_path(&root.join("my page.html")))
        );
        assert!(url.contains("my%20page.html"), "{url}");
        assert!(!url.contains(' '));
    }

    #[test]
    fn only_local_pages_are_contained() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("index.html"), "<p>hi</p>").unwrap();
        let tool = Screenshot::new(&workspace.path().canonicalize().unwrap());
        for local in [
            r#"{"url":"index.html"}"#,
            r#"{"url":"http://localhost:5173/app"}"#,
            r#"{"url":"http://127.0.0.1:8080"}"#,
            r#"{"url":"https://[::1]:3000/"}"#,
        ] {
            assert!(tool.contained(local), "{local}");
        }
        for asks in [
            r#"{"screen":true}"#,
            r#"{"window":"Firefox"}"#,
            r#"{"url":"https://example.com"}"#,
            r#"{"url":"http://localhost@evil.example/"}"#,
            r#"{"url":"http://localhost.evil.example/"}"#,
            r#"{"url":"http://127.0.0.1.evil.example/"}"#,
            r#"{"url":"../outside.html"}"#,
            r#"{"url":"index.html","screen":true}"#,
            "not json",
        ] {
            assert!(!tool.contained(asks), "{asks}");
        }
    }

    #[test]
    fn a_missing_window_lists_the_open_ones() {
        let windows = vec![
            serde_json::json!({"title": "Gertie", "app_id": "dev.gertie"}),
            serde_json::json!({"title": "Notes", "app_id": "notes"}),
        ];
        assert!(window_matches(&windows[0], "gertie", &["title", "app_id"]));
        let refused = no_window("zed", &windows, &["title", "app_id"]);
        assert!(
            refused.contains("Gertie / dev.gertie; Notes / notes"),
            "{refused}"
        );
    }

    /// Live: renders a local page light and dark, full page, and grabs
    /// a window. Needs a Chromium and a compositor, so it runs by hand:
    /// `cargo test -p gertie-tools screenshot -- --ignored`.
    #[test]
    #[ignore]
    fn live_capture() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("page.html"),
            "<style>body{margin:0;background:#fff}@media (prefers-color-scheme: dark)\
             {body{background:#123456}}</style><h1>hi</h1><div style=height:2000px></div>",
        )
        .unwrap();
        let root = workspace.path().canonicalize().unwrap();
        for arguments in [
            r#"{"url":"page.html","width":600,"height":400}"#,
            r#"{"url":"page.html","width":600,"height":400,"color_scheme":"dark"}"#,
            r#"{"url":"page.html","width":600,"height":400,"full_page":true}"#,
        ] {
            let (text, images) = capture(&root, arguments).unwrap();
            println!("{arguments}\n  {text}");
            assert!(!images.is_empty());
        }
        if let Ok(window) = std::env::var("GERTIE_SHOT_WINDOW") {
            let arguments = serde_json::json!({ "window": window }).to_string();
            let (text, images) = capture(&root, &arguments).unwrap();
            println!("{arguments}\n  {text}");
            assert_eq!(images.len(), 1);
        }
    }
}
