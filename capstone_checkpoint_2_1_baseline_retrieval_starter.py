r"""Capstone Checkpoint 2.1 — Retrieval Strategy Design and Baseline Implementation (starter).
Jupytext-style cell markers (# %% / # %% [markdown]) — runnable as a
plain script AND openable as cells in VS Code / PyCharm / Jupytext.
"""

# %% [markdown]
# # Capstone Checkpoint 2.1 — Retrieval Strategy Design and Baseline Implementation
# **MO-LLM Module 2 / Required Capstone Checkpoint (120 minutes)**
#
# ## What this checkpoint is
#
# In Checkpoint 1.1, you showed that a plain LLM can't reliably answer questions about
# your corpus. Now, you will **add retrieval**: Design a retrieval strategy for your scenario
# and build a **baseline retrieval system** that finds the most relevant documents for
# a query, so the model can ground its answers in them.
#
# This mirrors the Module 2 labs — keyword (BM25), vector (semantic), and hybrid
# retrieval — applied to your own capstone corpus. The graded deliverable is the completed 
# Capstone Checkpoint 2.1 worksheet, which includes your written responses and evidence of your 
# retrieval system implementation and testing. This script provides a small working example of 
# baseline retrieval. Use it to understand the retrieval workflow, then adapt the code to implement 
# and test a baseline retriever using your selected capstone dataset.
#
# **Learning outcomes (Module 2):**
# 1. Design a retrieval strategy appropriate for a given dataset and query type.
# 2. Implement and test a baseline retrieval system using structured and/or semantic
#    approaches.

# %% [markdown]
# ## Step 1 — Keep your capstone scenario
#
# Use the **same scenario** you chose in Checkpoint 1.1.
#
# | Scenario | Corpus | Retrieval considerations |
# |---|---|---|
# | **Research Paper Navigator** | ~150 research-paper PDFs (`Labs/CapstoneDatasets/ResearchPapers/`) | long documents; you'll likely chunk them; questions often name a specific paper or compare papers. |
# | **Wikipedia Retrieval Engine** | ~2,400 Wikipedia HTML articles (`Labs/CapstoneDatasets/Wikipedia/`) | many short-to-medium articles; questions name a figure/place or span several articles. |
#
# A good baseline is keyword (BM25), semantic (embeddings + vector search), or a
# hybrid of both — exactly what you built in Labs 1.2–2.2.

# %% [markdown]
# ## Setup (~5 min)
#
# 1. **Python 3.11 or 3.12**
# 2. `pip install langchain-openai langchain-core python-dotenv`
# 3. Use the OpenRouter API key provided for this program. This checkpoint uses
#  the `openai/gpt-5.4-mini` model, with usage covered by the course credits. (this uses the paid gpt-5.4-mini chat model — covered by your course credits — and a keyword retriever, no embeddings).
# 4. Create a `.env` file next to this script: `OPENROUTER_API_KEY=sk-or-v1-...`
#
# This runs on a tiny built-in sample corpus, so you do not need to prepare your own
# dataset. It still requires an OpenRouter API key to run the LLM (it is not offline or
# free of API calls). Your real baseline (over your full corpus) is what you describe in
# the writeup.

# %%
from __future__ import annotations

import warnings
warnings.filterwarnings("ignore")

import os
import re
from datetime import datetime
from pathlib import Path

from dotenv import load_dotenv
from langchain_core.messages import HumanMessage, SystemMessage
from langchain_openai import ChatOpenAI

# %%
OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"
LLM_MODEL = "openai/gpt-5.4-mini"  # latest small OpenAI model, fast; covered by course credits
TEMPERATURE = 0.2
TOP_K = 3
LOG_PATH = Path.cwd() / "checkpoint_2_1_retrieval.log"

# === SET THIS to the scenario you chose in Checkpoint 1.1 ===
SCENARIO = "research_papers"   # "research_papers" or "wikipedia"

ANSWER_SYSTEM = (
    "You are a helpful assistant. Answer the question using ONLY the provided "
    "documents, and quote from them where you can. If the documents do not contain "
    "the answer, say so rather than guessing."
)


# %%
def check_api_key() -> str:
    load_dotenv()
    key = os.getenv("OPENROUTER_API_KEY")
    if not key:
        raise RuntimeError(
            "OPENROUTER_API_KEY is not set. Use the OpenRouter API key "
            "provided for this course, put it in a .env file next to this "
            "script, and rerun."
        )
    return key


def make_llm() -> ChatOpenAI:
    return ChatOpenAI(
        model=LLM_MODEL,
        temperature=TEMPERATURE,
        api_key=check_api_key(),
        base_url=OPENROUTER_BASE_URL,
    )


def log(label: str, text: str) -> None:
    ts = datetime.now().isoformat(timespec="seconds")
    with LOG_PATH.open("a", encoding="utf-8") as fh:
        fh.write(f"[{ts}] {label}\n{text}\n{'-' * 72}\n")


# %% [markdown]
# ## A tiny sample corpus (stands in for your real one)
#
# There are six short "documents" on distinct topics so a baseline retriever has something to
# discriminate between. Your real corpus is the PDFs/articles in `Labs/CapstoneDatasets/`,
# which came with the course in Module 1. Point your code at your local copy of that
# folder, and update the path if your checkout puts it elsewhere.


