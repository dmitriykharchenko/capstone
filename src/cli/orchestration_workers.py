"""Owned, killable specialist processes; providers are constructed inside workers.

The CLI and its workers share the bridge-owned process group. Individual handle
close terminates and joins its worker; channel failure terminates the whole group.
No provider exception, message object, prompt, or credential travels back on IPC.
"""

from dataclasses import replace
import json
import multiprocessing

from ..orchestration_runtime import specialist_steps
from ..orchestration_store import SQLiteOrchestrationStore
from ..orchestration_tools import OrchestrationTools
from ..orchestration_wire import ERROR, validate_type


class StructuredModelAdapter:
    def __init__(self, model):
        self.model = model

    def invoke(self, messages):
        from langchain_core.messages import HumanMessage, SystemMessage
        from langsmith import tracing_context

        converted = [
            SystemMessage(m.content) if m.role == "system" else HumanMessage(m.content)
            for m in messages
        ]
        with tracing_context(enabled=False):
            response = self.model.invoke(converted, config={"callbacks": []})
        # No raw provider object is ever persisted or returned from the worker.
        if not isinstance(response.content, str):
            raise ValueError(ERROR)
        return response.content


class OfflineStructuredModel:
    def invoke(self, messages):
        prompt = messages[0].content
        data = json.loads(messages[-1].content)
        if prompt.startswith("INTAKE:"):
            result = {"queries": [], "questions": [data["original_query"]]}
        elif prompt.startswith("QUESTION-HANDLER:"):
            result = {
                "answer": "No provider is configured; no factual answer is available.",
                "evidence_ids": [],
                "followups": [],
            }
        elif prompt.startswith("EVIDENCE-COMPRESSION:"):
            result = {
                "summary": "Offline evidence summary.",
                "evidence_ids": [e["evidence_id"] for e in data["evidence"]],
            }
        else:
            result = {
                "assistant_text": "[offline mock reply] No model is configured. Evidence is incomplete. You said: "
                + data["original_query"]
            }
        return json.dumps(result)


class DisplayRetriever:
    def __init__(self, store, args, retriever):
        self.store, self.args, self.retriever = store, args, retriever

    def retrieve_chunks(self, query, n_results=5):
        from itertools import islice

        if self.retriever is None:
            return []
        iterator = iter(self.retriever.retrieve_chunks(query, n_results=n_results))
        try:
            chunks = list(islice(iterator, n_results))
        finally:
            close = getattr(iterator, "close", None)
            if close:
                close()
        metadata = []
        for chunk in chunks:
            labels = {}
            for name in ("filename", "title"):
                value = getattr(chunk, name)
                try:
                    validate_type(value, "label")
                except ValueError:
                    value = "[Unsafe source label omitted]"
                labels[name] = value
            metadata.append(
                dict(paper_id=chunk.paper_id, chunk_index=chunk.chunk_index, **labels)
            )
        self.store.remember_sources(*self.args, metadata)
        return chunks


def production_model(config):
    if not config.has_api_key:
        return OfflineStructuredModel()
    from ..agent_loop import ChatOpenAIConfig, _create_chat_openai

    return StructuredModelAdapter(
        _create_chat_openai(
            ChatOpenAIConfig(
                model=config.chat_model,
                base_url=config.chat_base_url,
                api_key=config.api_key,
                max_tokens=config.chat_max_tokens,
            )
        )
    )


def _worker(pipe, config, dispatch):
    try:
        from .models import build_paper_search_index

        with SQLiteOrchestrationStore(dispatch.tools._store.database_path) as store:
            scope = dispatch.scope
            args = (scope.run_id, scope.session_id, scope.graph_version)
            retriever = DisplayRetriever(store, args, build_paper_search_index(config))
            tools = OrchestrationTools(
                store, scope, retriever, limits=dispatch.tools.limits
            )
            steps = specialist_steps(
                replace(dispatch, tools=tools), production_model(config)
            )
            try:
                for item in steps:
                    pipe.send(item)
            finally:
                steps.close()
    except Exception:
        pipe.send(None)
    finally:
        pipe.close()


class ProcessHandle:
    def __init__(self, config, dispatch):
        context = multiprocessing.get_context("spawn")
        self.reader, writer = context.Pipe(duplex=False)
        self.process = context.Process(
            target=_worker, args=(writer, config, dispatch), daemon=True
        )
        try:
            self.process.start()
        except BaseException:
            self.reader.close()
            writer.close()
            raise
        writer.close()
        self.closed = False

    def poll(self):
        if self.reader.poll():
            value = self.reader.recv()
            if value is None:
                raise RuntimeError(ERROR)
            return value
        if not self.process.is_alive():
            raise RuntimeError(ERROR)
        return None

    def close(self):
        if not self.closed:
            self.closed = True
            self.reader.close()
            if self.process.is_alive():
                self.process.terminate()
            self.process.join(timeout=2)
            if self.process.is_alive():
                self.process.kill()
                self.process.join()
            self.process.close()


class ProcessExecutor:
    def __init__(self, config):
        self.config = config

    def submit(self, operation_id, dispatch):
        return ProcessHandle(self.config, dispatch)
