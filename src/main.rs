mod api;
mod app;
mod config;
mod crypto;
mod discord_rpc;
mod mpris;
mod player;
mod ui;

use std::{
    fs,
    io,
    io::Write,
    path::Path,
    path::PathBuf,
    process::{Child, ChildStdin, Command as ProcessCommand, Stdio},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use anyhow::{anyhow, Context, Result as AnyResult};
use base64::Engine;
use app::{ActivePanel, App, Command, NowPlaying, RepeatMode, SearchCategory, UiEvent};
use config::{AudioQuality, Config, Theme};
use discord_rpc::{DiscordPresence, DiscordRpcHandle};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};
use rand::seq::SliceRandom;
use rand::thread_rng;
use serde_json::json;
use mpris_server::{LoopStatus, PlaybackStatus, Server};
use tokio::sync::{mpsc, oneshot};

#[cfg(unix)]
struct StderrSilenceGuard {
    original_stderr_fd: i32,
}

#[cfg(unix)]
impl StderrSilenceGuard {
    fn activate() -> AnyResult<Self> {
        let devnull = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .context("failed to open /dev/null")?;

        let original_stderr_fd = unsafe { libc::dup(libc::STDERR_FILENO) };
        if original_stderr_fd < 0 {
            return Err(anyhow!("failed to dup stderr"));
        }

        let rc = unsafe { libc::dup2(devnull.as_raw_fd(), libc::STDERR_FILENO) };
        if rc < 0 {
            unsafe {
                libc::close(original_stderr_fd);
            }
            return Err(anyhow!("failed to redirect stderr to /dev/null"));
        }

        Ok(Self { original_stderr_fd })
    }
}

#[cfg(unix)]
impl Drop for StderrSilenceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::dup2(self.original_stderr_fd, libc::STDERR_FILENO);
            let _ = libc::close(self.original_stderr_fd);
        }
    }
}

#[cfg(not(unix))]
struct StderrSilenceGuard;

