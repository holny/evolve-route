pub const EMBEDDED_DEFAULT_CONFIG: &str = include_str!("../../../config/modelroute.default.toml");

pub fn load_config(explicit: Option<&std::path::Path>) -> anyhow::Result<(mr_core::config::FileConfig, Option<std::path::PathBuf>)> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = explicit {
        // 显式指定的配置文件不存在 = 用户敲错了路径，静默回退默认配置
        // 会让网关看似正常启动实则路由全错（审查 M-8）——必须报错
        if !p.is_file() {
            anyhow::bail!("explicit config not found: {}", p.display());
        }
        candidates.push(p.to_path_buf());
    }
    candidates.extend(mr_core::config::default_config_paths());
    mr_core::config::FileConfig::from_paths(&candidates, EMBEDDED_DEFAULT_CONFIG)
}
