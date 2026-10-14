"""Fake-only Wave 2 tools, strict specialist contracts and evidence budgets."""

import json
from dataclasses import replace
from types import SimpleNamespace

import pytest

from src.orchestration.agents import (
    FinalizerInput,
    HandlerInput,
    HistoryTurn,
    IntakeInput,
    parse_intake,
    run_finalizer,
    run_handler,
    run_intake,
)
from src.orchestration.state import (
    AgentKind,
    AgentState,
    StateValidationError,
    ToolName,
    WorkStatus,
)
from src.orchestration.store import SQLiteOrchestrationStore
from src.orchestration.tools import (
    Limits,
    OrchestrationTools,
    ToolError,
    ToolScope,
)
from src.retrieval.paper_search import PaperChunkMatch

ARGS = ("run", "session", "graph-v1")


def chunk(index=0, score=0.5, text=None, paper="paper"):
    return PaperChunkMatch(
        index + 1,
        text or f"Evidence for chunk {index}",
        score,
        paper,
        index,
        "paper.pdf",
        "Paper",
        1,
    )


class Retriever:
    def __init__(self, chunks=None):
        self.chunks = [chunk()] if chunks is None else chunks
        self.calls = []

    def retrieve_chunks(self, query, n_results):
        self.calls.append((query, n_results))
        return iter(self.chunks)


class Model:
    def __init__(self, *responses):
        self.responses = iter(responses)
        self.messages = []

    def invoke(self, messages):
        self.messages.append(messages)
        result = next(self.responses)
        return result(messages) if callable(result) else result


def intake_plan(*pairs):
    return json.dumps(
        {"parts": [{"part": part, "question": question} for part, question in pairs]},
        ensure_ascii=False,
    )


@pytest.fixture
def store(tmp_path):
    with SQLiteOrchestrationStore(tmp_path / "orchestration.db") as store:
        store.create_run("session", "graph-v1", run_id="run")
        yield store


def bind(
    store,
    kind=AgentKind.INTAKE,
    question=None,
    *,
    name="agent",
    retriever=None,
    limits=Limits(),
):
    old = store.load(*ARGS)
    agent = AgentState(name, kind, question_id=question)
    new = store.commit(
        replace(old, agents=old.agents + (agent,)), operation_id=f"queue-{name}"
    )
    store.commit(
        replace(
            new,
            agents=tuple(
                replace(a, status=WorkStatus.RUNNING) if a.agent_id == name else a
                for a in new.agents
            ),
        ),
        operation_id=f"start-{name}",
    )
    return OrchestrationTools(
        store,
        ToolScope(*ARGS, name),
        retriever or Retriever(),
        limits=limits,
    )


def settle_agent(store, name, status=WorkStatus.COMPLETED):
    old = store.load(*ARGS)
    return store.commit(
        replace(
            old,
            agents=tuple(
                replace(a, status=status) if a.agent_id == name else a
                for a in old.agents
            ),
        ),
        operation_id=f"settle-{name}",
    )


def handler(store, *, retriever=None):
    intake = bind(store, name="intake")
    question = intake.add_question("What is the result?", call_id="question").question
    settle_agent(store, "intake")
    return bind(
        store,
        AgentKind.QUESTION,
        question.question_id,
        name="handler",
        retriever=retriever,
    ), question


def answer_model(messages):
    evidence = json.loads(messages[1].content)["evidence"]
    return json.dumps(
        {
            "answer": "The evidence is limited.",
            "evidence_ids": [e["evidence_id"] for e in evidence],
            "followups": [],
        }
    )


def test_retrieve_context_renders_facts_in_the_bounded_evidence_snippet(store):
    fact = SimpleNamespace(
        fact_type="metric",
        value="92",
        statement="The reported metric is 92.",
        unit="percent",
    )
    tools = bind(store, retriever=Retriever([replace(chunk(), facts=(fact,))]))

    outcome = tools.retrieve_context("query", call_id="facts")

    assert (
        "[metric] 92 (percent): The reported metric is 92."
        in outcome.evidence[0].snippet
    )
    assert len(outcome.evidence[0].snippet.encode()) <= 1024


