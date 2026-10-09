//! LaunchAgent 服务管理：`evolveroute start/stop/restart/status/install`。
//! 将网关注册为 macOS 常驻服务（KeepAlive + 开机自启），无需记忆 launchctl。

use anyhow::bail;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

pub const LABEL: &str = "ai.evolveroute.gateway";
const DEFAULT_PORT: u16 = 8787;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn plist_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn uid() -> anyhow::Result<String> {
    let out = std::process::Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run(cmd: &mut std::process::Command) -> anyhow::Result<std::process::Output> {
    tracing::debug!(?cmd, "launchctl");
    Ok(cmd.output()?)
}

fn launchctl(args: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut c = std::process::Command::new("launchctl");
    c.args(args);
    run(&mut c)
}

fn gui_domain() -> anyhow::Result<String> {
    Ok(format!("gui/{}", uid()?))
}

fn is_loaded() -> bool {
    let Ok(domain) = gui_domain() else { return false };
    launchctl(&["print", &format!("{domain}/{LABEL}")]).is_ok()
}

/// 网关端口是否在监听（TCP 握手成功即视为就绪）
fn is_listening(port: u16) -> bool {
    let addr = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port);
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

fn wait_listening(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if is_listening(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

fn wait_port_closed(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !is_listening(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn write_plist(bin: &std::path::Path, config: &std::path::Path, port: u16) -> anyhow::Result<()> {
    let plist = plist_path();
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = home().join(".evolveroute/gateway.log");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>serve</string>
        <string>--config</string>
        <string>{}</string>
        <string>--port</string>
        <string>{port}</string>
    </array>
    <key>KeepAlive</key><true/>
    <key>RunAtLoad</key><true/>
    <key>StandardOutPath</key><string>{}</string>
    <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        bin.display(),
        config.display(),
        log.display(),
        log.display(),
    );
    std::fs::write(&plist, xml)?;
    println!("已写入服务定义: {}", plist.display());
    Ok(())
}

/// 当前网关端口：显式参数 > 默认配置文件 > 8787
pub fn resolve_port(explicit: Option<u16>) -> u16 {
    if let Some(p) = explicit {
        return p;
    }
    match crate::config::load_config(None) {
        Ok((cfg, _)) => cfg.server.port,
        Err(_) => DEFAULT_PORT,
    }
}

pub fn start(port: u16) -> anyhow::Result<()> {
    let domain = gui_domain()?;
    let plist = plist_path();
    if !plist.exists() {
        let bin = std::env::current_exe()?;
        let config = home().join(".evolveroute/evolveroute.toml");
        write_plist(&bin, &config, port)?;
    }
    if is_loaded() {
        launchctl(&["kickstart", &format!("{domain}/{LABEL}")])?;
    } else {
        launchctl(&["bootstrap", &domain, &plist.to_string_lossy()])?;
    }
    if wait_listening(port, Duration::from_secs(10)) {
        println!("网关已启动: http://127.0.0.1:{port}");
        Ok(())
    } else {
        bail!("启动超时：10s 内端口 {port} 未就绪，查看 {}", home().join(".evolveroute/gateway.log").display())
    }
}

pub fn stop(port: u16) -> anyhow::Result<()> {
    let domain = gui_domain()?;
    if !is_loaded() {
        println!("服务未运行");
        return Ok(());
    }
    launchctl(&["bootout", &format!("{domain}/{LABEL}")])?;
    if wait_port_closed(port, Duration::from_secs(10)) {
        println!("网关已停止");
        Ok(())
    } else {
        bail!("停止超时：端口 {port} 仍在监听")
    }
}

pub fn restart(port: u16) -> anyhow::Result<()> {
    let domain = gui_domain()?;
    let plist = plist_path();
    if !plist.exists() {
        bail!("服务尚未安装：先运行 evolveroute install");
    }
    if is_loaded() {
        launchctl(&["kickstart", "-k", &format!("{domain}/{LABEL}")])?;
    } else {
        launchctl(&["bootstrap", &domain, &plist.to_string_lossy()])?;
    }
    if wait_listening(port, Duration::from_secs(10)) {
        println!("网关已重启: http://127.0.0.1:{port}");
        Ok(())
    } else {
        bail!("重启超时：10s 内端口 {port} 未就绪")
    }
}

pub fn status(port: u16) -> anyhow::Result<()> {
    let loaded = is_loaded();
    let listening = is_listening(port);
    println!("服务注册:   {}", if loaded { "已加载" } else { "未加载" });
    println!("网关监听:   {} (http://127.0.0.1:{port})", if listening { "运行中" } else { "未运行" });
    if let Ok(out) = std::process::Command::new("pgrep").args(["-fl", "evolveroute serve"]).output() {
        let procs = String::from_utf8_lossy(&out.stdout);
        for line in procs.lines().filter(|l| !l.is_empty()) {
            println!("进程:       {line}");
        }
    }
    if loaded && listening {
        Ok(())
    } else if !loaded {
        bail!("服务未加载：运行 evolveroute start 或 evolveroute install")
    } else {
        bail!("服务已加载但端口未就绪：查看 ~/.evolveroute/gateway.log")
    }
}

/// 安装/重写服务定义（KeepAlive 崩溃自拉起 + RunAtLoad 开机自启）
pub fn install(port: u16) -> anyhow::Result<()> {
    let bin = std::env::current_exe()?;
    let config = home().join(".evolveroute/evolveroute.toml");
    if !config.exists() {
        bail!("配置文件不存在: {}（先运行 evolveroute doctor 检查）", config.display());
    }
    write_plist(&bin, &config, port)?;
    let domain = gui_domain()?;
    if is_loaded() {
        launchctl(&["kickstart", "-k", &format!("{domain}/{LABEL}")])?;
    } else {
        launchctl(&["bootstrap", &domain, &plist_path().to_string_lossy()])?;
    }
    if wait_listening(port, Duration::from_secs(10)) {
        println!("服务已安装并启动: http://127.0.0.1:{port}（开机自启已启用）");
        Ok(())
    } else {
        bail!("安装后启动超时，查看 ~/.evolveroute/gateway.log")
    }
}
