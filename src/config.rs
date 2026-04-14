pub struct Config {
    pub poll_interval_secs: u16,
    pub hosts: Vec<HostConfig>,
}

pub struct HostConfig {
    pub name: String,
    pub ssh_target: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            poll_interval_secs: 5,
            hosts: vec![],
        }
    }
}