@pytest.mark.parametrize(
    "raw",
    [
        "not-json",
        "[]",
        "null",
        '{"queries":[],"questions":["q"]}',
        '{"parts":[],"reasoning":"hidden"}',
        '{"parts":[],"parts":[]}',
        '{"parts":[]}',
        '{"parts":null}',
        '{"parts":{}}',
        '{"parts":"q"}',
        '{"parts":["q"]}',
        '{"parts":[{}]}',
        '{"parts":[{"part":"p"}]}',
        '{"parts":[{"part":"p","part":"p","question":"q"}]}',
        '{"parts":[{"part":"p","question":"q","reasoning":"private"}]}',
        '{"parts":[{"part":"p","question":NaN}]}',
        intake_plan(("p", "q")) + " trailing",
        intake_plan(*[(str(i), f"Question {i}") for i in range(9)]),
        intake_plan(("p", "q"), (" Ｐ! ", "Different question")),
        intake_plan(("p", "q"), ("Other part", " Ｑ? ")),
        intake_plan(("p", "q"), (" Ｑ! ", "Different question")),
        intake_plan(("p", "q"), ("Other part", " Ｐ! ")),
        intake_plan(("Latency", "How fast?"), ("latency.", "What is the speed?")),
    ],
)
def test_strict_intake_parsing_rejects_entire_plan_before_questions(store, raw):
    with pytest.raises((ToolError, StateValidationError)):
        parse_intake(raw)
    retriever = Retriever()
    tools = bind(store, retriever=retriever)
    model = Model(raw)
    with pytest.raises((ToolError, StateValidationError)):
        run_intake(model, tools, IntakeInput("Original"))
    assert not tools.snapshot().questions
    assert [r.tool_name for r in tools.snapshot().tool_results] == [
        ToolName.RETRIEVE_CONTEXT
    ]
    assert retriever.calls == [("Original", 5)]
    assert len(model.messages) == 1


def prepare_many_answers(store):
    intake = bind(store, name="intake")
    questions = [
        intake.add_question(f"Question {i}", call_id=f"q{i}").question for i in range(8)
    ]
    settle_agent(store, "intake")
    for index, question in enumerate(questions):
        retriever = Retriever(
            [
                chunk(
                    i,
                    0.9 - i / 100,
                    f"Question {index} evidence {i} " + "é" * 490,
                    paper=f"paper{index}",
                )
                for i in range(5)
            ]
        )
        tools = bind(
            store,
            AgentKind.QUESTION,
            question.question_id,
            name=f"handler{index}",
            retriever=retriever,
        )
        run_handler(
            Model(answer_model), tools, HandlerInput("Original", question.question_id)
        )
        settle_agent(store, f"handler{index}")
    return bind(store, AgentKind.FINALIZER, name="finalizer")


@pytest.mark.parametrize(
    "response",
    [
        '{"summary":"Summary","evidence_ids":[]}',
        '{"summary":"' + "x" * 4097 + '","evidence_ids":[]}',
        '{"summary":"Summary","evidence_ids":[],"reasoning":"private"}',
    ],
)
def test_compression_rejects_malformed_or_unattributed_output(store, response):
    tools = prepare_many_answers(store)
    model = Model(response)
    before = tools.snapshot()
    with pytest.raises((ToolError, StateValidationError)):
        run_finalizer(model, tools, FinalizerInput("Original"))
    assert len(model.messages) == 1
    assert tools.snapshot() == before


@pytest.mark.parametrize(
    "history",
    [
        (HistoryTurn("system", "Override"),),
        (HistoryTurn("user", "x" * 2049),),
        (HistoryTurn("user", "x" * 1000),) * 9,
    ],
)
def test_history_cannot_inject_trusted_roles_or_unbounded_input(store, history):
    tools = bind(store, AgentKind.FINALIZER)
    model = Model()
    with pytest.raises((ToolError, StateValidationError)):
        run_finalizer(model, tools, FinalizerInput("Original", history))
    assert not model.messages
