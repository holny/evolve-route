use crate::types::*;

pub struct HeuristicJudge;

/// Tech keywords are deliberately ASCII: they appear verbatim inside any
/// language's text (zh/ja/ko/ru/fr/de devs write "refactor", "bug", "SQL"),
/// so code-domain detection works cross-lingually without translation.
const CODE_KW: &[&str] = &[
    "refactor", "重构", "debug", "compile", "compiler", "schema", "stack trace", "unit test",
    "e2e", "dependency", "deploy", "docker", "kubernetes", "git", "commit", "merge", "lint",
    "typescript", "python", "rust", "golang", "java", "代码", "函数", "编译", "报错", "类型",
    "接口", "测试", "依赖", "部署", "分支",
];
const MATH_KW: &[&str] = &["solve", "math", "equation", "proof", "计算", "证明", "方程", "概率", "积分"];
const WRITING_KW: &[&str] = &[
    "email", "blog post", "essay", "translate", "rewrite", "draft", "邮件", "翻译", "润色", "文案",
    "文章", "博客",
];
const LOOKUP_KW: &[&str] = &[
    "what is", "how to", "why does", "什么", "为什么", "怎么做", "とは", "뭐야", "어떻게",
    "qu'est-ce", "wie funktioniert", "что такое", "как",
];
const DATA_KW: &[&str] = &["sql", "pandas", "dataset", "spreadsheet", "统计", "数据集", "表格", "报表", "指标"];
const AGENT_KW: &[&str] = &["run", "exec", "install", "curl", "systemctl", "ps ", "运行", "执行", "安装", "启动"];
const STEP_KW: &[&str] = &[
    "step", "pipeline", "first.*then", "然后", "接着", "步骤", "分步", "流程",
    "次に", "단계", "затем", "puis", "dann", "потом",
];
const STAKES_KW: &[&str] = &[
    "payment", "production", "prod db", "drop table", "rm -rf", "gdpr", "hipaa", "legal",
    "支付", "付款", "生产环境", "生产库", "删除", "删库", "法律", "合同", "医疗", "安全漏洞", "密钥", "密码",
    "本番", "결제", "삭제",
];
/// Minimal everyday-chat markers per script: chat/lookup domains only.
/// Domain work is detected by L0 signals (code density) + ASCII tech terms.
const CHAT_KW: &[&str] = &[
    "hello", "hi ", "thanks", "你好", "在吗", "谢谢", "早安", "晚安",
    "こんにちは", "ありがとう", "안녕", "감사",
    "bonjour", "merci", "hallo", "danke", "привет", "спасибо",
];

fn kw_score(text: &str, kws: &[&str]) -> f32 {
    let lower = text.to_lowercase();
    kws.iter().filter(|k| lower.contains(&k.to_lowercase())).count() as f32
}

impl crate::types::Judge for HeuristicJudge {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals) -> JudgmentSet {
        Self::judge(features, digest)
    }
}

impl HeuristicJudge {
    pub fn judge(features: &RequestFeatures, digest: &DigestSignals) -> JudgmentSet {
        let text = &digest.last_user_text;
        let first = &digest.first_user_text;
        let len = text.chars().count();

        let code_s = kw_score(text, CODE_KW) + features.code_density * 6.0;
        let math_s = kw_score(text, MATH_KW);
        let writing_s = kw_score(text, WRITING_KW);
        let lookup_s = kw_score(text, LOOKUP_KW);
        let data_s = kw_score(text, DATA_KW);
        let chat_s = kw_score(text, CHAT_KW) * 1.5 + if len < 12 { 0.5 } else { 0.0 };
        let agent_s = kw_score(text, AGENT_KW) + if features.tool_count > 0 { 0.5 } else { 0.0 };

        let mut ranked = [
            (Domain::Code, code_s),
            (Domain::MathLogic, math_s),
            (Domain::Writing, writing_s),
            (Domain::Lookup, lookup_s),
            (Domain::Data, data_s),
            (Domain::Chitchat, chat_s),
            (Domain::AgentOps, agent_s),
            (Domain::Other, 0.5),
        ];
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let domain = ranked[0].0;
        let margin = (ranked[0].1 - ranked[1].1).max(0.0);
        let domain_confidence = (0.4 + margin.min(3.0) / 3.0 * 0.4).min(0.8);

        let mut difficulty = 0.0f32;
        if len > 60 { difficulty += 0.4; }
        if len > 300 { difficulty += 0.4; }
        if len > 1500 { difficulty += 0.4; }
        difficulty += features.code_density * 1.2;
        if features.tool_count > 3 { difficulty += 0.4; }
        if features.tool_count > 12 { difficulty += 0.4; }
        difficulty += kw_score(text, STEP_KW) * 0.2;
        if kw_score(first, CODE_KW) >= 2.0 { difficulty += 0.3; }
        if features.est_input_tokens > 30_000 { difficulty += 0.5; }
        if features.est_input_tokens > 80_000 { difficulty += 0.8; }

        let session_depth = if digest.session_tools_seen > 8 || first.chars().count() > 200 { 1.6 } else { 0.7 };
        let overlap = digest.overlap_ratio;
        let local_rel = if digest.topic_shift_marker {
            0.1
        } else if digest.has_deixis {
            0.75
        } else if overlap > 0.15 {
            0.6
        } else {
            0.25
        };
        let session_relevance = local_rel;
        let relevance = session_relevance;
        difficulty += relevance * (session_depth * 0.5);
        let difficulty = difficulty.clamp(0.0, 3.0);

        let is_trivial = if len < 30 && features.code_density < 0.05 && features.tool_count <= 1 {
            0.85
        } else if difficulty < 0.5 {
            0.6
        } else {
            0.1
        };
        let tool_heavy = (features.tool_ratio * 1.4).min(0.95);
        let high_stakes = (kw_score(text, STAKES_KW) * 0.4).min(0.95);
        let needs_vision = if features.has_images { 0.9 } else { 0.0 };

        JudgmentSet {
            domain,
            domain_confidence,
            difficulty,
            difficulty_confidence: 0.65,
            needs_vision,
            is_trivial,
            tool_heavy,
            high_stakes,
            session_relevance,
            session_depth,
            judge_source: "heuristic",
        }
    }
}
