#!/usr/bin/env python3
"""Evaluate Model2Vec embeddings with EliteSQL on the Spanish MIRACL corpus."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import math
import shutil
import statistics
import sys
import time
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

import numpy as np
from huggingface_hub import snapshot_download
from model2vec import StaticModel


REPO_ROOT = Path(__file__).resolve().parents[2]
PYTHON_BINDING = REPO_ROOT / "bindings" / "python"
if str(PYTHON_BINDING) not in sys.path:
    sys.path.insert(0, str(PYTHON_BINDING))

from elitesql import EliteSQL  # noqa: E402


MODEL_ID = "minishlab/potion-multilingual-128M"
MODEL_REVISION = "73908c3438cf03b6a01bcb9611d62b23d0726f08"
MODEL_DIM = 256
DATASET_ID = "jinaai/miracl-es"
DATASET_REVISION = "c2c5c2776af79b9d0a831a44195573a5d7213d63"
DATASET_FILE = "test.jsonl.gz"
DATASET_SHA256 = "43fd2f9787710ac4c4dac622a55871f0a3bbb37198ab3abc514ddd8942ad183b"
DATASET_URL = (
    f"https://huggingface.co/datasets/{DATASET_ID}/resolve/"
    f"{DATASET_REVISION}/{DATASET_FILE}"
)
CORPUS_ID = "miracl/miracl-corpus"
CORPUS_REVISION = "d921ec7e349ce0d28daf30b2da9da5ee698bef0d"
CORPUS_FILE = "miracl-corpus-v1.0-es/docs-0.jsonl.gz"
CORPUS_SHA256 = "e261da2adb2a5d817756dc6d9f977ab70d5ee350faad7b6e129531413d8fae89"
CORPUS_URL = (
    f"https://huggingface.co/datasets/{CORPUS_ID}/resolve/"
    f"{CORPUS_REVISION}/{CORPUS_FILE}"
)


@dataclass(frozen=True)
class QueryCase:
    query: str
    positives: tuple[str, ...]
    negatives: tuple[str, ...]


@dataclass
class RankingMetrics:
    count: int = 0
    hit_at_k: float = 0.0
    recall_at_k: float = 0.0
    reciprocal_rank: float = 0.0
    ndcg_at_k: float = 0.0

    def add(self, ranked_ids: Sequence[str], relevant_ids: set[str]) -> None:
        self.count += 1
        relevance = [doc_id in relevant_ids for doc_id in ranked_ids]
        relevant_found = sum(relevance)
        self.hit_at_k += float(relevant_found > 0)
        self.recall_at_k += relevant_found / len(relevant_ids)
        first = next((i for i, is_relevant in enumerate(relevance) if is_relevant), None)
        self.reciprocal_rank += 0.0 if first is None else 1.0 / (first + 1)
        dcg = sum(1.0 / math.log2(i + 2) for i, rel in enumerate(relevance) if rel)
        ideal_count = min(len(relevant_ids), len(ranked_ids))
        ideal_dcg = sum(1.0 / math.log2(i + 2) for i in range(ideal_count))
        self.ndcg_at_k += dcg / ideal_dcg if ideal_dcg else 0.0

    def means(self) -> dict[str, float]:
        if not self.count:
            return {"hit_rate": 0.0, "recall": 0.0, "mrr": 0.0, "ndcg": 0.0}
        return {
            "hit_rate": self.hit_at_k / self.count,
            "recall": self.recall_at_k / self.count,
            "mrr": self.reciprocal_rank / self.count,
            "ndcg": self.ndcg_at_k / self.count,
        }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark potion-multilingual-128M + EliteSQL with MIRACL-es."
    )
    parser.add_argument(
        "--db",
        type=Path,
        default=REPO_ROOT / "target" / "potion-miracl-es.esql",
        help="EliteSQL database path (default: target/potion-miracl-es.esql)",
    )
    parser.add_argument(
        "--data-cache",
        type=Path,
        default=REPO_ROOT / "target" / "potion-miracl-es" / DATASET_FILE,
        help="Location of the pinned dataset download",
    )
    parser.add_argument(
        "--corpus-cache",
        type=Path,
        default=REPO_ROOT / "target" / "potion-miracl-es" / "docs-0.jsonl.gz",
        help="Location of the pinned MIRACL-es corpus shard",
    )
    parser.add_argument(
        "--corpus-size",
        type=int,
        default=0,
        help="Expand to N passages from MIRACL-es; 0 uses only judged candidates",
    )
    parser.add_argument(
        "--max-queries",
        type=int,
        default=0,
        help="Use only the first N queries; 0 uses all 648",
    )
    parser.add_argument("--top-k", type=int, default=10)
    parser.add_argument("--ef-search", type=int, default=128)
    parser.add_argument("--embedding-batch-size", type=int, default=1024)
    parser.add_argument("--insert-batch-size", type=int, default=64)
    parser.add_argument(
        "--total-memory-mib",
        type=int,
        default=128,
        help="EliteSQL total memory envelope in MiB (default: 128)",
    )
    parser.add_argument(
        "--maintenance-memory-mib",
        type=int,
        default=32,
        help="EliteSQL maintenance pool in MiB; HNSW build must fit it (default: 32)",
    )
    parser.add_argument(
        "--quantized-index",
        action="store_true",
        help="Store the HNSW vectors as int8 (canonical vectors remain float32)",
    )
    parser.add_argument(
        "--rebuild",
        action="store_true",
        help="Delete and rebuild the generated database at --db",
    )
    parser.add_argument(
        "--output-json",
        type=Path,
        help="Optionally save the metrics as JSON",
    )
    args = parser.parse_args()
    if args.max_queries < 0 or args.corpus_size < 0:
        parser.error("--max-queries and --corpus-size must be >= 0")
    if args.top_k < 1 or args.ef_search < 1:
        parser.error("--top-k and --ef-search must be >= 1")
    if args.embedding_batch_size < 1 or args.insert_batch_size < 1:
        parser.error("batch sizes must be >= 1")
    if args.total_memory_mib < 1 or args.maintenance_memory_mib < 1:
        parser.error("memory sizes must be >= 1 MiB")
    return args


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download_file(path: Path, url: str, expected_sha256: str, label: str) -> None:
    if path.is_file() and file_sha256(path) == expected_sha256:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".part")
    print(f"Descargando {label} ...")
    try:
        with urllib.request.urlopen(url) as response, temporary.open("wb") as output:
            shutil.copyfileobj(response, output)
        actual_hash = file_sha256(temporary)
        if actual_hash != expected_sha256:
            raise RuntimeError(
                f"checksum inválido para {label}: {actual_hash} != {expected_sha256}"
            )
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def load_cases(path: Path, max_queries: int) -> list[QueryCase]:
    # This particular artifact concatenates JSON objects without newlines even
    # though its filename says jsonl, so parse it as a stream of JSON values.
    with gzip.open(path, "rt", encoding="utf-8") as stream:
        payload = stream.read()
    decoder = json.JSONDecoder()
    offset = 0
    cases: list[QueryCase] = []
    while offset < len(payload):
        while offset < len(payload) and payload[offset].isspace():
            offset += 1
        if offset == len(payload):
            break
        row, offset = decoder.raw_decode(payload, offset)
        positives = tuple(dict.fromkeys(row["positive"]))
        negatives = tuple(text for text in dict.fromkeys(row["negative"]) if text not in positives)
        if positives:
            cases.append(QueryCase(row["query"], positives, negatives))
        if max_queries and len(cases) >= max_queries:
            break
    if not cases:
        raise RuntimeError("el dataset no contiene consultas evaluables")
    return cases


def stable_doc_id(text: str) -> str:
    return "doc-" + hashlib.sha256(text.encode("utf-8")).hexdigest()[:32]


def collect_documents(cases: Sequence[QueryCase]) -> tuple[list[str], dict[str, int]]:
    documents: list[str] = []
    document_index: dict[str, int] = {}
    for case in cases:
        for text in (*case.positives, *case.negatives):
            if text not in document_index:
                document_index[text] = len(documents)
                documents.append(text)
    return documents, document_index


def expand_documents_from_corpus(
    documents: list[str], document_index: dict[str, int], corpus_path: Path, target: int
) -> None:
    if not target:
        return
    if target < len(documents):
        raise RuntimeError(
            f"--corpus-size={target} es menor que los {len(documents)} candidatos evaluados"
        )
    with gzip.open(corpus_path, "rt", encoding="utf-8") as stream:
        for line in stream:
            row = json.loads(line)
            text = row["text"]
            if text and text not in document_index:
                document_index[text] = len(documents)
                documents.append(text)
                if len(documents) >= target:
                    break
    if len(documents) < target:
        raise RuntimeError(
            f"el fragmento descargado solo permitió reunir {len(documents)} pasajes"
        )


def database_fingerprint(documents: Sequence[str], quantized: bool) -> str:
    digest = hashlib.sha256()
    for value in (
        MODEL_ID,
        MODEL_REVISION,
        DATASET_ID,
        DATASET_REVISION,
        CORPUS_ID,
        CORPUS_REVISION,
        str(quantized),
    ):
        digest.update(value.encode("utf-8"))
        digest.update(b"\0")
    for text in documents:
        digest.update(stable_doc_id(text).encode("ascii"))
        digest.update(b"\0")
    return digest.hexdigest()


def encode_normalized(
    model: StaticModel, texts: Sequence[str], batch_size: int, label: str
) -> tuple[np.ndarray, float]:
    started = time.perf_counter()
    vectors = model.encode(
        texts,
        batch_size=batch_size,
        max_length=None,
        show_progress_bar=True,
        use_multiprocessing=False,
    ).astype(np.float32, copy=False)
    norms = np.linalg.norm(vectors, axis=1, keepdims=True)
    vectors = vectors / np.maximum(norms, np.finfo(np.float32).eps)
    elapsed = time.perf_counter() - started
    print(f"Embeddings de {label}: {len(texts)} en {elapsed:.3f}s")
    return vectors, elapsed


def sql_string(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def remove_generated_db(path: Path) -> None:
    if not path.exists():
        return
    if path.is_dir():
        shutil.rmtree(path)
    else:
        path.unlink()


def insert_documents(
    db: EliteSQL,
    documents: Sequence[str],
    embeddings: np.ndarray,
    batch_size: int,
) -> None:
    next_progress = 0
    progress_step = max(batch_size, len(documents) // 100)
    for start in range(0, len(documents), batch_size):
        values = []
        for text, vector in zip(
            documents[start : start + batch_size], embeddings[start : start + batch_size]
        ):
            vector_json = json.dumps(vector.tolist(), separators=(",", ":"))
            values.append(
                f"({sql_string(stable_doc_id(text))},"
                f"{sql_string(text)},{sql_string(vector_json)})"
            )
        db.query(
            "INSERT INTO passages (id, body, embedding) VALUES " + ",".join(values)
        )
        done = min(start + batch_size, len(documents))
        if done >= next_progress or done == len(documents):
            print(f"\rInsertando en EliteSQL: {done}/{len(documents)}", end="", flush=True)
            next_progress = done + progress_step
    print()


def open_or_build_db(
    path: Path,
    documents: Sequence[str],
    embeddings: np.ndarray,
    insert_batch_size: int,
    quantized: bool,
    rebuild: bool,
    memory: dict[str, int],
) -> tuple[EliteSQL, dict[str, float | bool]]:
    expected_fingerprint = database_fingerprint(documents, quantized)
    if rebuild:
        remove_generated_db(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    existed = path.exists()
    db = EliteSQL(path, durability="fast", memory=memory)
    timings: dict[str, float | bool] = {"database_reused": existed}
    if existed:
        try:
            result = db.query("SELECT fingerprint FROM benchmark_meta LIMIT 1")
            actual_fingerprint = result["rows"][0][0] if result["rows"] else None
        except Exception as error:
            db.close()
            raise RuntimeError(
                f"{path} no es una base válida de este benchmark; use --rebuild"
            ) from error
        if actual_fingerprint != expected_fingerprint:
            db.close()
            raise RuntimeError(
                f"{path} fue creada con otro corpus, modelo o tipo de índice; use --rebuild"
            )
        return db, timings

    started = time.perf_counter()
    db.query(
        f"CREATE TABLE passages (body text NOT NULL, embedding vector({MODEL_DIM}) NOT NULL)"
    )
    db.query("CREATE TABLE benchmark_meta (fingerprint text NOT NULL)")
    insert_documents(db, documents, embeddings, insert_batch_size)
    timings["insert_seconds"] = time.perf_counter() - started

    started = time.perf_counter()
    db.create_vector_index(
        "passages", "embedding", metric="cosine", quantized=quantized
    )
    timings["index_seconds"] = time.perf_counter() - started
    db.query(
        "INSERT INTO benchmark_meta (id, fingerprint) VALUES "
        f"('configuration', {sql_string(expected_fingerprint)})"
    )
    return db, timings


def top_indices(scores: np.ndarray, k: int) -> np.ndarray:
    k = min(k, scores.shape[0])
    if k == scores.shape[0]:
        return np.argsort(-scores)
    selected = np.argpartition(scores, -k)[-k:]
    return selected[np.argsort(-scores[selected])]


def evaluate_reranking(
    cases: Sequence[QueryCase],
    query_vectors: np.ndarray,
    document_vectors: np.ndarray,
    document_index: dict[str, int],
    top_k: int,
) -> RankingMetrics:
    metrics = RankingMetrics()
    for case, query_vector in zip(cases, query_vectors):
        candidate_texts = list(dict.fromkeys((*case.positives, *case.negatives)))
        candidate_indices = np.asarray([document_index[text] for text in candidate_texts])
        scores = document_vectors[candidate_indices] @ query_vector
        ranked = top_indices(scores, top_k)
        ranked_ids = [stable_doc_id(candidate_texts[i]) for i in ranked]
        relevant_ids = {stable_doc_id(text) for text in case.positives}
        metrics.add(ranked_ids, relevant_ids)
    return metrics


def evaluate_elitesql(
    db: EliteSQL,
    cases: Sequence[QueryCase],
    query_vectors: np.ndarray,
    documents: Sequence[str],
    document_vectors: np.ndarray,
    top_k: int,
    ef_search: int,
) -> tuple[RankingMetrics, RankingMetrics, float, list[float]]:
    exact_metrics = RankingMetrics()
    ann_metrics = RankingMetrics()
    ann_overlap = 0.0
    latencies_ms: list[float] = []

    document_ids = np.asarray([stable_doc_id(text) for text in documents])
    for case, query_vector in zip(cases, query_vectors):
        exact_idx = top_indices(document_vectors @ query_vector, top_k)
        exact_ids = document_ids[exact_idx].tolist()
        relevant_ids = {stable_doc_id(text) for text in case.positives}
        exact_metrics.add(exact_ids, relevant_ids)

        started = time.perf_counter_ns()
        hits = db.search_vector(
            "passages",
            "embedding",
            query_vector.tolist(),
            top_k=top_k,
            ef_search=ef_search,
        )
        latencies_ms.append((time.perf_counter_ns() - started) / 1_000_000)
        ann_ids = [hit["id"] for hit in hits]
        ann_metrics.add(ann_ids, relevant_ids)
        ann_overlap += len(set(ann_ids) & set(exact_ids)) / len(exact_ids)

    return exact_metrics, ann_metrics, ann_overlap / len(cases), latencies_ms


def percentile(values: Sequence[float], percentile_value: float) -> float:
    return float(np.percentile(np.asarray(values), percentile_value))


def print_metric_line(label: str, metrics: RankingMetrics, top_k: int) -> None:
    mean = metrics.means()
    print(
        f"{label:<29} Hit@{top_k}={mean['hit_rate']:.4f}  "
        f"Recall@{top_k}={mean['recall']:.4f}  MRR@{top_k}={mean['mrr']:.4f}  "
        f"nDCG@{top_k}={mean['ndcg']:.4f}"
    )


def main() -> None:
    args = parse_args()
    download_file(
        args.data_cache,
        DATASET_URL,
        DATASET_SHA256,
        f"{DATASET_ID}@{DATASET_REVISION[:12]}",
    )
    cases = load_cases(args.data_cache, args.max_queries)
    documents, document_index = collect_documents(cases)
    if args.corpus_size:
        download_file(
            args.corpus_cache,
            CORPUS_URL,
            CORPUS_SHA256,
            f"{CORPUS_ID}@{CORPUS_REVISION[:12]} (fragmento es)",
        )
        expand_documents_from_corpus(
            documents, document_index, args.corpus_cache, args.corpus_size
        )
    print(f"Corpus: {len(documents)} pasajes únicos; consultas: {len(cases)}")

    started = time.perf_counter()
    model_path = snapshot_download(
        repo_id=MODEL_ID,
        revision=MODEL_REVISION,
        allow_patterns=["README.md", "config.json", "model.safetensors", "tokenizer.json"],
    )
    model = StaticModel.from_pretrained(model_path, normalize=True, force_download=False)
    model_load_seconds = time.perf_counter() - started
    document_vectors, document_embedding_seconds = encode_normalized(
        model, documents, args.embedding_batch_size, "pasajes"
    )
    query_vectors, query_embedding_seconds = encode_normalized(
        model, [case.query for case in cases], args.embedding_batch_size, "consultas"
    )
    if document_vectors.shape[1] != MODEL_DIM:
        raise RuntimeError(
            f"{MODEL_ID} produjo dimensión {document_vectors.shape[1]}, se esperaba {MODEL_DIM}"
        )

    db, build_timings = open_or_build_db(
        args.db,
        documents,
        document_vectors,
        args.insert_batch_size,
        args.quantized_index,
        args.rebuild,
        {
            "total_memory_bytes": args.total_memory_mib * 1024 * 1024,
            "maintenance_pool_bytes": args.maintenance_memory_mib * 1024 * 1024,
        },
    )
    try:
        reranking = evaluate_reranking(
            cases,
            query_vectors,
            document_vectors,
            document_index,
            args.top_k,
        )
        exact, ann, overlap, latencies = evaluate_elitesql(
            db,
            cases,
            query_vectors,
            documents,
            document_vectors,
            args.top_k,
            args.ef_search,
        )
    finally:
        db.close()

    print("\nResultados")
    print_metric_line("Reranking MIRACL (exacto)", reranking, args.top_k)
    print_metric_line("Corpus global (exacto)", exact, args.top_k)
    print_metric_line("Corpus global (EliteSQL)", ann, args.top_k)
    print(f"EliteSQL vs exacto             ANN recall@{args.top_k}={overlap:.4f}")
    print(
        "Latencia EliteSQL              "
        f"media={statistics.fmean(latencies):.3f}ms  "
        f"p50={percentile(latencies, 50):.3f}ms  "
        f"p95={percentile(latencies, 95):.3f}ms  "
        f"p99={percentile(latencies, 99):.3f}ms"
    )

    result = {
        "model": MODEL_ID,
        "model_revision": MODEL_REVISION,
        "model_dimension": MODEL_DIM,
        "dataset": DATASET_ID,
        "dataset_revision": DATASET_REVISION,
        "corpus": CORPUS_ID,
        "corpus_revision": CORPUS_REVISION,
        "documents": len(documents),
        "queries": len(cases),
        "top_k": args.top_k,
        "ef_search": args.ef_search,
        "quantized_index": args.quantized_index,
        "memory": {
            "total_memory_mib": args.total_memory_mib,
            "maintenance_memory_mib": args.maintenance_memory_mib,
        },
        "model_load_seconds": model_load_seconds,
        "embedding_seconds": {
            "documents": document_embedding_seconds,
            "queries": query_embedding_seconds,
        },
        "database": {"path": str(args.db), **build_timings},
        "reranking": reranking.means(),
        "pooled_exact": exact.means(),
        "pooled_elitesql": ann.means(),
        "ann_recall": overlap,
        "latency_ms": {
            "mean": statistics.fmean(latencies),
            "p50": percentile(latencies, 50),
            "p95": percentile(latencies, 95),
            "p99": percentile(latencies, 99),
        },
    }
    if args.output_json:
        args.output_json.parent.mkdir(parents=True, exist_ok=True)
        args.output_json.write_text(
            json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        print(f"Métricas JSON: {args.output_json}")


if __name__ == "__main__":
    main()
