#!/usr/bin/env python3
"""HTTP-level load tester for quiz-service.

Closed-loop virtual users over a single aiohttp session (keep-alive).
Scenarios:
  read   - 40% quiz detail / 35% quiz list / 25% leaderboard
  write  - startAttempt + submitAttempt cycles (exercises BEGIN CONCURRENT)
  mixed  - 75% read ops / 25% write cycles

Usage:
  python3 http-loadtest.py setup --users 24 --quizzes 12
  python3 http-loadtest.py run   --scenario read --vus 64 --duration 15
"""
from __future__ import annotations

import argparse
import asyncio
import json
import random
import sys
import time
from pathlib import Path

import aiohttp

try:
    import uvloop

    uvloop.install()
except ImportError:
    pass

STATE_FILE = Path(__file__).with_name("state.json")
URL = "http://127.0.0.1:8080/graphql"
TIMEOUT = 15.0
WARMUP = 3.0

Q_LIST = "query($f:Int!,$o:Int!){ quizzes(first:$f,offset:$o){ total items { id title questionCount creator { username } } } }"
Q_DETAIL = "query($id:ID!){ quiz(id:$id){ quiz{ title questionCount } questions { id kind points options { id } } } }"
Q_LEADER = "query($q:ID!){ leaderboard(quizId:$q,first:10){ rank score user { username } } }"
M_START = "mutation($q:ID!){ startAttempt(quizId:$q){ id status } }"
M_SUBMIT = (
    "mutation($a:ID!,$ans:[AnswerInput!]!){ submitAttempt(attemptId:$a,answers:$ans)"
    "{ score maxScore correctCount totalCount } }"
)


