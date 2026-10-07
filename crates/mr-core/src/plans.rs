//! Provider 配额方案注册表（按 baseUrl 匹配）。
//! 数值来源：各 provider 官方文档（2026-10 录入，可在面板订正）：
//! - 智谱 GLM Coding Plan  https://docs.bigmodel.cn/cn/coding-plan/overview
//! - z.ai Devpack          https://docs.z.ai/devpack/overview
//! - OpenCode Go           https://opencode.ai/docs/go/
//! - 火山方舟 Coding/Agent Plan https://docs.volcengine.com/docs/ark/coding-plan-personal-plan-overview
pub struct PlanProfile {
    /// overrides 键 / 面板分组键
    pub key: &'static str,
    /// baseUrl 包含即命中（小写匹配）
    pub url_match: &'static str,
    pub provider: &'static str,
    /// 方案类型（面板 i18n key：scheme_*）
    pub scheme_key: &'static str,
    /// 窗口与额度摘要（语言中立：数字+单位）
    pub windows: &'static str,
    /// 模型级差异摘要（空 = 无模型级差异）
    pub models_note: &'static str,
    pub docs_url: &'static str,
    /// 常见档位（面板档位提示）
    pub tiers: &'static str,
    /// 接入类型：api / coding / go / agent
    pub plan_kind: &'static str,
    /// 档位→5h 窗口额度（积分制方案），语言中立 "tier|credits" 行
    pub tier_allowances: &'static str,
    /// 模型倍率/系数表：每行 "name|v1|v2|v3|v4"（列含义见 rates_kind）
    pub model_rates: &'static str,
    /// rates_kind: credits（输入/缓存/输出积分系数）| dollar（输入$/缓存读$/输出$/月限$）| afp（输入/输出 AFP 系数）
    pub rates_kind: &'static str,
    /// 计费口径说明
    pub rates_note: &'static str,
}
impl PartialEq for PlanProfile {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self as *const PlanProfile, other as *const PlanProfile)
    }
}
impl std::fmt::Debug for PlanProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key)
    }
}

