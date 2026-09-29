pub const EMBEDDED_DEFAULT_CONFIG: &str = include_str!("../../../config/modelroute.default.toml");

pub fn load_config(explicit: Option<&std::path::Path>) -> anyhow::Result<(mr_core::config::FileConfig, Option<std::path::PathBuf>)> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = explicit {
        candidates.push(p.to_path_buf());
    }
    candidates.extend(mr_core::config::default_config_paths());
    mr_core::config::FileConfig::from_paths(&candidates, EMBEDDED_DEFAULT_CONFIG)
}
