"""Laya decision-model sidecar for ModelRoute.

Exposes laya's System-1 typed decisions (choice/score/noul) over local HTTP
so the Rust gateway can use the multilingual decision model with ~35ms
latency and zero per-call cost. Runs fully local: no request content ever
leaves the machine.

Install & run:
    pip install "laya[judge]" fastapi "uvicorn[standard]"
    uvicorn laya_server:app --host 127.0.0.1 --port 8321

Gateway config (modelroute.toml):
    [decision]
    backend = "laya"
    # LAYA_URL env overrides http://127.0.0.1:8321
"""

from __future__ import annotations

from fastapi import FastAPI
from pydantic import BaseModel

app = FastAPI(title="modelroute-laya-sidecar")

# 语言感知检查点（laya README）：CJK 文本走 multilingual（100+语言），
# 英文走 typed-decisions（针对 typed workflow 微调，benchmark 最优）。
# 两个检查点都常驻，切换零重载。
CHECKPOINT_FOR_LANG = {
    "typed-decisions": "english",      # ModernBERT-large，英文 typed workflow 最优
    "multilingual": "multilingual",    # mmBERT，100+ 语言
}

_router = None


def get_router():
    global _router
    if _router is None:
        from laya import Router

        _router = Router(preload=True, max_loaded=3)
    return _router


class JudgeRequest(BaseModel):
    state: dict
    lang_hint: str = "auto"


QUESTIONS = {
    "task_domain": {
        "type": "choice",
        "instructions": "What is the primary domain of the user's current request?",
        "criteria": {
            "code": "software engineering, refactoring, debugging, architecture, tests",
            "math_logic": "mathematics, proofs, calculations, logic puzzles",
            "writing": "creative or professional writing, emails, translation, copy",
            "factual_lookup": "facts, definitions, how-to questions",
            "data_analysis": "statistics, SQL, spreadsheets, metrics",
            "chitchat": "greetings, small talk, pleasantries",
            "agent_ops": "running commands, managing services, shell work",
            "other": "none of the above",
        },
    },
    "difficulty": {
        "type": "score",
        "instructions": "How hard is this request for a language model?",
        "criteria": [
            "trivial: a lookup or one-liner",
            "easy: short answer, no reasoning",
            "moderate: several steps or careful editing",
            "hard: long multi-step reasoning or specialist knowledge",
        ],
    },
    "needs_vision": {"type": "noul", "instructions": "Does answering require seeing images?"},
    "is_trivial": {"type": "noul", "instructions": "Is this answerable in one short sentence with no tools?"},
    "tool_heavy": {"type": "noul", "instructions": "Is this request tool-intensive?"},
    "high_stakes": {"type": "noul", "instructions": "Could this request cause money, legal, medical, safety or production damage?"},
    "session_relevance": {"type": "noul", "instructions": "Does the current message continue the session's main task rather than switch topics?"},
    "session_depth": {
        "type": "score",
        "instructions": "How complex is the ongoing session task?",
        "criteria": ["simple ongoing task", "moderate ongoing task", "complex ongoing task"],
    },
}

DOMAIN_MAP = {
    "code": "code",
    "math_logic": "math_logic",
    "writing": "writing",
    "factual_lookup": "lookup",
    "data_analysis": "data",
    "chitchat": "chitchat",
    "agent_ops": "agent_ops",
    "other": "other",
}


@app.get("/health")
def health() -> dict:
    try:
        get_router()
        return {"status": "ok", "backend": "laya"}
    except Exception as e:  # noqa: BLE001
        return {"status": "error", "detail": str(e)}


def pick_checkpoint(req: JudgeRequest) -> str:
    # CJK 占比高 -> multilingual；英文 -> typed-decisions；auto 交给 Router 自检
    text = json.dumps(req.state, ensure_ascii=False)
    cjk = sum(1 for ch in text if "\u4e00" <= ch <= "\u9fff" or "\u3040" <= ch <= "\u30ff" or "\uac00" <= ch <= "\ud7af")
    total = max(len(text), 1)
    if req.lang_hint == "auto" and cjk / total > 0.15:
        return "multilingual"
    if req.lang_hint in CHECKPOINT_FOR_LANG.values():
        return req.lang_hint
    return "typed-decisions"


@app.post("/v1/judge")
def judge(req: JudgeRequest) -> dict:
    import json as _json

    router = get_router()
    checkpoint = pick_checkpoint(req)
    result = router.predict(req.state, QUESTIONS, model=checkpoint)
    answers = result["answers"]
    answers = result["answers"]

    domain_raw = answers["task_domain"]["choice"]

    def noul(name: str) -> dict:
        return {"noul": float(answers[name]["noul"])}

    def scored(name: str) -> dict:
        return {
            "score": min(3.0, max(0.0, float(answers[name]["score"]))),
            "confidence": answers[name].get("confidence", 0.5),
        }

    # same answers-shape as the TypeSafe API so the gateway parses both alike
    return {
        "answers": {
            "task_domain": {
                "choice": DOMAIN_MAP.get(domain_raw, "other"),
                "confidence": answers["task_domain"].get("confidence", 0.5),
            },
            "difficulty": scored("difficulty"),
            "needs_vision": noul("needs_vision"),
            "is_trivial": noul("is_trivial"),
            "tool_heavy": noul("tool_heavy"),
            "high_stakes": noul("high_stakes"),
            "session_relevance": noul("session_relevance"),
            "session_depth": scored("session_depth"),
        },
        "routing": result.get("routing", {}),
    }