pub const REGISTRY: &[PlanProfile] = &[
    PlanProfile {
        key: "zhipu-coding",
        url_match: "open.bigmodel.cn/api/coding",
        provider: "zhipu",
        scheme_key: "scheme_credits5h",
        windows: "5h 积分: Lite 2k · Pro 12k · Max 28k | 周积分: 10k/60k/140k | 动态刷新",
        models_note: "积分=(入×系+缓存×系+出×系)/1w · GLM-5.3: 6.9/1.7/24 · Flash: 2.3/0.56/8 · 非高峰5折(高峰=周一~五14-18 UTC+8)",
        docs_url: "https://docs.bigmodel.cn/cn/coding-plan/overview",
        tiers: "lite / pro / max",
        tier_allowances: "lite|2000\npro|12000\nmax|28000",
        model_rates: "GLM-5.3|6.9|1.7|24\nGLM-5.3-Flash|2.3|0.56|8",
        rates_kind: "credits",
        rates_note: "积分系数（输入/缓存命中/输出）· 消耗=(入+缓存+出)×系数/1万 · 非高峰(工作日14-18 UTC+8之外)5折",
        plan_kind: "coding",
    },
    PlanProfile {
        key: "zai-devpack",
        url_match: "api.z.ai/api/coding",
        provider: "z.ai",
        scheme_key: "scheme_credits5h",
        windows: "5h credits: Lite 2k · Pro 12k · Max 28k | weekly: 10k/60k/140k | dynamic refresh",
        models_note: "credits=(in×k+cached×k+out×k)/10k · GLM-5.3: 6.9/1.7/24 · Flash: 2.3/0.56/8 · off-peak 50% (peak=Mon-Fri 14-18 UTC+8)",
        docs_url: "https://docs.z.ai/devpack/overview",
        tiers: "lite / pro / max",
        tier_allowances: "lite|2000\npro|12000\nmax|28000",
        model_rates: "GLM-5.3|6.9|1.7|24\nGLM-5.3-Flash|2.3|0.56|8",
        rates_kind: "credits",
        rates_note: "credit multipliers (input/cached/output) · usage=(sum×k)/10k · off-peak 50% (peak=Mon-Fri 14-18 UTC+8)",
        plan_kind: "coding",
    },
    PlanProfile {
        key: "opencode-go",
        url_match: "opencode.ai/zen/go",
        provider: "opencode",
        scheme_key: "scheme_dollar",
        windows: "月度美元按模型限额 · 5h=月20% · 周=月50% · 月=100%",
        models_note: "Go/Plus 月限: Flash $60/$180 · GLM-5.3 $15/$120 · GLM-5.2 $60/$180 · Kimi-K3 $15/$60 · MiniMax-M3 $60/$180 · DeepSeek-V4-Pro $15/$60 (每模型独立)",
        docs_url: "https://opencode.ai/docs/go/",
        tiers: "go / go-plus",
        tier_allowances: "",
        model_rates: "glm-5.3-flash|0.15|0.03|0.50|$60\nglm-5.3|1.40|0.26|4.40|$15\nglm-5.2|1.40|0.26|4.40|$60\nkimi-k3|3.00|0.30|15.00|$15\nkimi-k2.7-code|0.95|0.19|4.00|$60\nminimax-m3|0.30|0.06|1.20|$60\nminimax-m2.7|0.30|0.06|1.20|$60\ndeepseek-v4-pro|0.66|0.022|1.98|$15\ndeepseek-v4-flash|0.15|0.003|0.60|$30\nqwen3.8-max|2.00|0.25|6.00|$15\nqwen3.8-flash|0.15|0.016|0.47|$30\nqwen3.7-plus|0.40|0.04|1.60|$60\ngpt-6-luna|0.10|0.01|0.50|$15\nlongcat-2.0|0.30|0.006|1.20|$60\nlongcat-2.5-preview-free|0|0|0|Unlimited",
        rates_kind: "dollar",
        rates_note: "$/1M tokens (input / cached-read / output) · Go 档月限，Plus 限额更高(Flash $180/5.3 $120…) · 5h=月20% 周=50%",
        plan_kind: "go",
    },
    PlanProfile {
        key: "volces-coding",
        url_match: "ark.cn-beijing.volces.com/api/coding",
        provider: "volces",
        scheme_key: "scheme_credits3w",
        windows: "5h(首请求起周期刷新) / 周(周一00:00) / 月(订阅月1日) 三窗口 | Lite/Pro",
        models_note: "抵扣系数按模型: (入×系+出×系)/1w · doubao-2.0-mini 0.25 · deepseek-v4-flash 0.5 · Kimi-K3 系数高仅建议 Pro · glm-5.3 系数较高 · 多模型 1M 上下文",
        docs_url: "https://docs.volcengine.com/docs/ark/coding-plan-personal-plan-overview?lang=zh",
        tiers: "lite / pro",
        tier_allowances: "",
        model_rates: "doubao-seed-2.0-mini|0.25|0.25\ndeepseek-v4-flash|0.5|0.5\ndoubao-seed-2.1-lite|0.5|0.5\nglm-5.3-flash|0.5|0.5\ndoubao-seed-evolving|2.5|2.5\nminimax-m3|2.5|2.5\ndoubao-seed-2.1-pro|2.5|2.5\nkimi-k2.7-code|4.5|4.5\nglm-5.3|4.5|4.5\ndeepseek-v4.1-flash|2.5|2.5\ndeepseek-v4-pro|5.5|5.5\nkimi-k3|10|10",
        rates_kind: "afp",
        rates_note: "AFP 系数（输入/输出）· 消耗=(入×系+出×系)/1万 · Auto=1(活动期) · deepseek-v4.1-flash 5折(活动期) · Coding Plan 抵扣以控制台为准",
        plan_kind: "coding",
    },
    PlanProfile {
        key: "volces-agent",
        url_match: "ark.cn-beijing.volces.com/api/agent",
        provider: "volces",
        scheme_key: "scheme_afp",
        windows: "AFP 抵扣: (入×系+出×系)/1w · Harness 层级影响系数",
        models_note: "Auto 模式系数 0.5(活动期) · deepseek-v4.1-flash 5折(活动期) · 全模态+专属 Harness",
        docs_url: "https://www.volcengine.com/docs/82379/1502001",
        tiers: "见官方活动页",
        tier_allowances: "",
        plan_kind: "agent",
        model_rates: "auto|0.5|0.5\\ndeepseek-v4.1-flash|2.5(活动5折)|2.5(活动5折)\\nkimi-k2.8-preview|8(活动6折)|8(活动6折)\\nkimi-k3|10|10\\nglm-5.3|4.5|4.5\\nglm-5.3-flash|0.5|0.5\\nminimax-m3|2.5|2.5\\ndeepseek-v4-pro|5.5|5.5",
        rates_kind: "afp",
        rates_note: "AFP 系数（输入/输出）· 消耗=(入×系+出×系)/1万 · Harness 层级影响系数 · 折扣为活动期价格",
    },
];

