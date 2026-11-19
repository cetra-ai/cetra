"""Python SDK for cetra.

Tracing, scores and configuration follow ``docs/spec/10-sdk.md``.
"""

import importlib.metadata

# __version__ is set before the submodule imports: _runtime imports it from here.
__version__ = importlib.metadata.version("cetra-sdk")

from cetra._client import ApiError, Client
from cetra._config import ConfigError
from cetra._context import context
from cetra._datasets import Dataset, DatasetInfo, DatasetItem
from cetra._experiments import ExperimentResult, Run, Score, aevaluate, evaluate
from cetra._llm import LlmSpan, llm_span
from cetra._runtime import flush, init, shutdown, span_processor, tracer_provider
from cetra._scores import score, uuid7
from cetra._spans import Span, current_span_id, current_trace_id, observe, span

__all__ = [
    "ApiError",
    "Client",
    "ConfigError",
    "Dataset",
    "DatasetInfo",
    "DatasetItem",
    "ExperimentResult",
    "LlmSpan",
    "Run",
    "Score",
    "Span",
    "__version__",
    "aevaluate",
    "context",
    "current_span_id",
    "current_trace_id",
    "evaluate",
    "flush",
    "init",
    "llm_span",
    "observe",
    "score",
    "shutdown",
    "span",
    "span_processor",
    "tracer_provider",
    "uuid7",
]
