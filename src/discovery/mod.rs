pub mod local;
pub mod remote;

#[derive(Debug, Clone)]
pub struct DiscoveredInstance {
    pub pid: u32,
    pub port: u16,
    pub tmux_session: Option<String>,
    pub tmux_window: Option<String>,
    pub tmux_window_index: Option<u32>,
    pub tmux_pane_index: Option<u32>,
}