/// API 按量兜底（无窗口、仅限流）
pub const PAYG: PlanProfile = PlanProfile {
    key: "payg",
    url_match: "",
    provider: "-",
    scheme_key: "scheme_payg",
    windows: "-",
    models_note: "",
    docs_url: "",
    tiers: "-",
    tier_allowances: "",
    plan_kind: "api",
    model_rates: "",
    rates_kind: "",
    rates_note: "",
};

pub fn plan_for(base_url: &str) -> Option<&'static PlanProfile> {
    let l = base_url.to_lowercase();
    REGISTRY.iter().find(|p| !p.url_match.is_empty() && l.contains(p.url_match))
}

/// 订阅模型的配额消耗强度（合成价格，$/1M 量纲）：
/// 同方案内两模型的比值 = "贵多少倍"（配额烧得快多少倍）。
/// 来源为各官方文档的积分系数 / 月度美元价（模型名子串匹配）。
/// 合成口径：输入系数 + 3×输出系数（输出对窗口/额度压力约 3 倍权重）。
pub fn model_price_hint(base_url: &str, model_id: &str) -> Option<f32> {
    let p = plan_for(base_url)?;
    let m = model_id.to_lowercase();
    let v = match p.key {
        "zhipu-coding" | "zai-devpack" => {
            if m.contains("flash") { 2.3 + 3.0 * 8.0 } else { 6.9 + 3.0 * 24.0 }
        }
        "opencode-go" => {
            if m.contains("kimi-k3") {
                3.0 + 3.0 * 15.0
            } else if m.contains("v4-pro") {
                0.66 + 3.0 * 1.98
            } else if m.contains("v4-flash") || m.contains("v4.1") {
                0.15 + 3.0 * 0.60
            } else if m.contains("qwen3.8-max") {
                2.0 + 3.0 * 6.0
            } else if m.contains("qwen3.8-flash") {
                0.15 + 3.0 * 0.47
            } else if m.contains("qwen3.7") {
                0.40 + 3.0 * 1.60
            } else if m.contains("flash") {
                0.15 + 3.0 * 0.50
            } else if m.contains("5.3") || m.contains("5.2") {
                1.40 + 3.0 * 4.40
            } else if m.contains("kimi-k2") {
                0.95 + 3.0 * 4.00
            } else {
                0.30 + 3.0 * 1.20 // minimax / longcat 等
            }
        }
        "volces-coding" | "volces-agent" => {
            if m.contains("kimi-k3") {
                20.0
            } else if m.contains("v4-pro") {
                11.0
            } else if m.contains("kimi-k2") || (m.contains("5.3") && !m.contains("flash")) {
                9.0
            } else if m.contains("evolving") || m.contains("2.1-pro") || m.contains("minimax") || m.contains("v4.1") {
                5.0
            } else {
                1.0 // flash / 2.0-mini / v4-flash
            }
        }
        _ => return None,
    };
    Some(v)
}


/// 积分系数（输入/缓存命中/输出）——按官方文档的模型级倍率；非积分制返回 None
pub fn credit_multipliers(base_url: &str, model_id: &str) -> Option<(f64, f64, f64)> {
    let p = plan_for(base_url)?;
    let m = model_id.to_lowercase();
    match p.key {
        "zhipu-coding" | "zai-devpack" => {
            if m.contains("flash") { Some((2.3, 0.56, 8.0)) } else { Some((6.9, 1.7, 24.0)) }
        }
        "volces-coding" | "volces-agent" => {
            let k = if m.contains("kimi-k3") {
                10.0
            } else if m.contains("v4-pro") {
                5.5
            } else if m.contains("kimi-k2") || (m.contains("5.3") && !m.contains("flash")) {
                4.5
            } else if m.contains("evolving") || m.contains("2.1-pro") || m.contains("minimax") || m.contains("v4.1") {
                2.5
            } else {
                0.5
            };
            Some((k, 0.0, k))
        }
        _ => None,
    }
}

