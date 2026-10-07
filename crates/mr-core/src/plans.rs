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
};

pub fn plan_for(base_url: &str) -> Option<&'static PlanProfile> {
    let l = base_url.to_lowercase();
    REGISTRY.iter().find(|p| !p.url_match.is_empty() && l.contains(p.url_match))
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
}
