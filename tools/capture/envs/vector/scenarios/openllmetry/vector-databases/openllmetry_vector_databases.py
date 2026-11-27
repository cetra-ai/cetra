"""OpenLLMetry's vector instrumentors: add, query and delete on Chroma, Qdrant and Milvus Lite."""

from __future__ import annotations

import os
import shutil
import tempfile
from collections.abc import Sequence

import chromadb
from chromadb.config import Settings
from opentelemetry.instrumentation.chromadb import ChromaInstrumentor
from opentelemetry.instrumentation.milvus import MilvusInstrumentor
from opentelemetry.instrumentation.qdrant import QdrantInstrumentor
from pymilvus import MilvusClient
from qdrant_client import QdrantClient
from qdrant_client.models import Distance, PointStruct, VectorParams

from capture_py.harness import finish, setup

COLLECTION = "docs"
# chromadb's embeddings parameters are invariant: the values carry these exact types.
CHROMA_EMBEDDINGS: list[Sequence[float] | Sequence[int]] = [[0.1, 0.2], [0.3, 0.4]]
CHROMA_QUERY: list[Sequence[float] | Sequence[int]] = [[0.15, 0.25]]


def chroma_calls() -> None:
    """Add, query and delete on Chroma's in-memory client."""
    client = chromadb.EphemeralClient(settings=Settings(anonymized_telemetry=False))
    collection = client.get_or_create_collection(COLLECTION)
    collection.add(ids=["a", "b"], documents=["sunny", "rainy"], embeddings=CHROMA_EMBEDDINGS)
    collection.query(query_embeddings=CHROMA_QUERY, n_results=1)
    collection.delete(ids=["a"])


def qdrant_calls() -> None:
    """Add, query and delete on Qdrant's in-memory local client."""
    client = QdrantClient(":memory:")
    client.create_collection(
        COLLECTION, vectors_config=VectorParams(size=2, distance=Distance.COSINE)
    )
    client.upsert(
        COLLECTION,
        points=[
            PointStruct(id=1, vector=[0.1, 0.2], payload={"text": "sunny"}),
            PointStruct(id=2, vector=[0.3, 0.4], payload={"text": "rainy"}),
        ],
    )
    client.query_points(COLLECTION, query=[0.15, 0.25], limit=1)
    client.delete(COLLECTION, points_selector=[1, 2])


def milvus_calls() -> None:
    """Add, query and delete on a Milvus Lite file database."""
    path = os.path.join(tempfile.gettempdir(), "cetra-capture-openllmetry-milvus.db")
    # Milvus Lite writes a directory at the path, not a file.
    if os.path.isdir(path):
        shutil.rmtree(path)
    elif os.path.exists(path):
        os.remove(path)
    client = MilvusClient(path)
    client.create_collection(COLLECTION, dimension=2)
    client.insert(
        COLLECTION,
        data=[
            {"id": 1, "vector": [0.1, 0.2], "text": "sunny"},
            {"id": 2, "vector": [0.3, 0.4], "text": "rainy"},
        ],
    )
    client.search(COLLECTION, data=[[0.15, 0.25]], limit=1)
    client.delete(COLLECTION, ids=[1, 2])


def main() -> None:
    """Instrument the three clients, then run the three operations on each database."""
    provider = setup()
    # The instrumentor constructors are untyped (bare __init__(self, exception_logger=None)).
    ChromaInstrumentor().instrument(tracer_provider=provider)  # type: ignore[no-untyped-call]
    QdrantInstrumentor().instrument(tracer_provider=provider)  # type: ignore[no-untyped-call]
    MilvusInstrumentor().instrument(tracer_provider=provider)  # type: ignore[no-untyped-call]
    chroma_calls()
    qdrant_calls()
    milvus_calls()
    finish(provider)


if __name__ == "__main__":
    main()