# %%
SAMPLE_DOCS = [
    {"id": "doc1", "text": "Program synthesis: generating programs automatically from a specification, such as input-output examples or a logical formula."},
    {"id": "doc2", "text": "The sketching approach lets a programmer write a partial program with holes, and a synthesizer fills the holes to satisfy a specification."},
    {"id": "doc3", "text": "Retrieval-augmented generation grounds a language model's answers in documents retrieved from a corpus, reducing hallucination."},
    {"id": "doc4", "text": "BM25 is a keyword ranking function that scores documents by term frequency and inverse document frequency."},
    {"id": "doc5", "text": "Vector search embeds text into dense vectors and ranks documents by cosine similarity to the query embedding."},
    {"id": "doc6", "text": "Evaluation of retrieval systems measures whether the retrieved documents actually contain the information needed to answer the query."},
]
DOC_BY_ID = {d["id"]: d for d in SAMPLE_DOCS}


# %% [markdown]
# ## Step 2 — The baseline retriever (provided)
#
# A simple keyword-overlap retriever scores each document by how many query words
# it shares, and returns the top-k. This is the smallest possible baseline (a stand-in
# for the BM25 / vector / hybrid retriever you built in the Module 2 labs). The
# `answer` function then asks the LLM using only the retrieved documents.

# %%
def _tokens(text: str) -> set[str]:
    return set(re.findall(r"[a-z0-9]+", text.lower()))


def retrieve(query: str, k: int = TOP_K) -> list[tuple[str, float]]:
    """Baseline keyword retrieval: Score each doc by shared-word count, return top-k."""
    q = _tokens(query)
    scored = [(d["id"], float(len(q & _tokens(d["text"])))) for d in SAMPLE_DOCS]
    scored.sort(key=lambda x: x[1], reverse=True)
    return [(doc_id, score) for doc_id, score in scored[:k] if score > 0]


def answer(llm: ChatOpenAI, query: str, doc_ids: list[str]) -> str:
    context = "\n\n".join(f"[{i}] {DOC_BY_ID[i]['text']}" for i in doc_ids if i in DOC_BY_ID)
    messages = [
        SystemMessage(content=ANSWER_SYSTEM),
        HumanMessage(content=f"Documents:\n{context}\n\nQuestion: {query}"),
    ]
    return llm.invoke(messages).content


# %% [markdown]
# ## Step 3 — Your representative queries (TODO)
#
# Submission item #2 asks for **3-5 representative queries** for your scenario and the
# results your system retrieves for each. Write those queries here. Some good ones include questions that:
#
# - Are answerable from **one** document (tests precision),
# - Need **several** documents (tests recall / aggregation),
# - Have wording that **differs** from the document's wording (i.e., tests whether
#   keyword vs. semantic retrieval matters for your corpus)
#
# Return a list of 3-5 query strings.

# %%
def my_representative_queries() -> list[str]:
    """Return 3-5 representative queries for YOUR chosen scenario.

    TODO — your turn. See the guidance above. Each item is a query string. Pick
    queries that a real user of your system would ask and that require different
    retrieval behaviors (single-doc, multi-doc, paraphrased).

    Delete the raise NotImplementedError line once your code works.
    """
    raise NotImplementedError("my_representative_queries() — see the TODO above.")


# %% [markdown]
# ## Step 4 — Run the baseline and capture the evidence
#
# This runs each query through the baseline retriever and the LLM, printing the
# retrieved document ids/scores and the grounded answer, and logging everything to
# `checkpoint_2_1_retrieval.log`. The retrieved documents from the output are the rest of the evidence for
# submission item #2.

# %%
def run() -> None:
    llm = make_llm()
    queries = my_representative_queries()
    print(f"Checkpoint 2.1 — baseline retrieval  |  scenario: {SCENARIO}\n")
    for i, query in enumerate(queries, 1):
        hits = retrieve(query, TOP_K)
        print("=" * 72)
        print(f"QUERY {i}: {query}")
        print(f"  retrieved: {hits}")
        if not hits:
            print("  (nothing matched — note this in your writeup)")
            continue
        ans = answer(llm, query, [doc_id for doc_id, _ in hits])
        print(f"  answer: {ans}\n")
        log(f"QUERY {i}: {query}", f"retrieved={hits}\nanswer={ans}")
    print("=" * 72)
    print("Done. Use the retrieved document results above as evidence in your writeup, and "
          "describe your REAL baseline (over your full corpus) in the submission.")


run()

# %% [markdown]
# ## Step 5 — Your written submission (the graded deliverable)
#
# Use your completed retrieval implementation and test results to complete the Capstone Checkpoint 2.1
# worksheet. In the worksheet, you will document your retrieval approach, provide evidence that your 
# system is functioning, include 3–5 representative queries and retrieved results, and reflect on where
# your approach performs well and where it struggles.  
 
# Save your completed Python file in the appropriate checkpoint folder in your GitHub repository. 
# Upload the completed worksheet only to the learning platform as your graded submission.