def gql(payload: dict, token: str | None = None) -> dict:
    headers = {"content-type": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    return {"json": payload, "headers": headers}


class Results:
    def __init__(self) -> None:
        self.lat: dict[str, list[float]] = {}
        self.errors: dict[str, int] = {}
        self.completed = 0

    def record(self, name: str, dt: float) -> None:
        self.lat.setdefault(name, []).append(dt)
        self.completed += 1

    def fail(self, name: str, why: str) -> None:
        key = f"{name}:{why}"
        self.errors[key] = self.errors.get(key, 0) + 1

    def summary(self, dur: float, vus: int, scenario: str) -> dict:
        all_lat = sorted(t for v in self.lat.values() for t in v)
        out: dict = {"scenario": scenario, "vus": vus, "duration_s": round(dur, 2),
                     "ops": self.completed,
                     "rps": round(self.completed / dur, 1) if dur > 0 else 0.0,
                     "errors": self.errors}
        for label, series in (("all", all_lat), ) + tuple(self.lat.items()):
            if series:
                s = sorted(series)
                n = len(s)
                out[f"{label}_ms"] = {
                    "p50": round(s[int(0.50 * (n - 1))] * 1000, 2),
                    "p90": round(s[int(0.90 * (n - 1))] * 1000, 2),
                    "p99": round(s[int(0.99 * (n - 1))] * 1000, 2),
                    "max": round(s[-1] * 1000, 2),
                }
        return out


async def do_post(sess, res, name, payload, token=None):
    try:
        t0 = time.perf_counter()
        async with sess.post(URL, timeout=aiohttp.ClientTimeout(total=TIMEOUT), **gql(payload, token)) as r:
            body = await r.json(content_type=None)
            dt = time.perf_counter() - t0
        if r.status != 200:
            res.fail(name, f"http{r.status}")
            return None
        if body.get("errors"):
            res.fail(name, "graphql")
            return None
        res.record(name, dt)
        return body.get("data")
    except Exception as e:  # noqa: BLE001
        res.fail(name, type(e).__name__)
        return None


# ------------------------------------------------------------------ setup
async def setup(users: int, quizzes: int) -> None:
    st = {"users": [], "tokens": [], "quizzes": []}
    async with aiohttp.ClientSession() as sess:
        for i in range(users):
            body = await do_post(sess, Results(), "register",
                                 {"query": f'mutation {{ register(username: "lt_u{i:02d}", password: "loadtest123") {{ token }} }}'})
            if body:
                st["users"].append(f"lt_u{i:02d}")
                st["tokens"].append(body["register"]["token"])

        rng = random.Random(42)
        for qi in range(quizzes):
            body = await do_post(sess, Results(), "create_quiz", {
                "query": 'mutation($i:QuizInput!){ createQuiz(input:$i){ id } }',
                "variables": {"i": {"title": f"Perf Quiz {qi:02d}", "description": "load test fixture",
                                    "isPublic": True}}},
                st["tokens"][0])
            qid = body["createQuiz"]["id"]

            questions = []
            for k in range(5):
                kind = ["single", "single", "multi", "single", "boolean"][k]
                if kind == "boolean":
                    opts = [{"text": "True", "isCorrect": k % 2 == 0}, {"text": "False", "isCorrect": k % 2 == 1}]
                else:
                    correct = rng.randrange(4)
                    opts = [{"text": f"opt {j}", "isCorrect": j == correct} for j in range(4)]
                    if kind == "multi":
                        opts = [{"text": f"opt {j}", "isCorrect": j in (correct, (correct + 2) % 4)} for j in range(4)]
                questions.append({"kind": kind.upper(), "text": f"Q{qi}.{k} body text {rng.random():.6f}",
                                  "points": 1.0, "options": opts})
            await do_post(sess, Results(), "add_questions", {
                "query": "mutation($q:ID!,$qs:[QuestionInput!]!){ addQuestions(quizId:$q,input:$qs){ id } }",
                "variables": {"q": qid, "qs": questions}}, st["tokens"][0])

            detail = await do_post(sess, Results(), "detail", {
                "query": Q_DETAIL, "variables": {"id": qid}}, st["tokens"][0])
            st["quizzes"].append({
                "id": qid,
                "questions": [{"id": q["id"],
                               "options": [o["id"] for o in q["options"]]}
                              for q in detail["quiz"]["questions"]],
            })
    STATE_FILE.write_text(json.dumps(st))
    print(f"setup done: {len(st['tokens'])} users, {len(st['quizzes'])} quizzes "
          f"({sum(len(q['questions']) for q in st['quizzes'])} questions)")


# ------------------------------------------------------------------ scenarios
async def read_op(sess, res, st, tok):
    r = random.random()
    if r < 0.40:
        qz = random.choice(st["quizzes"])
        await do_post(sess, res, "quiz_detail", {"query": Q_DETAIL, "variables": {"id": qz["id"]}}, tok)
    elif r < 0.75:
        await do_post(sess, res, "quiz_list",
                      {"query": Q_LIST, "variables": {"f": 20, "o": random.randrange(0, 40)}}, tok)
    else:
        qz = random.choice(st["quizzes"])
        await do_post(sess, res, "leaderboard", {"query": Q_LEADER, "variables": {"q": qz["id"]}}, tok)


async def write_cycle(sess, res, st, tok):
    qz = random.choice(st["quizzes"])
    body = await do_post(sess, res, "start_attempt", {"query": M_START, "variables": {"q": qz["id"]}}, tok)
    if not body:
        return
    aid = body["startAttempt"]["id"]
    ans = [{"questionId": q["id"],
            "selectedOptionIds": [random.choice(q["options"])]}
           for q in qz["questions"]]
    await do_post(sess, res, "submit_attempt", {"query": M_SUBMIT, "variables": {"a": aid, "ans": ans}}, tok)


async def vu_worker(vu: int, scenario: str, st, res, deadline: float):
    conn = aiohttp.TCPConnector(limit=0, keepalive_timeout=65)
    async with aiohttp.ClientSession(connector=conn) as sess:
        tok = st["tokens"][vu % len(st["tokens"])]
        while time.perf_counter() < deadline:
            if scenario == "read":
                await read_op(sess, res, st, tok)
            elif scenario == "write":
                await write_cycle(sess, res, st, tok)
            else:  # mixed
                if random.random() < 0.75:
                    await read_op(sess, res, st, tok)
                else:
                    await write_cycle(sess, res, st, tok)


async def run(scenario: str, vus: int, duration: float) -> dict:
    st = json.loads(STATE_FILE.read_text())
    res = Results()
    conn = aiohttp.TCPConnector(limit=0, keepalive_timeout=65)
    sess = aiohttp.ClientSession(connector=conn)
    # warmup (unmeasured)
    warm_deadline = time.perf_counter() + WARMUP
    tasks = [asyncio.create_task(vu_worker(i, scenario, st, res, warm_deadline)) for i in range(vus)]
    await asyncio.gather(*tasks)
    res = Results()

    t0 = time.perf_counter()
    deadline = t0 + duration
    tasks = [asyncio.create_task(vu_worker(i, scenario, st, res, deadline)) for i in range(vus)]
    await asyncio.gather(*tasks)
    dur = time.perf_counter() - t0
    await sess.close()
    out = res.summary(dur, vus, scenario)
    print(json.dumps(out))
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    s1 = sub.add_parser("setup")
    s1.add_argument("--users", type=int, default=24)
    s1.add_argument("--quizzes", type=int, default=12)
    s2 = sub.add_parser("run")
    s2.add_argument("--scenario", choices=["read", "write", "mixed"], required=True)
    s2.add_argument("--vus", type=int, default=32)
    s2.add_argument("--duration", type=float, default=15.0)
    args = ap.parse_args()
    if args.cmd == "setup":
        asyncio.run(setup(args.users, args.quizzes))
    else:
        asyncio.run(run(args.scenario, args.vus, args.duration))


if __name__ == "__main__":
    main()
