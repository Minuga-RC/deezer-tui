use ratatui::widgets::ListState;
use tokio::sync::mpsc;

use crate::config::{AudioQuality, Config};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    PlayTrack(String),
    AutoPlayTrack(String),
    PlayTrackAt {
        track_id: String,
        quality: AudioQuality,
        seek_ms: u64,
    },
    Pause,
    Resume,
    SetVolume(u16),
    SetQuality(AudioQuality),
    SetCrossfade { enabled: bool, duration_ms: u64 },
    LoadPlaylist(String),
    LoadHome,
    LoadExplore,
    LoadFavorites,
    Search(String),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    PlaybackProgress {
        current_ms: u64,
        total_ms: u64,
    },
    TrackChanged {
        id: String,
        title: String,
        artist: String,
        quality: AudioQuality,
        album_art_url: Option<String>,
        initial_ms: u64,
    },
    PlaybackPaused,
    PlaybackResumed,
    PlaybackStopped,
    Error(String),
    PlaylistsLoaded(Vec<(String, String)>),
    TracksLoaded(Vec<(String, String, String)>),
    SearchResultsLoaded {
        tracks: Vec<(String, String, String)>,
        playlists: Vec<(String, String)>,
        artists: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchCategory {
    Tracks,
    Playlists,
    Artists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub quality: AudioQuality,
    pub current_ms: u64,
    pub total_ms: u64,
    pub album_art_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePanel {
    Navigation,
    Playlists,
    Queue,
    Search,
    Main,
    Player,
    PlayerProgress,
    PlayerInfo,
}

#[derive(Debug, Clone)]
pub struct App {
    pub config: Config,
    pub command_sender: mpsc::UnboundedSender<Command>,
    pub now_playing: Option<NowPlaying>,
    pub is_playing: bool,
    pub volume: u16,
    pub discord_rpc_enabled: bool,
    pub active_panel: ActivePanel,
    pub nav_state: ListState,
    pub playlist_state: ListState,
    pub queue_state: ListState,
    pub playlists: Vec<(String, String)>,
    pub queue: Vec<String>,
    pub queue_tracks: Vec<(String, String, String)>,
    pub queue_index: Option<usize>,
    pub current_tracks: Vec<(String, String, String)>,
    pub search_playlists: Vec<(String, String)>,
    pub search_artists: Vec<(String, String)>,
    pub showing_search_results: bool,
    pub search_category: SearchCategory,
    pub main_state: ListState,
    pub settings_state: ListState,
    pub player_button_index: usize,
    pub player_info_index: usize,
    pub repeat_mode: RepeatMode,
    pub viewing_settings: bool,
    pub current_playlist_id: Option<String>,
    pub status_message: String,
    pub is_searching: bool,
    pub search_query: String,
    pub auto_transition_armed: bool,
    /// Decoded cover-art image for the current track (None until downloaded).
    pub cover_art: Option<image::DynamicImage>,
    pub cover_art_png: Option<Vec<u8>>,
    pub cover_art_track_id: Option<String>,
}

impl App {
    pub fn new(config: Config, command_sender: mpsc::UnboundedSender<Command>) -> Self {
        let mut nav_state = ListState::default();
        nav_state.select(Some(0));

        let mut playlist_state = ListState::default();
        playlist_state.select(Some(0));

        let mut queue_state = ListState::default();
        queue_state.select(Some(0));

        let mut main_state = ListState::default();
        main_state.select(Some(0));

        let mut settings_state = ListState::default();
        settings_state.select(Some(0));

        let discord_rpc_enabled = config.discord_rpc_enabled;

        Self {
            volume: 100,
            config,
            command_sender,
            now_playing: None,
            is_playing: false,
            discord_rpc_enabled,
            active_panel: ActivePanel::Navigation,
            nav_state,
            playlist_state,
            queue_state,
            playlists: vec![],
            queue: vec![],
            queue_tracks: vec![],
            queue_index: None,
            current_tracks: vec![],
            search_playlists: vec![],
            search_artists: vec![],
            showing_search_results: false,
            search_category: SearchCategory::Tracks,
            main_state,
            settings_state,
            player_button_index: 2,
            player_info_index: 0,
            repeat_mode: RepeatMode::Off,
            viewing_settings: false,
            current_playlist_id: None,
            status_message: "Status: Waiting...".into(),
            is_searching: false,
            search_query: String::new(),
            auto_transition_armed: false,
            cover_art: None,
            cover_art_png: None,
            cover_art_track_id: None,
        }
    }

    pub fn handle_down(&mut self) {
        match self.active_panel {
            ActivePanel::Navigation => {
                let current = self.nav_state.selected().unwrap_or(0);
                if current >= 3 {
                    self.active_panel = ActivePanel::Playlists;
                    if !self.playlists.is_empty() {
                        self.playlist_state.select(Some(0));
                    }
                } else {
                    self.nav_state.select(Some(current + 1));
                }
            }
            ActivePanel::Playlists => {
                if self.playlists.is_empty() {
                    self.active_panel = ActivePanel::Queue;
                    if !self.queue.is_empty() {
                        self.queue_state.select(Some(0));
                    }
                    return;
                }

                let max = self.playlists.len() - 1;
                let current = self.playlist_state.selected().unwrap_or(0);
                if current >= max {
                    self.active_panel = ActivePanel::Queue;
                    if !self.queue.is_empty() {
                        self.queue_state.select(Some(0));
                    }
                } else {
                    self.playlist_state.select(Some(current + 1));
                }
            }
            ActivePanel::Queue => {
                if self.queue.is_empty() {
                    return;
                }

                let max = self.queue.len() - 1;
                let current = self.queue_state.selected().unwrap_or(0);
                self.queue_state.select(Some((current + 1).min(max)));
            }
            ActivePanel::Search => {
                self.active_panel = ActivePanel::Main;
            }
            ActivePanel::Main => {
                if self.viewing_settings {
                    let max = 4usize;
                    let current = self.settings_state.selected().unwrap_or(0);
                    self.settings_state.select(Some((current + 1).min(max)));
                } else if self.showing_search_results {
                    let max = match self.search_category {
                        SearchCategory::Tracks => self.current_tracks.len(),
                        SearchCategory::Playlists => self.search_playlists.len().saturating_sub(1),
                        SearchCategory::Artists => self.search_artists.len().saturating_sub(1),
                    };
                    let current = self.main_state.selected().unwrap_or(0);
                    self.main_state.select(Some((current + 1).min(max)));
                } else if !self.current_tracks.is_empty() {
                    // +1 for the top action row: "Play Playlist"
                    let max = self.current_tracks.len();
                    let current = self.main_state.selected().unwrap_or(0);
                    self.main_state.select(Some((current + 1).min(max)));
                } else {
                    self.active_panel = ActivePanel::Player;
                }
            }
            ActivePanel::Player => {
                self.active_panel = ActivePanel::PlayerProgress;
            }
            ActivePanel::PlayerProgress => {}
            ActivePanel::PlayerInfo => {
                self.player_info_index = (self.player_info_index + 1).min(1);
            }
        }
    }

    pub fn handle_up(&mut self) {
        match self.active_panel {
            ActivePanel::Queue => {
                if self.queue.is_empty() {
                    self.active_panel = ActivePanel::Playlists;
                    if !self.playlists.is_empty() {
                        self.playlist_state.select(Some(self.playlists.len() - 1));
                    }
                    return;
                }

                let current = self.queue_state.selected().unwrap_or(0);
                if current == 0 {
                    self.active_panel = ActivePanel::Playlists;
                    if !self.playlists.is_empty() {
                        self.playlist_state.select(Some(self.playlists.len() - 1));
                    }
                } else {
                    self.queue_state.select(Some(current - 1));
                }
            }
            ActivePanel::Playlists => {
                if self.playlists.is_empty() {
                    self.active_panel = ActivePanel::Navigation;
                    self.nav_state.select(Some(3));
                    return;
                }

                let current = self.playlist_state.selected().unwrap_or(0);
                if current == 0 {
                    self.active_panel = ActivePanel::Navigation;
                    self.nav_state.select(Some(3));
                } else {
                    self.playlist_state.select(Some(current - 1));
                }
            }
            ActivePanel::Navigation => {
                let current = self.nav_state.selected().unwrap_or(0);
                self.nav_state.select(Some(current.saturating_sub(1)));
            }
            ActivePanel::Search => {
                self.active_panel = ActivePanel::Navigation;
            }
            ActivePanel::Main => {
                if self.viewing_settings {
                    let current = self.settings_state.selected().unwrap_or(0);
                    self.settings_state.select(Some(current.saturating_sub(1)));
                } else if !self.current_tracks.is_empty() {
                    let current = self.main_state.selected().unwrap_or(0);
                    if current == 0 {
                        self.active_panel = ActivePanel::Search;
                    } else {
                        self.main_state.select(Some(current.saturating_sub(1)));
                    }
                }
            }
            ActivePanel::Player => {
                self.active_panel = ActivePanel::Main;
            }
            ActivePanel::PlayerProgress => {
                self.active_panel = ActivePanel::Player;
            }
            ActivePanel::PlayerInfo => {
                self.active_panel = ActivePanel::Player;
            }
        }
    }

    pub fn handle_right(&mut self) {
        match self.active_panel {
            ActivePanel::Navigation | ActivePanel::Playlists | ActivePanel::Queue => {
                self.active_panel = ActivePanel::Search;
            }
            ActivePanel::Search => {
                self.active_panel = ActivePanel::Main;
            }
            ActivePanel::Main => {
                self.active_panel = ActivePanel::Player;
            }
            ActivePanel::Player => {
                if self.player_button_index < 4 {
                    self.player_button_index += 1;
                } else {
                    self.active_panel = ActivePanel::PlayerInfo;
                }
            }
            ActivePanel::PlayerProgress => {
                self.active_panel = ActivePanel::PlayerInfo;
            }
            ActivePanel::PlayerInfo => {
                self.player_info_index = (self.player_info_index + 1).min(1);
            }
        }
    }

    pub fn handle_left(&mut self) {
        match self.active_panel {
            ActivePanel::Main => {
                if self.showing_search_results {
                    self.search_category = match self.search_category {
                        SearchCategory::Tracks => SearchCategory::Artists,
                        SearchCategory::Playlists => SearchCategory::Tracks,
                        SearchCategory::Artists => SearchCategory::Playlists,
                    };
                    self.main_state.select(Some(0));
                } else {
                    self.active_panel = ActivePanel::Playlists;
                }
            }
            ActivePanel::Player => {
                self.player_button_index = self.player_button_index.saturating_sub(1);
            }
            ActivePanel::PlayerProgress => {
                self.active_panel = ActivePanel::Player;
            }
            ActivePanel::PlayerInfo => {
                if self.player_info_index == 0 {
                    self.active_panel = ActivePanel::Player;
                    self.player_button_index = 4;
                } else {
                    self.player_info_index -= 1;
                }
            }
            _ => {}
        }
    }

    pub fn switch_search_category_right(&mut self) {
        if self.showing_search_results && self.active_panel == ActivePanel::Main {
            self.search_category = match self.search_category {
                SearchCategory::Tracks => SearchCategory::Playlists,
                SearchCategory::Playlists => SearchCategory::Artists,
                SearchCategory::Artists => SearchCategory::Tracks,
            };
            self.main_state.select(Some(0));
        }
    }
}