#[cfg(not(unix))]
impl StderrSilenceGuard {
    fn activate() -> AnyResult<Self> {
        Ok(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageProtocol {
    None,
    Kitty,
    Ueberzugpp,
}

fn detect_image_protocol() -> ImageProtocol {
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let kitty_window = std::env::var("KITTY_WINDOW_ID").ok();

    if kitty_window.is_some()
        || term.contains("xterm-kitty")
        || term_program.contains("wezterm")
        || term_program.contains("ghostty")
    {
        ImageProtocol::Kitty
    } else if ProcessCommand::new("ueberzugpp")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
    {
        ImageProtocol::Ueberzugpp
    } else {
        ImageProtocol::None
    }
}

struct UeberzugppLayer {
    child: Child,
    stdin: ChildStdin,
    identifier: String,
}

impl UeberzugppLayer {
    fn start() -> Option<Self> {
        let mut child = ProcessCommand::new("ueberzugpp")
            .arg("layer")
            .arg("--silent")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        Some(Self {
            child,
            stdin,
            identifier: "deezer-tui-cover".to_string(),
        })
    }

    fn add(&mut self, path: &Path, area: ratatui::layout::Rect) -> io::Result<()> {
        if area.width == 0 || area.height == 0 {
            return Ok(());
        }
        let payload = json!({
            "action": "add",
            "identifier": self.identifier,
            "path": path.display().to_string(),
            "x": area.x,
            "y": area.y,
            "width": area.width,
            "height": area.height,
            "scaler": "fit_contain"
        });
        writeln!(self.stdin, "{}", payload)?;
        self.stdin.flush()
    }

    fn remove(&mut self) -> io::Result<()> {
        let payload = json!({
            "action": "remove",
            "identifier": self.identifier
        });
        writeln!(self.stdin, "{}", payload)?;
        self.stdin.flush()
    }

    fn shutdown(mut self) {
        let _ = self.remove();
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_cover_png_temp(png: &[u8]) -> io::Result<PathBuf> {
    let path = std::env::temp_dir().join("deezer-tui-cover.png");
    fs::write(&path, png)?;
    Ok(path)
}

fn kitty_delete_image(image_id: u32) -> io::Result<()> {
    let mut out = io::stdout();
    write!(out, "\x1b_Ga=d,d=I,i={}\x1b\\", image_id)?;
    out.flush()
}

fn kitty_draw_png_at_rect(png: &[u8], image_id: u32, area: ratatui::layout::Rect) -> io::Result<()> {
    if area.width == 0 || area.height == 0 {
        return Ok(());
    }

    let payload = base64::engine::general_purpose::STANDARD.encode(png);
    let mut out = io::stdout();

    // Save/restore cursor to avoid disturbing TUI input focus.
    write!(out, "\x1b[s\x1b[{};{}H", area.y + 1, area.x + 1)?;

    let mut idx = 0usize;
    let chunk = 4096usize;
    let mut first = true;
    while idx < payload.len() {
        let end = (idx + chunk).min(payload.len());
        let part = &payload[idx..end];
        let more = if end < payload.len() { 1 } else { 0 };
        if first {
            write!(
                out,
                "\x1b_Ga=T,f=100,i={},c={},r={},m={};{}\x1b\\",
                image_id,
                area.width,
                area.height,
                more,
                part
            )?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={};{}\x1b\\", more, part)?;
        }
        idx = end;
    }

    write!(out, "\x1b[u")?;
    out.flush()
}

struct MprisHandle {
    server: Server<mpris::MprisPlayer>,
    event_rx: crossbeam_channel::Receiver<mpris::MprisEvent>,
    last_update: Instant,
}

async fn create_mpris_handle() -> Option<MprisHandle> {
    let (server, event_rx) = mpris::create_server().await?;
    Some(MprisHandle { server, event_rx, last_update: Instant::now() })
}

#[derive(Debug, Clone, Copy)]
enum PlayerControl {
    Pause,
    Resume,
    Stop,
    FadeOutStop(u64),
    SetVolume(f32),
}

fn next_quality(current: AudioQuality, forward: bool) -> AudioQuality {
    match (current, forward) {
        (AudioQuality::Kbps128, true) => AudioQuality::Kbps320,
        (AudioQuality::Kbps320, true) => AudioQuality::Flac,
        (AudioQuality::Flac, true) => AudioQuality::Kbps128,
        (AudioQuality::Kbps128, false) => AudioQuality::Flac,
        (AudioQuality::Kbps320, false) => AudioQuality::Kbps128,
        (AudioQuality::Flac, false) => AudioQuality::Kbps320,
    }
}

fn sync_discord_presence(discord_rpc: &DiscordRpcHandle, app: &App) {
    if !app.discord_rpc_enabled {
        discord_rpc.clear();
        return;
    }

    if let Some(now) = app.now_playing.as_ref() {
        discord_rpc.update(DiscordPresence {
            title: now.title.clone(),
            artist: now.artist.clone(),
            track_id: now.id.clone(),
            album_art_url: now.album_art_url.clone(),
            current_ms: now.current_ms,
            total_ms: now.total_ms,
            is_playing: app.is_playing,
        });
    } else {
        discord_rpc.clear();
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ensure_verified_arl().await?;

    // Suppress noisy native audio warnings (e.g. ALSA underruns) while TUI is active.
    let _stderr_silence = StderrSilenceGuard::activate()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (command_sender, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<UiEvent>();

    let config = load_config();
    let initial_quality = config.default_quality;
    let initial_crossfade_enabled = config.crossfade_enabled;
    let initial_crossfade_duration_ms = config.crossfade_duration_ms;
    let mut app = App::new(config, command_sender);
    let discord_rpc = DiscordRpcHandle::new();

    // Spawn task to fetch user playlists on startup
    let playlists_event_tx = event_tx.clone();
    tokio::spawn(async move {
        use api::DeezerClient;
        
        let arl = match load_saved_arl() {
            Ok(arl) => arl,
            Err(err) => {
                let _ = playlists_event_tx.send(UiEvent::Error(format!("ARL Error: {}", err)));
                return;
            }
        };

        let _ = playlists_event_tx.send(UiEvent::Error("Status: Initializing...".into()));

        match DeezerClient::new(arl) {
            Ok(mut client) => {
                match client.fetch_api_token().await {
                    Ok(_) => {
                        let _ = playlists_event_tx.send(UiEvent::Error("Status: Auth success, fetching playlists...".into()));
                        
                        if let Some(user_id) = client.user_id() {
                            match client.fetch_user_playlists(user_id).await {
                                Ok(playlists) => {
                                    let _ = playlists_event_tx.send(UiEvent::PlaylistsLoaded(playlists));
                                }
                                Err(err) => {
                                    let _ = playlists_event_tx.send(UiEvent::Error(format!("API Error fetching playlists: {}", err)));
                                }
                            }
                        } else {
                            let _ = playlists_event_tx.send(UiEvent::Error("API Error: No user ID".into()));
                        }
                    }
                    Err(err) => {
                        let _ = playlists_event_tx.send(UiEvent::Error(format!("Auth Error: {}", err)));
                    }
                }
            }
            Err(err) => {
                let _ = playlists_event_tx.send(UiEvent::Error(format!("Client Error: {}", err)));
            }
        }
    });

    let mut audio_task = Some(tokio::spawn(audio_worker_loop(command_rx, event_tx)));
    // Sync worker's current_quality with the loaded config
    let _ = app.command_sender.send(Command::SetQuality(initial_quality));
    let _ = app.command_sender.send(Command::SetCrossfade {
        enabled: initial_crossfade_enabled,
        duration_ms: initial_crossfade_duration_ms,
    });
    let mut mpris_handle = create_mpris_handle().await;
    let run_result = run_tui_loop(
        &mut terminal,
        &mut app,
        &mut event_rx,
        &mut audio_task,
        &mut mpris_handle,
        &discord_rpc,
    ).await;

    drop(app);
    let restore_result = restore_terminal(&mut terminal);

    let join_result = if let Some(handle) = audio_task.take() {
        Some(handle.await)
    } else {
        None
    };

    discord_rpc.shutdown();

    if let Err(err) = restore_result {
        return Err(err.into());
    }

    if let Some(result) = join_result {
        if let Err(join_err) = result {
            return Err(anyhow!("background audio task failed: {join_err}").into());
        }
    }

    if let Err(err) = run_result {
        return Err(err.into());
    }

    Ok(())
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    event_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    audio_task: &mut Option<tokio::task::JoinHandle<()>>,
    mpris: &mut Option<MprisHandle>,
    discord_rpc: &DiscordRpcHandle,
) -> AnyResult<()> {
    let mut last_tick = Instant::now();
    let mut last_rpc_refresh = Instant::now();
    let mut image_protocol = detect_image_protocol();
    let mut ueberzugpp_layer = match image_protocol {
        ImageProtocol::Ueberzugpp => UeberzugppLayer::start(),
        _ => None,
    };
    if matches!(image_protocol, ImageProtocol::Ueberzugpp) && ueberzugpp_layer.is_none() {
        image_protocol = ImageProtocol::None;
    }
    let use_true_image_protocol = image_protocol != ImageProtocol::None;
    let kitty_image_id: u32 = 1337;
    let mut last_drawn_cover_sig: Option<(String, u16, u16, u16, u16)> = None;
    let mut force_full_redraw = true;
    // Channel for background cover-art downloads.
    let (cover_tx, mut cover_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();

    'ui: loop {
        let elapsed = last_tick.elapsed();
        last_tick = Instant::now();

        if app.is_playing {
            if let Some(now) = app.now_playing.as_mut() {
                now.current_ms = (now.current_ms + elapsed.as_millis() as u64).min(now.total_ms);
            }
        }

        // Heartbeat refresh: re-publish Discord presence periodically in case
        // Discord clears it unexpectedly during seek/crossfade transitions.
        if last_rpc_refresh.elapsed() >= Duration::from_secs(5) {
            sync_discord_presence(discord_rpc, app);
            last_rpc_refresh = Instant::now();
        }

        if app.is_playing
            && app.config.crossfade_enabled
            && app.config.crossfade_duration_ms > 0
            && !app.auto_transition_armed
        {
            if let (Some(now), Some(current_idx)) = (app.now_playing.as_ref(), app.queue_index) {
                if !app.queue_tracks.is_empty() {
                    let next_idx = if current_idx + 1 < app.queue_tracks.len() {
                        Some(current_idx + 1)
                    } else if app.repeat_mode == RepeatMode::All {
                        Some(0)
                    } else {
                        None
                    };

                    if let Some(next_idx) = next_idx {
                        let remaining_ms = now.total_ms.saturating_sub(now.current_ms);
                        if remaining_ms <= app.config.crossfade_duration_ms {
                            if let Some((next_track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                app.command_sender
                                    .send(Command::AutoPlayTrack(next_track_id.clone()))
                                    .map_err(|_| anyhow!("failed to send auto crossfade track command"))?;
                                app.queue_index = Some(next_idx);
                                app.queue_state.select(Some(next_idx));
                                app.auto_transition_armed = true;
                                app.status_message = format!(
                                    "Crossfading to queue item {}/{}",
                                    next_idx + 1,
                                    app.queue_tracks.len()
                                );
                            }
                        }
                    }
                }
            }
        }

        // Process MPRIS media control events from the OS/desktop environment
        if let Some(mpris_handle) = mpris.as_mut() {
            while let Ok(event) = mpris_handle.event_rx.try_recv() {
                match event {
                    mpris::MprisEvent::Play => {
                        if app.now_playing.is_some() && !app.is_playing {
                            app.command_sender
                                .send(Command::Resume)
                                .map_err(|_| anyhow!("MPRIS: failed to resume"))?;
                        }
                    }
                    mpris::MprisEvent::Pause => {
                        if app.is_playing {
                            app.command_sender
                                .send(Command::Pause)
                                .map_err(|_| anyhow!("MPRIS: failed to pause"))?;
                        }
                    }
                    mpris::MprisEvent::Toggle => {
                        if app.is_playing {
                            app.command_sender
                                .send(Command::Pause)
                                .map_err(|_| anyhow!("MPRIS: failed to toggle pause"))?;
                        } else if app.now_playing.is_some() {
                            app.command_sender
                                .send(Command::Resume)
                                .map_err(|_| anyhow!("MPRIS: failed to toggle resume"))?;
                        }
                    }
                    mpris::MprisEvent::Next => {
                        if let Some(current_idx) = app.queue_index {
                            let next_idx = current_idx + 1;
                            if next_idx < app.queue_tracks.len() {
                                app.queue_index = Some(next_idx);
                                app.queue_state.select(Some(next_idx));
                                if let Some((track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                    app.command_sender
                                        .send(Command::PlayTrack(track_id.clone()))
                                        .map_err(|_| anyhow!("MPRIS: failed to play next"))?;
                                    app.is_playing = true;
                                }
                            }
                        }
                    }
                    mpris::MprisEvent::Previous => {
                        if let Some(current_idx) = app.queue_index {
                            if current_idx > 0 {
                                let prev_idx = current_idx - 1;
                                app.queue_index = Some(prev_idx);
                                app.queue_state.select(Some(prev_idx));
                                if let Some((track_id, _, _)) = app.queue_tracks.get(prev_idx) {
                                    app.command_sender
                                        .send(Command::PlayTrack(track_id.clone()))
                                        .map_err(|_| anyhow!("MPRIS: failed to play previous"))?;
                                    app.is_playing = true;
                                }
                            }
                        }
                    }
                    mpris::MprisEvent::SeekBy(offset_us) => {
                        if let Some(now) = app.now_playing.as_ref() {
                            if now.quality != AudioQuality::Flac {
                                let current_us = now.current_ms as i64 * 1_000;
                                let new_us = (current_us + offset_us)
                                    .max(0)
                                    .min((now.total_ms as i64 - 1) * 1_000);
                                let seek_ms = (new_us / 1_000) as u64;
                                app.command_sender
                                    .send(Command::PlayTrackAt {
                                        track_id: now.id.clone(),
                                        quality: now.quality,
                                        seek_ms,
                                    })
                                    .map_err(|_| anyhow!("MPRIS: failed to seek"))?;
                            }
                        }
                    }
                    mpris::MprisEvent::SetPosition(pos_us) => {
                        if let Some(now) = app.now_playing.as_ref() {
                            if now.quality != AudioQuality::Flac {
                                let seek_ms =
                                    ((pos_us.max(0) / 1_000) as u64).min(now.total_ms.saturating_sub(1));
                                app.command_sender
                                    .send(Command::PlayTrackAt {
                                        track_id: now.id.clone(),
                                        quality: now.quality,
                                        seek_ms,
                                    })
                                    .map_err(|_| anyhow!("MPRIS: failed to set position"))?;
                            }
                        }
                    }
                    mpris::MprisEvent::SetShuffle(shuffle) => {
                        if shuffle && !app.queue_tracks.is_empty() {
                            let current_id = app
                                .queue_index
                                .and_then(|i| app.queue_tracks.get(i))
                                .map(|t| t.0.clone());
                            app.queue_tracks.shuffle(&mut thread_rng());
                            if let Some(current) = current_id {
                                app.queue_index =
                                    app.queue_tracks.iter().position(|t| t.0 == current);
                            }
                            app.queue = app
                                .queue_tracks
                                .iter()
                                .map(|(_, title, artist)| format!("{} - {}", title, artist))
                                .collect();
                            if let Some(i) = app.queue_index {
                                app.queue_state.select(Some(i));
                            }
                            app.status_message = "Queue shuffled".into();
                        }
                    }
                    mpris::MprisEvent::SetLoopStatus(loop_status) => {
                        app.repeat_mode = match loop_status {
                            LoopStatus::None => RepeatMode::Off,
                            LoopStatus::Track => RepeatMode::One,
                            LoopStatus::Playlist => RepeatMode::All,
                        };
                        app.status_message = format!("Repeat: {:?}", app.repeat_mode);
                    }
                    mpris::MprisEvent::SetVolume(v) => {
                        app.volume = (v * 100.0).round().clamp(0.0, 100.0) as u16;
                        app.command_sender
                            .send(Command::SetVolume(app.volume))
                            .map_err(|_| anyhow!("MPRIS: failed to set volume"))?;
                    }
                }
            }

            // Rate-limited position update (every ~1s) — no signal needed,
            // D-Bus clients poll Position directly.
            if mpris_handle.last_update.elapsed() >= Duration::from_secs(1) {
                if let Some(now) = app.now_playing.as_ref() {
                    mpris::update_position(&mpris_handle.server, now.current_ms);
                }
                mpris_handle.last_update = Instant::now();
            }
        }

        if let Some(handle) = audio_task.as_ref() {
            if handle.is_finished() {
                let finished = audio_task
                    .take()
                    .ok_or_else(|| anyhow!("audio task missing after completion check"))?;

                if let Err(join_err) = finished.await {
                    return Err(anyhow!("background audio task panicked: {join_err}"));
                }
            }
        }

        // Receive any completed cover-art downloads and decode them.
        while let Ok((track_id, bytes)) = cover_rx.try_recv() {
            if app.now_playing.as_ref().map(|n| n.id.as_str()) == Some(track_id.as_str()) {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    let mut png = Vec::new();
                    let mut cursor = io::Cursor::new(&mut png);
                    if img.write_to(&mut cursor, image::ImageFormat::Png).is_ok() {
                        app.cover_art_png = Some(png);
                    } else {
                        app.cover_art_png = None;
                    }
                    app.cover_art = Some(img);
                    app.cover_art_track_id = Some(track_id);
                }
            }
        }

        if force_full_redraw {
            terminal.clear()?;
            force_full_redraw = false;
        }
        let mut protocol_art_rect = None;
        terminal.draw(|f| {
            protocol_art_rect = ui::render(f, app, use_true_image_protocol);
        })?;

        if use_true_image_protocol {
            if let (Some(rect), Some(track_id), Some(png)) = (
                protocol_art_rect,
                app.cover_art_track_id.as_ref(),
                app.cover_art_png.as_ref(),
            ) {
                let sig = (track_id.clone(), rect.x, rect.y, rect.width, rect.height);
                if last_drawn_cover_sig.as_ref() != Some(&sig) {
                    match image_protocol {
                        ImageProtocol::Kitty => {
                            let _ = kitty_draw_png_at_rect(png, kitty_image_id, rect);
                        }
                        ImageProtocol::Ueberzugpp => {
                            if let (Some(layer), Ok(path)) =
                                (ueberzugpp_layer.as_mut(), write_cover_png_temp(png))
                            {
                                let _ = layer.add(&path, rect);
                            }
                        }
                        ImageProtocol::None => {}
                    }
                    last_drawn_cover_sig = Some(sig);
                }
            } else if last_drawn_cover_sig.is_some() {
                match image_protocol {
                    ImageProtocol::Kitty => {
                        let _ = kitty_delete_image(kitty_image_id);
                    }
                    ImageProtocol::Ueberzugpp => {
                        if let Some(layer) = ueberzugpp_layer.as_mut() {
                            let _ = layer.remove();
                        }
                    }
                    ImageProtocol::None => {}
                }
                last_drawn_cover_sig = None;
            }
        }

        let mut processed_input = false;
        while event::poll(if processed_input {
            Duration::from_millis(0)
        } else {
            Duration::from_millis(50)
        })? {
            processed_input = true;
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    match key.code {
                        KeyCode::Char('q') => {
                            discord_rpc.clear();
                            let _ = app.command_sender.send(Command::Shutdown);
                            break 'ui;
                        }
                        KeyCode::Char(c) if app.is_searching || app.active_panel == ActivePanel::Search => {
                            app.search_query.push(c);
                        }
                        KeyCode::Backspace if app.is_searching || app.active_panel == ActivePanel::Search => {
                            app.search_query.pop();
                        }
                        KeyCode::Esc if app.is_searching || app.active_panel == ActivePanel::Search => {
                            app.is_searching = false;
                            app.active_panel = ActivePanel::Main;
                        }
                        KeyCode::Enter if app.is_searching || app.active_panel == ActivePanel::Search => {
                            app.is_searching = false;
                            app.active_panel = ActivePanel::Main;
                            app.main_state.select(Some(0));
                            app.command_sender
                                .send(Command::Search(app.search_query.clone()))
                                .map_err(|_| anyhow!("failed to send search command"))?;
                            app.status_message = "Searching...".into();
                        }
                        KeyCode::Char('/') if !app.is_searching => {
                            app.is_searching = true;
                            app.active_panel = ActivePanel::Search;
                            app.search_query.clear();
                        }
                        KeyCode::Char('t') if !app.is_searching => {
                            app.config.theme = match app.config.theme {
                                Theme::SpotifyDark => Theme::NcmpcppBlue,
                                Theme::NcmpcppBlue => Theme::SpotifyDark,
                            };
                        }
                        KeyCode::Char('p') if !app.is_searching => {
                            if app.now_playing.is_none() {
                                app.command_sender
                                    .send(Command::PlayTrack("3135556".to_string()))
                                    .map_err(|_| anyhow!("failed to send play command to audio worker"))?;
                                app.is_playing = true;
                            } else if app.is_playing {
                                app.command_sender
                                    .send(Command::Pause)
                                    .map_err(|_| anyhow!("failed to send pause command to audio worker"))?;
                            } else {
                                app.command_sender
                                    .send(Command::Resume)
                                    .map_err(|_| anyhow!("failed to send resume command to audio worker"))?;
                            }
                        }
                        KeyCode::Tab if !app.is_searching => {
                            app.active_panel = match app.active_panel {
                                ActivePanel::Navigation => ActivePanel::Playlists,
                                ActivePanel::Playlists => ActivePanel::Queue,
                                ActivePanel::Queue => ActivePanel::Search,
                                ActivePanel::Search => ActivePanel::Main,
                                ActivePanel::Main => ActivePanel::Player,
                                ActivePanel::PlayerProgress => ActivePanel::Navigation,
                                ActivePanel::Player => ActivePanel::Navigation,
                                ActivePanel::PlayerInfo => ActivePanel::Navigation,
                            };
                        }
                        KeyCode::Down | KeyCode::Char('j') if !app.is_searching => app.handle_down(),
                        KeyCode::Up | KeyCode::Char('k') if !app.is_searching => app.handle_up(),
                        KeyCode::Left if !app.is_searching => {
                            if app.active_panel == ActivePanel::PlayerProgress {
                                if let Some(now) = app.now_playing.as_ref() {
                                    if now.quality == AudioQuality::Flac {
                                        app.status_message = "FLAC seek is disabled".into();
                                        continue;
                                    }
                                    let seek_ms = now.current_ms.saturating_sub(5_000);
                                    app.command_sender
                                        .send(Command::PlayTrackAt {
                                            track_id: now.id.clone(),
                                            quality: now.quality,
                                            seek_ms,
                                        })
                                        .map_err(|_| anyhow!("failed to seek track"))?;
                                    if let Some(mpris_handle) = mpris.as_mut() {
                                        mpris::notify_seeked(&mpris_handle.server, seek_ms).await;
                                    }
                                    app.status_message = format!("Seek: {}s", seek_ms / 1000);
                                }
                            } else if app.active_panel == ActivePanel::PlayerInfo {
                                match app.player_info_index {
                                    0 => {
                                        app.volume = app.volume.saturating_sub(5);
                                        app.command_sender
                                            .send(Command::SetVolume(app.volume))
                                            .map_err(|_| anyhow!("failed to set volume"))?;
                                    }
                                    1 => {
                                        if let Some(now) = app.now_playing.as_ref() {
                                            let quality = next_quality(now.quality, false);
                                            let seek_ms = if quality == AudioQuality::Flac {
                                                0
                                            } else {
                                                now.current_ms
                                            };
                                            app.command_sender
                                                .send(Command::PlayTrackAt {
                                                    track_id: now.id.clone(),
                                                    quality,
                                                    seek_ms,
                                                })
                                                .map_err(|_| anyhow!("failed to switch quality"))?;
                                            app.status_message = format!("Quality: {:?}", quality);
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                app.handle_left();
                            }
                        }
                        KeyCode::Right if !app.is_searching => {
                            if app.active_panel == ActivePanel::Main && app.showing_search_results {
                                app.switch_search_category_right();
                            } else if app.active_panel == ActivePanel::PlayerProgress {
                                if let Some(now) = app.now_playing.as_ref() {
                                    if now.quality == AudioQuality::Flac {
                                        app.status_message = "FLAC seek is disabled".into();
                                        continue;
                                    }
                                    let seek_ms = (now.current_ms + 5_000).min(now.total_ms.saturating_sub(1));
                                    app.command_sender
                                        .send(Command::PlayTrackAt {
                                            track_id: now.id.clone(),
                                            quality: now.quality,
                                            seek_ms,
                                        })
                                        .map_err(|_| anyhow!("failed to seek track"))?;
                                    if let Some(mpris_handle) = mpris.as_mut() {
                                        mpris::notify_seeked(&mpris_handle.server, seek_ms).await;
                                    }
                                    app.status_message = format!("Seek: {}s", seek_ms / 1000);
                                }
                            } else if app.active_panel == ActivePanel::PlayerInfo {
                                match app.player_info_index {
                                    0 => {
                                        app.volume = (app.volume + 5).min(100);
                                        app.command_sender
                                            .send(Command::SetVolume(app.volume))
                                            .map_err(|_| anyhow!("failed to set volume"))?;
                                    }
                                    1 => {
                                        if let Some(now) = app.now_playing.as_ref() {
                                            let quality = next_quality(now.quality, true);
                                            let seek_ms = if quality == AudioQuality::Flac {
                                                0
                                            } else {
                                                now.current_ms
                                            };
                                            app.command_sender
                                                .send(Command::PlayTrackAt {
                                                    track_id: now.id.clone(),
                                                    quality,
                                                    seek_ms,
                                                })
                                                .map_err(|_| anyhow!("failed to switch quality"))?;
                                            app.status_message = format!("Quality: {:?}", quality);
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                app.handle_right();
                            }
                        }
                        KeyCode::Enter if !app.is_searching => {
                            match app.active_panel {
                                ActivePanel::Navigation => {
                                    let nav_idx = app.nav_state.selected().unwrap_or(0);
                                    match nav_idx {
                                        0 => {
                                            app.command_sender
                                                .send(Command::LoadHome)
                                                .map_err(|_| anyhow!("failed to send load home command"))?;
                                            app.current_playlist_id = Some("__home__".to_string());
                                            app.active_panel = ActivePanel::Main;
                                            app.status_message = "Loading Home recommendations...".into();
                                        }
                                        1 => {
                                            app.command_sender
                                                .send(Command::LoadExplore)
                                                .map_err(|_| anyhow!("failed to send load explore command"))?;
                                            app.current_playlist_id = Some("__explore__".to_string());
                                            app.active_panel = ActivePanel::Main;
                                            app.status_message = "Loading Explore recommendations...".into();
                                        }
                                        2 => {
                                            app.command_sender
                                                .send(Command::LoadFavorites)
                                                .map_err(|_| anyhow!("failed to send load favorites command"))?;
                                            app.active_panel = ActivePanel::Main;
                                            app.status_message = "Loading favorites...".into();
                                        }
                                        3 => {
                                            app.viewing_settings = true;
                                            app.active_panel = ActivePanel::Main;
                                        }
                                        _ => {}
                                    }
                                }
                                ActivePanel::Playlists => {
                                    if let Some(idx) = app.playlist_state.selected() {
                                        if idx < app.playlists.len() {
                                            let (playlist_id, _) = &app.playlists[idx];
                                            app.current_playlist_id = Some(playlist_id.clone());
                                            app.command_sender
                                                .send(Command::LoadPlaylist(playlist_id.clone()))
                                                .map_err(|_| anyhow!("failed to send load playlist command"))?;
                                            app.active_panel = ActivePanel::Main;
                                        }
                                    }
                                }
                                ActivePanel::Queue => {
                                    if let Some(idx) = app.queue_state.selected() {
                                        if idx < app.queue_tracks.len() {
                                            app.queue_index = Some(idx);
                                            let (track_id, _, _) = &app.queue_tracks[idx];
                                            app.command_sender
                                                .send(Command::PlayTrack(track_id.clone()))
                                                .map_err(|_| anyhow!("failed to play queued track"))?;
                                            app.is_playing = true;
                                        }
                                    }
                                }
                                ActivePanel::Main => {
                                    if app.viewing_settings {
                                        if let Some(idx) = app.settings_state.selected() {
                                            match idx {
                                                0 => {
                                                    app.config.crossfade_enabled = !app.config.crossfade_enabled;
                                                    app.command_sender
                                                        .send(Command::SetCrossfade {
                                                            enabled: app.config.crossfade_enabled,
                                                            duration_ms: app.config.crossfade_duration_ms,
                                                        })
                                                        .map_err(|_| anyhow!("failed to set crossfade"))?;
                                                    let _ = save_config(&app.config);
                                                }
                                                1 => {
                                                    let presets = [1000u64, 3000, 5000, 8000, 10000, 13000];
                                                    let current = app.config.crossfade_duration_ms;
                                                    let next = presets
                                                        .iter()
                                                        .copied()
                                                        .find(|value| *value > current)
                                                        .unwrap_or(presets[0]);
                                                    app.config.crossfade_duration_ms = next;
                                                    app.command_sender
                                                        .send(Command::SetCrossfade {
                                                            enabled: app.config.crossfade_enabled,
                                                            duration_ms: app.config.crossfade_duration_ms,
                                                        })
                                                        .map_err(|_| anyhow!("failed to set crossfade duration"))?;
                                                    let _ = save_config(&app.config);
                                                }
                                                2 => {
                                                    app.config.default_quality = match app.config.default_quality {
                                                        AudioQuality::Kbps128 => AudioQuality::Kbps320,
                                                        AudioQuality::Kbps320 => AudioQuality::Flac,
                                                        AudioQuality::Flac => AudioQuality::Kbps128,
                                                    };
                                                    app.command_sender
                                                        .send(Command::SetQuality(app.config.default_quality))
                                                        .map_err(|_| anyhow!("failed to set quality"))?;
                                                    let _ = save_config(&app.config);
                                                }
                                                3 => {
                                                    app.discord_rpc_enabled = !app.discord_rpc_enabled;
                                                    app.config.discord_rpc_enabled = app.discord_rpc_enabled;
                                                    let _ = save_config(&app.config);
                                                    sync_discord_presence(discord_rpc, app);
                                                }
                                                4 => {
                                                    let new_arl = app.search_query.trim();
                                                    if new_arl.is_empty() {
                                                        app.status_message = "Type new ARL in search box first".into();
                                                    } else if let Err(err) = save_arl(new_arl) {
                                                        app.status_message = format!("Failed to save ARL: {}", err);
                                                    } else {
                                                        app.status_message = "ARL updated".into();
                                                        app.search_query.clear();
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    } else if app.showing_search_results {
                                        if let Some(idx) = app.main_state.selected() {
                                            match app.search_category {
                                                SearchCategory::Tracks => {
                                                    if idx == 0 {
                                                        app.queue_tracks = app.current_tracks.clone();
                                                        app.queue = app
                                                            .queue_tracks
                                                            .iter()
                                                            .map(|(_, title, artist)| format!("{} - {}", title, artist))
                                                            .collect();
                                                        app.queue_state.select(Some(0));
                                                        app.queue_index = Some(0);

                                                        if let Some((track_id, _, _)) = app.queue_tracks.first() {
                                                            app.command_sender
                                                                .send(Command::PlayTrack(track_id.clone()))
                                                                .map_err(|_| anyhow!("failed to send play track command"))?;
                                                            app.is_playing = true;
                                                        }
                                                    } else {
                                                        let track_idx = idx - 1;
                                                        if track_idx < app.current_tracks.len() {
                                                            let selected = app.current_tracks[track_idx].clone();
                                                            app.queue_tracks = vec![selected.clone()];
                                                            app.queue = vec![format!("{} - {}", selected.1, selected.2)];
                                                            app.queue_state.select(Some(0));
                                                            app.queue_index = Some(0);

                                                            app.command_sender
                                                                .send(Command::PlayTrack(selected.0))
                                                                .map_err(|_| anyhow!("failed to send play track command"))?;
                                                            app.is_playing = true;
                                                        }
                                                    }
                                                }
                                                SearchCategory::Playlists => {
                                                    if idx < app.search_playlists.len() {
                                                        let (playlist_id, title) = &app.search_playlists[idx];
                                                        app.current_playlist_id = Some(playlist_id.clone());
                                                        app.command_sender
                                                            .send(Command::LoadPlaylist(playlist_id.clone()))
                                                            .map_err(|_| anyhow!("failed to load playlist from search"))?;
                                                        app.status_message = format!("Loading playlist: {}", title);
                                                    }
                                                }
                                                SearchCategory::Artists => {
                                                    if idx < app.search_artists.len() {
                                                        let (_, name) = &app.search_artists[idx];
                                                        app.command_sender
                                                            .send(Command::Search(name.clone()))
                                                            .map_err(|_| anyhow!("failed to search artist"))?;
                                                        app.status_message = format!("Searching artist: {}", name);
                                                    }
                                                }
                                            }
                                        }
                                    } else if !app.current_tracks.is_empty() {
                                        if let Some(idx) = app.main_state.selected() {
                                            if idx == 0 {
                                                app.queue_tracks = app.current_tracks.clone();
                                                app.queue = app
                                                    .queue_tracks
                                                    .iter()
                                                    .map(|(_, title, artist)| format!("{} - {}", title, artist))
                                                    .collect();
                                                app.queue_state.select(Some(0));
                                                app.queue_index = Some(0);

                                                if let Some((track_id, _, _)) = app.queue_tracks.first() {
                                                    app.command_sender
                                                        .send(Command::PlayTrack(track_id.clone()))
                                                        .map_err(|_| anyhow!("failed to send play track command"))?;
                                                    app.is_playing = true;
                                                }
                                            } else {
                                                let track_idx = idx - 1;
                                                if track_idx < app.current_tracks.len() {
                                                    let selected = app.current_tracks[track_idx].clone();
                                                    app.queue_tracks = vec![selected.clone()];
                                                    app.queue = vec![format!("{} - {}", selected.1, selected.2)];
                                                    app.queue_state.select(Some(0));
                                                    app.queue_index = Some(0);

                                                    app.command_sender
                                                        .send(Command::PlayTrack(selected.0))
                                                        .map_err(|_| anyhow!("failed to send play track command"))?;
                                                    app.is_playing = true;
                                                }
                                            }
                                        }
                                    }
                                }
                                ActivePanel::Player => {
                                    match app.player_button_index {
                                        0 => {
                                            if !app.queue_tracks.is_empty() {
                                                let current_id = app
                                                    .queue_index
                                                    .and_then(|i| app.queue_tracks.get(i))
                                                    .map(|t| t.0.clone());
                                                app.queue_tracks.shuffle(&mut thread_rng());
                                                if let Some(current) = current_id {
                                                    app.queue_index = app
                                                        .queue_tracks
                                                        .iter()
                                                        .position(|t| t.0 == current);
                                                }
                                                app.queue = app
                                                    .queue_tracks
                                                    .iter()
                                                    .map(|(_, title, artist)| format!("{} - {}", title, artist))
                                                    .collect();
                                                if let Some(i) = app.queue_index {
                                                    app.queue_state.select(Some(i));
                                                }
                                                app.status_message = "Queue shuffled".into();
                                            }
                                        }
                                        1 => {
                                            if let Some(current_idx) = app.queue_index {
                                                if current_idx > 0 {
                                                    let prev_idx = current_idx - 1;
                                                    app.queue_index = Some(prev_idx);
                                                    app.queue_state.select(Some(prev_idx));
                                                    if let Some((track_id, _, _)) = app.queue_tracks.get(prev_idx) {
                                                        app.command_sender
                                                            .send(Command::PlayTrack(track_id.clone()))
                                                            .map_err(|_| anyhow!("failed to play previous track"))?;
                                                        app.is_playing = true;
                                                    }
                                                }
                                            }
                                        }
                                        2 => {
                                            if app.is_playing {
                                                app.command_sender
                                                    .send(Command::Pause)
                                                    .map_err(|_| anyhow!("failed to pause"))?;
                                            } else {
                                                app.command_sender
                                                    .send(Command::Resume)
                                                    .map_err(|_| anyhow!("failed to resume"))?;
                                            }
                                        }
                                        3 => {
                                            if let Some(current_idx) = app.queue_index {
                                                let next_idx = current_idx + 1;
                                                if next_idx < app.queue_tracks.len() {
                                                    app.queue_index = Some(next_idx);
                                                    app.queue_state.select(Some(next_idx));
                                                    if let Some((track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                                        app.command_sender
                                                            .send(Command::PlayTrack(track_id.clone()))
                                                            .map_err(|_| anyhow!("failed to play next track"))?;
                                                        app.is_playing = true;
                                                    }
                                                }
                                            }
                                        }
                                        4 => {
                                            app.repeat_mode = match app.repeat_mode {
                                                RepeatMode::Off => RepeatMode::All,
                                                RepeatMode::All => RepeatMode::One,
                                                RepeatMode::One => RepeatMode::Off,
                                            };
                                            if let Some(mpris_handle) = mpris.as_mut() {
                                                let loop_status = match app.repeat_mode {
                                                    RepeatMode::Off => LoopStatus::None,
                                                    RepeatMode::One => LoopStatus::Track,
                                                    RepeatMode::All => LoopStatus::Playlist,
                                                };
                                                mpris::update_loop_and_shuffle(&mpris_handle.server, loop_status, false).await;
                                            }
                                            app.status_message = format!("Repeat mode: {:?}", app.repeat_mode);
                                        }
                                        _ => {}
                                    }
                                }
                                ActivePanel::Search => {
                                    app.command_sender
                                        .send(Command::Search(app.search_query.clone()))
                                        .map_err(|_| anyhow!("failed to send search command"))?;
                                    app.active_panel = ActivePanel::Main;
                                }
                                ActivePanel::PlayerProgress => {
                                    // Progress bar uses Left/Right for seek; Enter is a no-op.
                                }
                                ActivePanel::PlayerInfo => {}
                            }
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse_event) => handle_mouse_event(mouse_event),
                Event::Resize(_, _) => {
                    // Resize often cleans up stray lines, keep that behavior explicit.
                    force_full_redraw = true;
                }
                _ => {}
            }
        }

        while let Ok(event) = event_rx.try_recv() {
            match event {
                UiEvent::TrackChanged {
                    id,
                    title,
                    artist,
                    quality,
                    album_art_url,
                    initial_ms,
                } => {
                    // Clear stale cover art immediately; new art arrives asynchronously.
                    app.cover_art = None;
                    app.cover_art_png = None;
                    app.cover_art_track_id = None;
                    if let Some(ref url) = album_art_url {
                        let url = url.clone();
                        let tid = id.clone();
                        let tx = cover_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(resp) = reqwest::get(&url).await {
                                if let Ok(bytes) = resp.bytes().await {
                                    let _ = tx.send((tid, bytes.to_vec()));
                                }
                            }
                        });
                    }
                    if let Some(mpris_handle) = mpris.as_mut() {
                        let can_go_next = app.queue_index
                            .map(|i| i + 1 < app.queue_tracks.len())
                            .unwrap_or(false);
                        let can_go_previous = app.queue_index.unwrap_or(0) > 0;
                        let meta = mpris::build_metadata(&id, &title, &artist, album_art_url.as_deref(), 0);
                        mpris::set_track_metadata(&mpris_handle.server, meta, initial_ms, can_go_next, can_go_previous).await;
                        mpris_handle.last_update = Instant::now();
                    }
                    app.now_playing = Some(NowPlaying {
                        id,
                        title,
                        artist,
                        album_art_url,
                        quality,
                        current_ms: initial_ms,
                        total_ms: 224_000,
                    });
                    app.is_playing = true;
                    app.auto_transition_armed = false;
                    sync_discord_presence(discord_rpc, app);
                }
                UiEvent::PlaybackProgress {
                    current_ms,
                    total_ms,
                } => {
                    let prev_total = app.now_playing.as_ref().map(|n| n.total_ms).unwrap_or(0);
                    if let Some(now) = app.now_playing.as_mut() {
                        now.current_ms = current_ms;
                        now.total_ms = total_ms.max(1);
                    }
                    // On the first real total_ms, update the MPRIS metadata with proper duration
                    if prev_total == 224_000 && total_ms != 224_000 {
                        if let (Some(mpris_handle), Some(now)) = (mpris.as_mut(), app.now_playing.as_ref()) {
                            let meta = mpris::build_metadata(
                                &now.id, &now.title, &now.artist,
                                now.album_art_url.as_deref(), total_ms,
                            );
                            let can_go_next = app.queue_index.map(|i| i + 1 < app.queue_tracks.len()).unwrap_or(false);
                            let can_go_previous = app.queue_index.unwrap_or(0) > 0;
                            mpris::set_track_metadata(&mpris_handle.server, meta, current_ms, can_go_next, can_go_previous).await;
                        }
                        sync_discord_presence(discord_rpc, app);
                    }
                }
                UiEvent::PlaybackPaused => {
                    if let Some(mpris_handle) = mpris.as_mut() {
                        let pos_ms = app.now_playing.as_ref().map(|n| n.current_ms).unwrap_or(0);
                        mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Paused, pos_ms).await;
                    }
                    app.is_playing = false;
                    sync_discord_presence(discord_rpc, app);
                }
                UiEvent::PlaybackResumed => {
                    if let Some(mpris_handle) = mpris.as_mut() {
                        let pos_ms = app.now_playing.as_ref().map(|n| n.current_ms).unwrap_or(0);
                        mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Playing, pos_ms).await;
                    }
                    app.is_playing = true;
                    sync_discord_presence(discord_rpc, app);
                }
                UiEvent::PlaybackStopped => {
                    if let Some(current_idx) = app.queue_index {
                        let next_idx = current_idx + 1;
                        if next_idx < app.queue_tracks.len() {
                            app.queue_index = Some(next_idx);
                            app.queue_state.select(Some(next_idx));

                            if let Some((next_track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                app.command_sender
                                    .send(Command::AutoPlayTrack(next_track_id.clone()))
                                    .map_err(|_| anyhow!("failed to send next queued track command"))?;
                                app.is_playing = true;
                                app.status_message = format!(
                                    "Playing queue item {}/{}",
                                    next_idx + 1,
                                    app.queue_tracks.len()
                                );
                            }
                        } else if !app.queue_tracks.is_empty() {
                            match app.repeat_mode {
                                RepeatMode::One => {
                                    if let Some((track_id, _, _)) = app.queue_tracks.get(current_idx) {
                                        app.command_sender
                                            .send(Command::AutoPlayTrack(track_id.clone()))
                                            .map_err(|_| anyhow!("failed to repeat current track"))?;
                                        app.is_playing = true;
                                    }
                                }
                                RepeatMode::All => {
                                    app.queue_index = Some(0);
                                    app.queue_state.select(Some(0));
                                    if let Some((track_id, _, _)) = app.queue_tracks.first() {
                                        app.command_sender
                                            .send(Command::AutoPlayTrack(track_id.clone()))
                                            .map_err(|_| anyhow!("failed to repeat queue"))?;
                                        app.is_playing = true;
                                    }
                                }
                                RepeatMode::Off => {
                                    app.queue_index = None;
                                    app.is_playing = false;
                                    app.status_message = "Queue finished".into();
                                    if let Some(mpris_handle) = mpris.as_mut() {
                                        mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Stopped, 0).await;
                                    }
                                }
                            }
                        } else {
                            app.queue_index = None;
                            app.is_playing = false;
                            app.status_message = "Queue finished".into();
                            if let Some(mpris_handle) = mpris.as_mut() {
                                mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Stopped, 0).await;
                            }
                        }
                    } else {
                        app.is_playing = false;
                        if let Some(mpris_handle) = mpris.as_mut() {
                            mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Stopped, 0).await;
                        }
                    }
                    if !app.is_playing {
                        discord_rpc.clear();
                    }
                }
                UiEvent::Error(message) => {
                    let is_status_message = message.starts_with("Status:")
                        || message.contains("Loading")
                        || message.contains("Auth")
                        || message.contains("ARL")
                        || message.contains("Client error")
                        || message.contains("API Error")
                        || message.contains("Search");

                    if is_status_message {
                        app.status_message = message;
                        // Keep playback/RPC state intact for informational status updates.
                        continue;
                    }

                    let noisy_terminal_output = message.to_lowercase().contains("underrun")
                        || message.to_lowercase().contains("error.pcm")
                        || message.to_lowercase().contains("alsa");
                    if noisy_terminal_output {
                        // If audio stack printed junk over the TUI, hard redraw once.
                        force_full_redraw = true;
                    }

                    let looks_like_playback_failure = true;

                    if looks_like_playback_failure {
                        let failed_idx = app.queue_index;
                        // Skip failed track and move forward in queue when possible.
                        if let Some(current_idx) = failed_idx {
                            let mut next_idx = current_idx + 1;
                            if next_idx >= app.queue_tracks.len() {
                                if app.repeat_mode == RepeatMode::All && app.queue_tracks.len() > 1 {
                                    next_idx = 0;
                                } else {
                                    app.is_playing = false;
                                    app.status_message = format!("{} | Skipped failed track", message);
                                    if let Some(mpris_handle) = mpris.as_mut() {
                                        mpris::set_playback_status(
                                            &mpris_handle.server,
                                            PlaybackStatus::Stopped,
                                            0,
                                        )
                                        .await;
                                    }
                                    discord_rpc.clear();
                                    continue;
                                }
                            }

                            app.queue_index = Some(next_idx);
                            app.queue_state.select(Some(next_idx));
                            if let Some((track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                app.command_sender
                                    .send(Command::AutoPlayTrack(track_id.clone()))
                                    .map_err(|_| anyhow!("failed to skip to next track"))?;
                                app.is_playing = true;
                                app.status_message = format!(
                                    "{} | Skipped to {}/{}",
                                    message,
                                    next_idx + 1,
                                    app.queue_tracks.len()
                                );
                                continue;
                            }
                        }
                    }

                    app.status_message = message;
                    app.is_playing = false;
                    if let Some(mpris_handle) = mpris.as_mut() {
                        mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Stopped, 0)
                            .await;
                    }
                    discord_rpc.clear();
                }
                UiEvent::PlaylistsLoaded(playlists) => {
                    app.playlists = playlists;
                    app.status_message = "Playlists loaded!".into();
                    if !app.playlists.is_empty() {
                        app.playlist_state.select(Some(0));
                    }
                }
                UiEvent::TracksLoaded(tracks) => {
                    app.current_tracks = tracks;
                    app.showing_search_results = false;
                    app.search_playlists.clear();
                    app.search_artists.clear();
                    if app.current_tracks.is_empty() {
                        app.status_message = "No tracks found for this playlist/search".into();
                    } else {
                        app.status_message = format!("Loaded {} tracks", app.current_tracks.len());
                    }
                    app.main_state.select(Some(0));
                    app.viewing_settings = false;
                    app.active_panel = ActivePanel::Main;
                }
                UiEvent::SearchResultsLoaded {
                    tracks,
                    playlists,
                    artists,
                } => {
                    app.current_tracks = tracks;
                    app.search_playlists = playlists;
                    app.search_artists = artists;
                    app.showing_search_results = true;
                    app.search_category = SearchCategory::Tracks;
                    app.main_state.select(Some(0));
                    app.viewing_settings = false;
                    app.active_panel = ActivePanel::Main;
                    app.status_message = format!(
                        "Search: {} tracks, {} playlists, {} artists",
                        app.current_tracks.len(),
                        app.search_playlists.len(),
                        app.search_artists.len()
                    );
                }
            }
        }
    }

    if use_true_image_protocol {
        match image_protocol {
            ImageProtocol::Kitty => {
                let _ = kitty_delete_image(kitty_image_id);
            }
            ImageProtocol::Ueberzugpp => {
                if let Some(layer) = ueberzugpp_layer.take() {
                    layer.shutdown();
                }
            }
            ImageProtocol::None => {}
        }
    }

    Ok(())
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> AnyResult<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
        .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to restore cursor")?;
    Ok(())
}

fn handle_mouse_event(mouse_event: MouseEvent) {
    match mouse_event.kind {
        MouseEventKind::Down(_) => {}
        MouseEventKind::Up(_) => {}
        MouseEventKind::Drag(_) => {}
        MouseEventKind::Moved => {}
        MouseEventKind::ScrollDown => {}
        MouseEventKind::ScrollUp => {}
        MouseEventKind::ScrollLeft => {}
        MouseEventKind::ScrollRight => {}
    }
}

async fn audio_worker_loop(
    mut command_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<UiEvent>,
) {
    let mut active_controls: Option<std::sync::mpsc::Sender<PlayerControl>> = None;
    let mut active_playback_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut current_volume: u16 = 100;
    let mut current_quality: AudioQuality = AudioQuality::Kbps320;
    let mut crossfade_enabled = false;
    let mut crossfade_duration_ms: u64 = 0;

    while let Some(cmd) = command_rx.recv().await {
        if let Some(handle) = active_playback_task.as_ref() {
            if handle.is_finished() {
                active_playback_task.take();
                active_controls = None;
            }
        }

        match cmd {
            Command::PlayTrack(track_id) => {
                if active_playback_task
                    .as_ref()
                    .map(|task| !task.is_finished())
                    .unwrap_or(false)
                {
                    if let Some(control_sender) = active_controls.as_ref() {
                        let _ = control_sender.send(PlayerControl::Stop);
                    }

                    if let Some(handle) = active_playback_task.take() {
                        let _ = handle.await;
                    }

                    active_controls = None;
                }

                let event_tx_for_task = event_tx.clone();
                let (controls_ready_tx, controls_ready_rx) =
                    oneshot::channel::<std::sync::mpsc::Sender<PlayerControl>>();

                let quality_for_task = current_quality;
                let handle = tokio::spawn(async move {
                    if let Err(err) = run_play_track_pipeline(
                        track_id,
                        quality_for_task,
                        0,
                        &event_tx_for_task,
                        controls_ready_tx,
                    ).await {
                        let _ = event_tx_for_task.send(UiEvent::Error(err.to_string()));
                    }
                });
                active_playback_task = Some(handle);

                if let Ok(control_sender) = controls_ready_rx.await {
                    let _ = control_sender.send(PlayerControl::SetVolume(current_volume as f32 / 100.0));
                    active_controls = Some(control_sender);
                }
            }
            Command::AutoPlayTrack(track_id) => {
                let should_crossfade = crossfade_enabled && crossfade_duration_ms > 0;
                if active_playback_task
                    .as_ref()
                    .map(|task| !task.is_finished())
                    .unwrap_or(false)
                {
                    if let Some(control_sender) = active_controls.as_ref() {
                        let _ = if should_crossfade {
                            control_sender.send(PlayerControl::FadeOutStop(crossfade_duration_ms))
                        } else {
                            control_sender.send(PlayerControl::Stop)
                        };
                    }

                    if let Some(handle) = active_playback_task.take() {
                        if should_crossfade {
                            tokio::spawn(async move {
                                let _ = handle.await;
                            });
                        } else {
                            let _ = handle.await;
                        }
                    }

                    active_controls = None;
                }

                let event_tx_for_task = event_tx.clone();
                let (controls_ready_tx, controls_ready_rx) =
                    oneshot::channel::<std::sync::mpsc::Sender<PlayerControl>>();

                let quality_for_task = current_quality;
                let handle = tokio::spawn(async move {
                    if let Err(err) = run_play_track_pipeline(
                        track_id,
                        quality_for_task,
                        0,
                        &event_tx_for_task,
                        controls_ready_tx,
                    ).await {
                        let _ = event_tx_for_task.send(UiEvent::Error(err.to_string()));
                    }
                });
                active_playback_task = Some(handle);

                if let Ok(control_sender) = controls_ready_rx.await {
                    let target_volume = current_volume as f32 / 100.0;
                    if should_crossfade {
                        let _ = control_sender.send(PlayerControl::SetVolume(0.0));
                        let fade_sender = control_sender.clone();
                        let step_count: u64 = 20;
                        let step_ms = (crossfade_duration_ms / step_count).max(1);
                        tokio::spawn(async move {
                            for step in 1..=step_count {
                                tokio::time::sleep(Duration::from_millis(step_ms)).await;
                                let next = target_volume * (step as f32 / step_count as f32);
                                if fade_sender.send(PlayerControl::SetVolume(next)).is_err() {
                                    break;
                                }
                            }
                        });
                    } else {
                        let _ = control_sender.send(PlayerControl::SetVolume(target_volume));
                    }
                    active_controls = Some(control_sender);
                }
            }
            Command::PlayTrackAt {
                track_id,
                quality,
                seek_ms,
            } => {
                if active_playback_task
                    .as_ref()
                    .map(|task| !task.is_finished())
                    .unwrap_or(false)
                {
                    if let Some(control_sender) = active_controls.as_ref() {
                        // PlayTrackAt is used for seek/quality jumps and should feel immediate.
                        let _ = control_sender.send(PlayerControl::Stop);
                    }

                    if let Some(handle) = active_playback_task.take() {
                        let _ = handle.await;
                    }

                    active_controls = None;
                }

                let event_tx_for_task = event_tx.clone();
                let (controls_ready_tx, controls_ready_rx) =
                    oneshot::channel::<std::sync::mpsc::Sender<PlayerControl>>();

                let handle = tokio::spawn(async move {
                    if let Err(err) = run_play_track_pipeline(
                        track_id,
                        quality,
                        seek_ms,
                        &event_tx_for_task,
                        controls_ready_tx,
                    )
                    .await
                    {
                        let _ = event_tx_for_task.send(UiEvent::Error(err.to_string()));
                    }
                });
                active_playback_task = Some(handle);

                if let Ok(control_sender) = controls_ready_rx.await {
                    let _ = control_sender.send(PlayerControl::SetVolume(current_volume as f32 / 100.0));
                    active_controls = Some(control_sender);
                }
            }
            Command::Pause => {
                if let Some(control_sender) = active_controls.as_ref() {
                    let _ = control_sender.send(PlayerControl::Pause);
                    let _ = event_tx.send(UiEvent::PlaybackPaused);
                }
            }
            Command::Resume => {
                if let Some(control_sender) = active_controls.as_ref() {
                    let _ = control_sender.send(PlayerControl::Resume);
                    let _ = event_tx.send(UiEvent::PlaybackResumed);
                }
            }
            Command::SetVolume(volume) => {
                current_volume = volume.min(100);
                if let Some(control_sender) = active_controls.as_ref() {
                    let _ = control_sender.send(PlayerControl::SetVolume(current_volume as f32 / 100.0));
                }
            }
            Command::LoadPlaylist(playlist_id) => {
                let event_tx_for_task = event_tx.clone();
                let _ = event_tx_for_task.send(UiEvent::Error("Status: Loading tracks...".into()));
                
                tokio::spawn(async move {
                    use api::DeezerClient;
                    
                    let arl = match load_saved_arl() {
                        Ok(arl) => arl,
                        Err(err) => {
                            let _ = event_tx_for_task.send(UiEvent::Error(format!("Failed to load ARL: {}", err)));
                            return;
                        }
                    };

                    match DeezerClient::new(arl) {
                        Ok(mut client) => {
                            match client.fetch_api_token().await {
                                Ok(_) => {
                                    match client.fetch_playlist_tracks(&playlist_id).await {
                                        Ok(tracks) => {
                                            let _ = event_tx_for_task.send(UiEvent::TracksLoaded(tracks));
                                        }
                                        Err(err) => {
                                            let _ = event_tx_for_task.send(UiEvent::Error(format!("Failed to load tracks: {}", err)));
                                        }
                                    }
                                }
                                Err(err) => {
                                    let _ = event_tx_for_task.send(UiEvent::Error(format!("Failed to fetch API token: {}", err)));
                                }
                            }
                        }
                        Err(err) => {
                            let _ = event_tx_for_task.send(UiEvent::Error(format!("Failed to create client: {}", err)));
                        }
                    }
                });
            }
            Command::LoadHome => {
                let event_tx_for_task = event_tx.clone();
                let _ = event_tx_for_task.send(UiEvent::Error("Status: Loading Home recommendations...".into()));

                tokio::spawn(async move {
                    use api::DeezerClient;

                    let arl = match load_saved_arl() {
                        Ok(arl) => arl,
                        Err(err) => {
                            let _ = event_tx_for_task
                                .send(UiEvent::Error(format!("Failed to load ARL: {}", err)));
                            return;
                        }
                    };

                    match DeezerClient::new(arl) {
                        Ok(mut client) => match client.fetch_api_token().await {
                            Ok(_) => match client.fetch_home_tracks().await {
                                Ok(tracks) => {
                                    let _ = event_tx_for_task.send(UiEvent::TracksLoaded(tracks));
                                }
                                Err(err) => {
                                    let err_text = err.to_string();
                                    let needs_token_retry = {
                                        let lower = err_text.to_lowercase();
                                        lower.contains("csrf") || lower.contains("token")
                                    };
                                    if needs_token_retry {
                                        match client.fetch_api_token().await {
                                            Ok(_) => match client.fetch_home_tracks().await {
                                                Ok(tracks) => {
                                                    let _ = event_tx_for_task
                                                        .send(UiEvent::TracksLoaded(tracks));
                                                }
                                                Err(retry_err) => {
                                                    let _ = event_tx_for_task.send(UiEvent::Error(format!(
                                                        "Home recommendations error: {}",
                                                        retry_err
                                                    )));
                                                }
                                            },
                                            Err(refresh_err) => {
                                                let _ = event_tx_for_task.send(UiEvent::Error(format!(
                                                    "Home auth refresh error: {}",
                                                    refresh_err
                                                )));
                                            }
                                        }
                                    } else {
                                        let _ = event_tx_for_task.send(UiEvent::Error(format!(
                                            "Home recommendations error: {}",
                                            err_text
                                        )));
                                    }
                                }
                            },
                            Err(err) => {
                                let _ = event_tx_for_task
                                    .send(UiEvent::Error(format!("Auth error: {}", err)));
                            }
                        },
                        Err(err) => {
                            let _ = event_tx_for_task
                                .send(UiEvent::Error(format!("Client error: {}", err)));
                        }
                    }
                });
            }
            Command::LoadExplore => {
                let event_tx_for_task = event_tx.clone();
                let _ = event_tx_for_task.send(UiEvent::Error("Status: Loading Explore recommendations...".into()));

                tokio::spawn(async move {
                    use api::DeezerClient;

                    let arl = match load_saved_arl() {
                        Ok(arl) => arl,
                        Err(err) => {
                            let _ = event_tx_for_task
                                .send(UiEvent::Error(format!("Failed to load ARL: {}", err)));
                            return;
                        }
                    };

                    match DeezerClient::new(arl) {
                        Ok(mut client) => match client.fetch_api_token().await {
                            Ok(_) => match client.fetch_explore_tracks().await {
                                Ok(tracks) => {
                                    let _ = event_tx_for_task.send(UiEvent::TracksLoaded(tracks));
                                }
                                Err(err) => {
                                    let err_text = err.to_string();
                                    let needs_token_retry = {
                                        let lower = err_text.to_lowercase();
                                        lower.contains("csrf") || lower.contains("token")
                                    };
                                    if needs_token_retry {
                                        match client.fetch_api_token().await {
                                            Ok(_) => match client.fetch_explore_tracks().await {
                                                Ok(tracks) => {
                                                    let _ = event_tx_for_task
                                                        .send(UiEvent::TracksLoaded(tracks));
                                                }
                                                Err(retry_err) => {
                                                    let _ = event_tx_for_task.send(UiEvent::Error(format!(
                                                        "Explore recommendations error: {}",
                                                        retry_err
                                                    )));
                                                }
                                            },
                                            Err(refresh_err) => {
                                                let _ = event_tx_for_task.send(UiEvent::Error(format!(
                                                    "Explore auth refresh error: {}",
                                                    refresh_err
                                                )));
                                            }
                                        }
                                    } else {
                                        let _ = event_tx_for_task.send(UiEvent::Error(format!(
                                            "Explore recommendations error: {}",
                                            err_text
                                        )));
                                    }
                                }
                            },
                            Err(err) => {
                                let _ = event_tx_for_task
                                    .send(UiEvent::Error(format!("Auth error: {}", err)));
                            }
                        },
                        Err(err) => {
                            let _ = event_tx_for_task
                                .send(UiEvent::Error(format!("Client error: {}", err)));
                        }
                    }
                });
            }
            Command::LoadFavorites => {
                let event_tx_for_task = event_tx.clone();
                let _ = event_tx_for_task.send(UiEvent::Error("Status: Loading favorites...".into()));

                tokio::spawn(async move {
                    use api::DeezerClient;

                    let arl = match load_saved_arl() {
                        Ok(arl) => arl,
                        Err(err) => {
                            let _ = event_tx_for_task
                                .send(UiEvent::Error(format!("Failed to load ARL: {}", err)));
                            return;
                        }
                    };

                    match DeezerClient::new(arl) {
                        Ok(mut client) => match client.fetch_api_token().await {
                            Ok(_) => match client.fetch_favorite_tracks().await {
                                Ok(tracks) => {
                                    let _ = event_tx_for_task.send(UiEvent::TracksLoaded(tracks));
                                }
                                Err(err) => {
                                    let _ = event_tx_for_task
                                        .send(UiEvent::Error(format!("Favorites error: {}", err)));
                                }
                            },
                            Err(err) => {
                                let _ = event_tx_for_task
                                    .send(UiEvent::Error(format!("Auth error: {}", err)));
                            }
                        },
                        Err(err) => {
                            let _ = event_tx_for_task
                                .send(UiEvent::Error(format!("Client error: {}", err)));
                        }
                    }
                });
            }
            Command::Search(query) => {
                let event_tx_for_task = event_tx.clone();
                let query_clone = query.clone();

                tokio::spawn(async move {
                    use api::DeezerClient;

                    let arl = match load_saved_arl() {
                        Ok(arl) => arl,
                        Err(err) => {
                            let _ = event_tx_for_task
                                .send(UiEvent::Error(format!("Failed to load ARL: {}", err)));
                            return;
                        }
                    };

                    match DeezerClient::new(arl) {
                        Ok(mut client) => match client.fetch_api_token().await {
                            Ok(_) => match client.fetch_search_results(&query_clone).await {
                                Ok((tracks, playlists, artists)) => {
                                    let _ = event_tx_for_task.send(UiEvent::SearchResultsLoaded {
                                        tracks,
                                        playlists,
                                        artists,
                                    });
                                }
                                Err(err) => {
                                    let _ = event_tx_for_task
                                        .send(UiEvent::Error(format!("Search error: {}", err)));
                                }
                            },
                            Err(err) => {
                                let _ = event_tx_for_task
                                    .send(UiEvent::Error(format!("Auth error: {}", err)));
                            }
                        },
                        Err(err) => {
                            let _ = event_tx_for_task
                                .send(UiEvent::Error(format!("Client error: {}", err)));
                        }
                    }
                });
            }
            Command::SetQuality(quality) => {
                current_quality = quality;
            }
            Command::SetCrossfade {
                enabled,
                duration_ms,
            } => {
                crossfade_enabled = enabled;
                crossfade_duration_ms = duration_ms;
            }
            Command::Shutdown => {
                if let Some(control_sender) = active_controls.as_ref() {
                    let _ = control_sender.send(PlayerControl::Stop);
                }
                if let Some(handle) = active_playback_task.take() {
                    let _ = handle.await;
                }
                active_controls = None;
                break;
            }
        }
    }

    // If command channel closes without an explicit shutdown command,
    // still stop and join the active playback so process exit is immediate.
    if let Some(control_sender) = active_controls.as_ref() {
        let _ = control_sender.send(PlayerControl::Stop);
    }
    if let Some(handle) = active_playback_task.take() {
        let _ = handle.await;
    }
}

async fn run_play_track_pipeline(
    track_id: String,
    quality: AudioQuality,
    seek_ms: u64,
    event_tx: &mpsc::UnboundedSender<UiEvent>,
    controls_ready_tx: oneshot::Sender<std::sync::mpsc::Sender<PlayerControl>>,
) -> AnyResult<()> {
    use api::DeezerClient;
    use crossbeam_channel::{unbounded, Receiver};
    use crypto::{decrypt_chunk_in_place_with_key, derive_blowfish_key};
    use player::StreamingPlayer;

    const DEEZER_CHUNK_SIZE: usize = 2048;
    const PREBUFFER_BYTES: usize = 512 * 1024;

    fn start_player_thread(
        player_thread: &mut Option<std::thread::JoinHandle<AnyResult<()>>>,
        receiver: &mut Option<Receiver<Vec<u8>>>,
        control_rx: &mut Option<std::sync::mpsc::Receiver<PlayerControl>>,
        event_tx: mpsc::UnboundedSender<UiEvent>,
    ) -> AnyResult<()> {
        if player_thread.is_some() {
            return Ok(());
        }

        let stream_receiver = receiver
            .take()
            .ok_or_else(|| anyhow!("stream receiver already consumed"))?;
        let control_rx = control_rx
            .take()
            .ok_or_else(|| anyhow!("playback control receiver already consumed"))?;

        let handle = std::thread::spawn(move || -> AnyResult<()> {
            let streaming_player = StreamingPlayer::new()?;
            streaming_player.stop();
            streaming_player.play_stream(stream_receiver)?;
            let mut interrupted = false;
            let mut volume = 1.0f32;

            loop {
                while let Ok(control) = control_rx.try_recv() {
                    match control {
                        PlayerControl::Pause => streaming_player.pause(),
                        PlayerControl::Resume => streaming_player.resume(),
                        PlayerControl::SetVolume(next_volume) => {
                            volume = next_volume;
                            streaming_player.set_volume(next_volume)
                        }
                        PlayerControl::FadeOutStop(duration_ms) => {
                            let steps: u64 = 20;
                            let step_ms = (duration_ms / steps).max(1);
                            for step in (0..steps).rev() {
                                let factor = step as f32 / steps as f32;
                                streaming_player.set_volume((volume * factor).clamp(0.0, 1.0));
                                std::thread::sleep(Duration::from_millis(step_ms));
                            }
                            streaming_player.stop();
                            interrupted = true;
                            break;
                        }
                        PlayerControl::Stop => {
                            streaming_player.stop();
                            interrupted = true;
                            break;
                        }
                    }
                }

                if interrupted {
                    break;
                }

                if streaming_player.is_empty() {
                    break;
                }

                std::thread::sleep(Duration::from_millis(100));
            }

            if !interrupted {
                let _ = event_tx.send(UiEvent::PlaybackStopped);
            }
            Ok(())
        });

        *player_thread = Some(handle);
        Ok(())
    }

    let arl = load_saved_arl()?;

    let mut client = DeezerClient::new(arl)?;
    client.fetch_api_token().await?;

    let metadata = client.fetch_track_metadata(&track_id).await?;

    let _ = event_tx.send(UiEvent::TrackChanged {
        id: metadata.id.clone(),
        title: metadata.title.clone(),
        artist: metadata.artist.clone(),
        quality,
        album_art_url: metadata.album_art_url.clone(),
        initial_ms: if quality == AudioQuality::Flac { 0 } else { seek_ms },
    });

    let signed_url = client.fetch_media_url(&metadata.track_token, quality).await?;

    let (control_tx, control_rx) = std::sync::mpsc::channel::<PlayerControl>();
    let _ = controls_ready_tx.send(control_tx);
    let mut control_rx = Some(control_rx);

    let (sender, receiver) = unbounded::<Vec<u8>>();
    let mut receiver = Some(receiver);
    let mut player_thread: Option<std::thread::JoinHandle<AnyResult<()>>> = None;
    let mut queued_bytes = 0usize;

    let mut response = client.open_signed_stream(&signed_url).await?;
    let content_length = response.content_length();
    let track_key = derive_blowfish_key(&metadata.id);
    let effective_seek_ms = if quality == AudioQuality::Flac {
        0
    } else {
        seek_ms
    };
    let track_duration_ms = metadata
        .duration_secs
        .map(|s| s * 1000)
        .unwrap_or_else(|| estimate_total_duration_ms(content_length, quality));
    let seek_ms = effective_seek_ms.min(track_duration_ms.saturating_sub(1));
    let seek_target_bytes = content_length
        .map(|total| total.saturating_mul(seek_ms) / track_duration_ms.max(1))
        .unwrap_or(0);
    let mut skipped_bytes: u64 = 0;

    let _ = event_tx.send(UiEvent::PlaybackProgress {
        current_ms: seek_ms,
        total_ms: track_duration_ms,
    });

    let mut pending = Vec::new();
    let mut chunk_index = 0usize;

    while let Some(network_chunk) = response.chunk().await? {
        pending.extend_from_slice(&network_chunk);

        while pending.len() >= DEEZER_CHUNK_SIZE {
            let mut chunk = pending.drain(..DEEZER_CHUNK_SIZE).collect::<Vec<u8>>();
            decrypt_chunk_in_place_with_key(&track_key, chunk_index, &mut chunk)?;

            if skipped_bytes < seek_target_bytes {
                let remaining_skip = (seek_target_bytes - skipped_bytes) as usize;
                if remaining_skip >= chunk.len() {
                    skipped_bytes = skipped_bytes.saturating_add(chunk.len() as u64);
                    chunk_index += 1;
                    continue;
                }

                let kept = chunk.split_off(remaining_skip);
                skipped_bytes = seek_target_bytes;
                chunk = kept;
            }

            queued_bytes += chunk.len();

            if sender.send(chunk).is_err() {
                return Ok(());
            }

            if player_thread.is_none() && queued_bytes >= PREBUFFER_BYTES {
                start_player_thread(
                    &mut player_thread,
                    &mut receiver,
                    &mut control_rx,
                    event_tx.clone(),
                )?;
            }

            chunk_index += 1;
        }
    }

    if !pending.is_empty() {
        let mut tail_chunk = pending;
        decrypt_chunk_in_place_with_key(&track_key, chunk_index, &mut tail_chunk)?;
        if skipped_bytes < seek_target_bytes {
            let remaining_skip = (seek_target_bytes - skipped_bytes) as usize;
            if remaining_skip >= tail_chunk.len() {
                return Ok(());
            }
            tail_chunk = tail_chunk.split_off(remaining_skip);
        }
        if sender.send(tail_chunk).is_err() {
            return Ok(());
        }
    }

    if player_thread.is_none() {
        start_player_thread(
            &mut player_thread,
            &mut receiver,
            &mut control_rx,
            event_tx.clone(),
        )?;
    }

    drop(sender);

    if let Some(handle) = player_thread {
        let join_result = tokio::task::block_in_place(|| handle.join())
            .map_err(|_| anyhow!("player thread panicked"))?;
        join_result?;
    }

    Ok(())
}

fn estimate_total_duration_ms(total_bytes: Option<u64>, quality: AudioQuality) -> u64 {
    let bytes_per_sec: u64 = match quality {
        AudioQuality::Kbps128 => 16_000,
        AudioQuality::Kbps320 => 40_000,
        AudioQuality::Flac => 90_000, // rough estimate for lossless
    };
    total_bytes
        .map(|bytes| bytes.saturating_mul(1000) / bytes_per_sec)
        .unwrap_or(224_000)
        .max(1)
}

fn load_saved_arl() -> AnyResult<String> {
    let arl = load_config().arl;
    let trimmed = arl.trim().to_owned();
    if trimmed.is_empty() {
        return Err(anyhow!("ARL not set. Go to Settings → Set ARL from search input"));
    }
    Ok(trimmed)
}

fn save_arl(arl: &str) -> AnyResult<()> {
    let mut config = load_config();
    config.arl = arl.trim().to_owned();
    save_config(&config)
}

fn prompt_for_arl() -> AnyResult<String> {
    print!("Enter Deezer ARL: ");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read ARL from stdin")?;

    let arl = input.trim().to_owned();
    if arl.is_empty() {
        return Err(anyhow!("ARL cannot be empty"));
    }

    Ok(arl)
}

async fn verify_arl(arl: &str) -> AnyResult<()> {
    let mut client = api::DeezerClient::new(arl.to_owned())?;
    client
        .fetch_api_token()
        .await
        .context("failed to verify ARL with Deezer")?;
    if client.user_id().is_none() {
        return Err(anyhow!("ARL is invalid: no Deezer user found for this session"));
    }
    Ok(())
}

async fn ensure_verified_arl() -> AnyResult<()> {
    let mut config = load_config();

    loop {
        let existing_arl = config.arl.trim().to_owned();

        if !existing_arl.is_empty() {
            if verify_arl(&existing_arl).await.is_ok() {
                return Ok(());
            }
            println!("Saved ARL is invalid. Please enter a new ARL.");
        } else {
            println!("No ARL found in config. Please enter your Deezer ARL.");
        }

        let entered = prompt_for_arl()?;
        match verify_arl(&entered).await {
            Ok(_) => {
                config.arl = entered;
                save_config(&config)?;
                println!("ARL saved and verified.");
                return Ok(());
            }
            Err(err) => {
                println!("ARL verification failed: {}", err);
            }
        }
    }
}

fn config_file_path() -> AnyResult<PathBuf> {
    dirs::home_dir()
        .map(|path| path.join(".deezer-tui-config.json"))
        .ok_or_else(|| anyhow!("could not resolve the user's home directory"))
}

fn load_config() -> Config {
    let Ok(path) = config_file_path() else { return Config::default() };
    let Ok(data) = fs::read_to_string(&path) else { return Config::default() };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_config(config: &Config) -> AnyResult<()> {
    let path = config_file_path()?;
    let data = serde_json::to_string_pretty(config).context("failed to serialize config")?;
    fs::write(&path, data)
        .with_context(|| format!("failed to write config file at {}", path.display()))
}