pub fn plan_key_for(base_url: &str) -> Option<&'static str> {
    plan_for(base_url).map(|p| p.key)
}

/// 档位对应额度（积分）；未登记返回 None
pub fn tier_allowance_by_key(key: &str, tier: &str) -> Option<f64> {
    let p = REGISTRY.iter().find(|p| p.key == key)?;
    let t = tier.to_lowercase();
    p.tier_allowances
        .split('\n')
        .filter_map(|l| l.split_once('|'))
        .find(|(k, _)| k.eq_ignore_ascii_case(&t))
        .and_then(|(_, v)| v.parse::<f64>().ok())
}

/// 窗口内积分消耗（官方系数折算）；非积分制（如 OpenCode Go 美元制）返回 0（v2 接入）
pub fn plan_credits_used(base_url: &str, model_id: &str, in_tok: f64, cached_tok: f64, out_tok: f64) -> f64 {
    match credit_multipliers(base_url, model_id) {
        Some((ki, kc, ko)) => (in_tok * ki + cached_tok * kc + out_tok * ko) / 10_000.0,
        None => 0.0,
    }
}

/// 按模型 id（provider/model 全名）折算积分消耗——子串匹配注册表 baseUrl
pub fn plan_credits_used_by_id(model_id: &str, in_tok: f64, cached_tok: f64, out_tok: f64) -> f64 {
    let l = model_id.to_lowercase();
    let base = if l.contains("zhipu") || l.contains("bigmodel") {
        "https://open.bigmodel.cn/api/coding/paas/v4"
    } else if l.contains("z.ai") || l.contains("zai") {
        "https://api.z.ai/api/coding/paas/v4"
    } else if l.contains("opencode") {
        "https://opencode.ai/zen/go/v1/chat/completions"
    } else if l.contains("volces") || l.contains("ark") {
        "https://ark.cn-beijing.volces.com/api/coding/v3"
    } else {
        return 0.0;
    };
    plan_credits_used(base, model_id, in_tok, cached_tok, out_tok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_by_base_url() {
        assert_eq!(
            plan_for("https://open.bigmodel.cn/api/coding/paas/v4").map(|p| p.key),
            Some("zhipu-coding")
        );
        assert_eq!(
            plan_for("https://api.z.ai/api/coding/paas/v4").map(|p| p.key),
            Some("zai-devpack")
        );
        assert_eq!(
            plan_for("https://opencode.ai/zen/go/v1/chat/completions").map(|p| p.key),
            Some("opencode-go")
        );
        assert_eq!(
            plan_for("https://ark.cn-beijing.volces.com/api/coding/v3").map(|p| p.key),
            Some("volces-coding")
        );
        assert_eq!(plan_for("http://127.0.0.1:9101/v1"), None);
    }

    #[test]
    fn plan_price_intensity_matches_official_ratios() {
        let zhipu = "https://open.bigmodel.cn/api/coding/paas/v4";
        let go = "https://opencode.ai/zen/go/v1/chat/completions";
        let flash = model_price_hint(zhipu, "zhipuai-coding-plan/glm-5.3-flash").unwrap();
        let big = model_price_hint(zhipu, "zhipuai-coding-plan/glm-5.3").unwrap();
        // 官方系数 6.9/1.7/24 vs 2.3/0.56/8 → GLM-5.3 烧积分约 3 倍于 Flash
        assert!((big / flash) > 2.5 && (big / flash) < 3.5, "ratio={}", big / flash);
        let go_flash = model_price_hint(go, "opencode-go/glm-5.3-flash").unwrap();
        let go_big = model_price_hint(go, "opencode-go/glm-5.3").unwrap();
        // Go 官方月限 flash $60 vs 5.3 $15 → 4 倍差距
        assert!((go_big / go_flash) > 3.0 && (go_big / go_flash) < 12.0, "ratio={}", go_big / go_flash);
        assert!(model_price_hint("http://127.0.0.1:9101/v1", "x").is_none());
    }
}

