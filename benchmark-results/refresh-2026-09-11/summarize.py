#!/usr/bin/env python3
"""Regenerate benchmark.md and summary.json from this captured run."""
import csv
import hashlib
import json
from pathlib import Path
import re
import statistics as stats

OUT = Path(__file__).resolve().parent
ROOT = OUT.parents[1]
REL = OUT.relative_to(ROOT).as_posix()


def read_rows(name):
    with (OUT / name).open() as stream:
        return list(csv.DictReader(stream))


def median(rows, key):
    return stats.median(float(row[key]) for row in rows)


def spread(rows, key, digits=3, scale=1):
    values = [float(row[key]) / scale for row in rows]
    return f"{stats.median(values):,.{digits}f} [{min(values):,.{digits}f}–{max(values):,.{digits}f}]"


def link(filename, title=None):
    return f"[{title or filename}]({REL}/{filename})"


def scale_rows(mode, size, engine):
    return [row for file in sorted(OUT.glob(f"scale-{mode}-{size}-r*-{engine}.csv")) for row in read_rows(file.name)]


def estimates(job_prefix):
    result = {}
    for file in sorted((OUT / "criterion").glob(f"{job_prefix}-r*/**/estimates.json")):
        relative = file.relative_to(OUT / "criterion")
        # job / group / function / saved baseline / estimates.json
        key = "/".join(relative.parts[1:-2])
        result.setdefault(key, []).append(json.loads(file.read_text()))
    return result


def ns_time(value):
    return f"{value / 1e6:.3f} ms" if value >= 1e6 else f"{value / 1e3:.3f} µs"


def duration(text):
    match = re.search(r"([0-9.]+)(ns|µs|μs|ms|s)", text)
    return float(match[1]) * {"ns": 1e-9, "µs": 1e-6, "μs": 1e-6, "ms": 1e-3, "s": 1}[match[2]]


def main():
    metadata = json.loads((OUT / "metadata.json").read_text())
    jobs = json.loads((OUT / "jobs.json").read_text())
    statuses = [json.loads((OUT / (job["name"] + ".status.json")).read_text()) for job in jobs]
    assert all(status["returncode"] == 0 for status in statuses), "A failed job must be investigated before publishing"
    power = [status[point]['battery']['stdout'] for status in statuses for point in ['before','after']]
    battery = [int(match[1]) for text in power if (match := re.search(r'(\d+)%;',text))]
    changed = [name for name, expected in metadata["source_sha256"].items() if hashlib.sha256((ROOT / name).read_bytes()).hexdigest() != expected]
    assert not changed, f"Measured sources changed: {changed}"
    summary = {"metadata": metadata, "jobs_completed": len(statuses), "scale": {}, "criterion": {}, "resources": {}}
    for file in OUT.glob("*.resources.txt"):
        text = file.read_text()
        summary["resources"][file.name.removesuffix(".resources.txt")] = {
            label: int(match[1]) for label in ["maximum resident set size", "peak memory footprint"]
            if (match := re.search(r"(\d+)\s+" + label, text))
        }
    lines = []

    def paragraph(text):
        lines.extend([text.strip(), ""])

    def table(headers, rows):
        lines.append("| " + " | ".join(headers) + " |")
        lines.append("| " + " | ".join("---" for _ in headers) + " |")
        lines.extend("| " + " | ".join(map(str, row)) + " |" for row in rows)
        lines.append("")

    paragraph("# EliteSQL benchmarks — 2026-09-11")
    paragraph("Fresh measurements of the current implementation, including the integrity fixes, V3 index navigation checks, exact numeric comparisons and bounded query execution. This document replaces the previous mixed collection of current and historical results. All headline numbers below come from this new run; earlier measurements are linked separately at the end.")
    e10 = scale_rows("txn", "10m", "elitesql")
    s10 = scale_rows("txn", "10m", "sqlite")
    eb = scale_rows("bulk", "10m", "elitesql")
    sb = scale_rows("bulk", "10m", "sqlite")
    paragraph(f"At 10M rows, transactional total load took **{median(e10, 'total_load_seconds'):.3f} s** versus SQLite's **{median(s10, 'total_load_seconds'):.3f} s** ({median(e10, 'total_load_seconds') / median(s10, 'total_load_seconds'):.2f}× SQLite time). EliteSQL's direct sorted load took **{median(eb, 'total_load_seconds'):.3f} s**, **{median(sb, 'total_load_seconds') / median(eb, 'total_load_seconds'):.2f}×** SQLite's throughput in that comparison. The tables retain unfavorable results, variation and tail latency; throughput alone is insufficient to choose a workload configuration.")
    paragraph("## Source, environment and evidence")
    paragraph(f"- Source base: `{metadata['sha']}`, with uncommitted changes captured in {link('source.patch')}. The SHA alone does not reproduce this run. The source inventory covers crates, bindings, scripts, CI and Cargo files; generated reports are stored separately.\n- {metadata['cpu']['stdout']}, {metadata['logical_cpus']} logical CPUs, {int(metadata['memory']['stdout']) / 2**30:.0f} GiB RAM; {metadata['platform']}.\n- Rust/Cargo 1.93.1; benchmark profile is optimized with debug symbols, fat LTO and one codegen unit. SQLite 3.45.0 is bundled through rusqlite.\n- {len(statuses)} sequential processes completed successfully, from `{statuses[0]['before']['utc']}` to `{statuses[-1]['after']['utc']}`. Builds completed before measurement. No other benchmark or build was run concurrently.\n- Power and thermal observations are captured before and after every process. These observations do not establish CPU temperature or eliminate unrelated OS activity.\n- {link('metadata.json')} stores source/binary hashes, platform and environment overrides. {link('jobs.json')} contains every exact executable and argument list. Each job has CSV or Criterion samples, stdout, a resources file and a status record.\n- These use the normal benchmark allocator. The separate allocation-instrumented review is not mixed into these latency numbers.")
    paragraph("## Method and timing definitions")
    paragraph((f"Battery charge was {min(battery)}% at every recorded boundary. " if min(battery) == max(battery) else f"Observed battery charge ranged from {min(battery)}% to {max(battery)}%. ") + ("Every process-boundary observation reported AC power." if all("'AC Power'" in text for text in power) else "Power-source changes, if any, are retained in the status records.") + " These are boundary observations, not continuous power monitoring.")
    paragraph("Scale/bulk have three fresh runs per engine, alternating engine order. The sustained 1K-transaction workload has five. Writer and mixed/contention matrices have three runs per condition. SQL and synthetic ANN each have three fresh Criterion processes; small-transaction microbenchmarks have one process with Criterion's repeated samples. Criterion keeps its default 3 s warmup and 5 s measurement target; expensive cases may take longer. SQL/inserts set 10 samples, point reads/ANN 30.")
    paragraph("Unless stated otherwise, tables show the median across repetitions; brackets show the **minimum–maximum across runs**, not a confidence interval. Throughput ratios divide the two reported medians. A reported p99 is the median of per-run p99 values, not a percentile of pooled samples. Scale point-read and scan values are **per-operation averages**, then aggregated across runs. Criterion values are means estimated within each process; their 95% confidence intervals and raw samples are retained in the artifacts. None of these is a production latency SLO.")
    table(["Timing", "Included work"], [
        ["Ingest wall", "Staging and commits, including automatic maintenance/backpressure that overlaps ingestion"],
        ["Final checkpoint", "Explicit checkpoint after ingestion"],
        ["Maintenance drain", "Wait for pending primary-run promotion after the checkpoint"],
        ["Total load", "Ingest + final checkpoint + drain, measured as one elapsed interval"],
        ["Checkpoint/promotion work", "Accumulated background work; may overlap ingest, so do not add it to wall time"],
        ["Concurrent writes", "Timed transactions; final checkpoint is outside the throughput window and recorded separately"],
        ["Mixed throughput", "Operations divided by the whole concurrent run duration; readers/writers may finish at different times"],
    ])
    paragraph("Median stage values need not sum to the median total. Creation, fixture setup and validations follow the boundaries in the linked harnesses; whole-process resource files include those phases. Raw timings are retained so comparisons can be recalculated without inferring a different boundary.")
    paragraph("## Durability and memory profiles")
    table(["Profile", "EliteSQL", "SQLite", "Interpretation"], [
        ["Fast", "Fast", "WAL / synchronous=OFF", "No per-commit sync guarantee"],
        ["Balanced", "Balanced, default periodic sync", "WAL / synchronous=NORMAL", "Durability/performance profiles, not identical loss-window contracts"],
        ["Safe", "Safe / F_FULLFSYNC on this Mac", "FULL + fullfsync + checkpoint_fullfsync", "Strict writer comparison uses F_FULLFSYNC in both engines"],
    ])
    paragraph("Safe numbers below come from the strict concurrent-writer harness, which records and verifies the sync primitive. Fast/Balanced CSVs also label each engine's primitive, but Fast makes no timed physical sync calls; that label does not turn Fast into Safe. Scale, bulk, SQL, mixed/contention and ANN use Fast. SQLite scale disables automatic WAL checkpoints and performs its checkpoint explicitly; EliteSQL may checkpoint during ingestion, hence the separate total-load comparison.")
    paragraph("The default EliteSQL envelope is 384 MiB: 64 MiB concurrent query pool, 16 MiB working budget per query, 128 MiB mutable-index pool, 128 MiB maintenance pool and 8 MiB reserve, with headroom remaining. This is **not an RSS limit**. Mapped pages, returned rows, allocator overhead, stacks and transport memory are not equivalent to governor reservations. Frozen ownership is reported without clamping even when temporarily above its pool. RSS and physical footprint appear separately below.")
    paragraph("## Scale and direct sorted load")
    paragraph("[Harness](crates/elitesql-core/benches/scale_vs_sqlite.rs): deterministic explicit text ids and identical three-column payloads, 10K rows per transaction, 10K point reads and three full scans. Each engine runs in a separate process. EliteSQL bulk uses `bulk_insert_sorted`; SQLite retains its transaction path with the same data and batch size. Bulk is a specialized import API, not an acceleration of arbitrary transactions.")
    scale_table = []
    read_table = []
    for mode in ["txn", "bulk"]:
        for size in ["1m", "10m"]:
            e, s = scale_rows(mode, size, "elitesql"), scale_rows(mode, size, "sqlite")
            assert len(e) == len(s) == 3
            summary["scale"][f"{mode}-{size}"] = {"elitesql": e, "sqlite": s}
            scale_table.append([mode, size, spread(e, "total_load_seconds"), spread(s, "total_load_seconds"), f"{median(s,'total_load_seconds') / median(e,'total_load_seconds'):.2f}×"])
            read_table.append([mode, size, spread(e, "point_read_us"), spread(s, "point_read_us"), spread(e, "full_scan_seconds"), spread(s, "full_scan_seconds")])
    table(["Path", "Rows", "EliteSQL total s [range]", "SQLite total s [range]", "SQLite time / EliteSQL time"], scale_table)
    table(["Path", "Rows", "EliteSQL point µs [range]", "SQLite point µs [range]", "EliteSQL scan s [range]", "SQLite scan s [range]"], read_table)
    table(["10M transactional phase", "EliteSQL s [range]", "SQLite s [range]"], [[key, spread(e10,key), spread(s10,key)] for key in ["ingest_wall_seconds", "final_checkpoint_seconds", "maintenance_drain_seconds", "checkpoint_work_seconds", "promotion_work_seconds"]])
    paragraph("Point reads include an explicit 1,000-read warmup. These are warm/recently written files; no OS cache eviction is claimed. The much larger SQLite point-read latency and deferred checkpoint costs in some previous runs are not reused as current values. Every raw `scale-*.csv` is linked by " + link("jobs.json", "the run manifest") + ".")
    paragraph("## Sustained small transactions and microbenchmarks")
    sustained = {engine: [r for p in OUT.glob(f"sustained-r*-{engine}.csv") for r in read_rows(p.name)] for engine in ["elitesql", "sqlite"]}
    table(["1M rows / 1K-row batches", "EliteSQL [range]", "SQLite [range]"], [[key, spread(sustained['elitesql'],key), spread(sustained['sqlite'],key)] for key in ["ingest_wall_seconds", "final_checkpoint_seconds", "total_load_seconds", "rows_per_second"]])
    paragraph("Five fresh Fast/OFF runs use explicit ids, 100 point reads and one scan. The reported throughput is based on total load, including the final checkpoint. This avoids treating a small fresh transaction as representative of sustained ingestion.")
    micro = estimates("vs_sqlite")
    assert len(micro) == 9 and all(len(values) == 1 for values in micro.values()), 'Incomplete microbenchmark samples'
    summary["criterion"]["micro"] = micro
    table(["Criterion workload", "Mean", "95% confidence interval (within this process)"], [[key, ns_time(values[0]['mean']['point_estimate']), f"{ns_time(values[0]['mean']['confidence_interval']['lower_bound'])}–{ns_time(values[0]['mean']['confidence_interval']['upper_bound'])}"] for key, values in micro.items()])
    paragraph("[Microbenchmark harness](crates/elitesql-core/benches/vs_sqlite.rs). Insert entries time 1,000 rows; get-by-id entries time one read over 10K resident rows. Auto-generated and explicit id workloads are separate. These raw API/microbenchmark timings are not SQL or network latency.")
    paragraph("## Concurrent writers")
    paragraph("[Harness](crates/elitesql-core/benches/concurrent_writers.rs): batch size 10; 200K rows/run for Fast and Balanced, 40K for Safe; three runs per writer count. Engine order alternates internally. Writers insert disjoint ids; SQLite uses its single-writer WAL model. The workloads do not include FK, identity or secondary-index work, which is measured separately.")
    for mode in ["fast", "balanced", "safe"]:
        rows = read_rows(f"writers-{mode}.csv")
        summary["writers_"+mode] = rows
        paragraph(f"### {mode.title()}")
        result = []
        for writers in [1,2,4,8,16]:
            e = [r for r in rows if r['writers']==str(writers) and r['engine']=='EliteSQL']
            s = [r for r in rows if r['writers']==str(writers) and r['engine'].startswith('SQLite')]
            assert len(e)==len(s)==3
            result.append([writers,spread(e,'rows_per_second',0),spread(s,'rows_per_second',0),f"{median(e,'rows_per_second')/median(s,'rows_per_second'):.2f}×",f"{median(e,'p99_us'):.1f}",f"{median(s,'p99_us'):.1f}",f"{max(float(r['max_us']) for r in e)/1000:.1f}",f"{max(float(r['max_us']) for r in s)/1000:.1f}"])
        table(["Writers", "EliteSQL rows/s [range]", "SQLite rows/s [range]", "Throughput ratio", "EliteSQL p99 µs", "SQLite p99 µs", "EliteSQL worst max ms", "SQLite worst max ms"], result)
        if mode == "safe":
            table(["Writers", "EliteSQL commits/sync [range]"], [[w,spread([r for r in rows if r['writers']==str(w) and r['engine']=='EliteSQL'],'commits_per_sync',2)] for w in [1,2,4,8,16]])
        paragraph(link(f"writers-{mode}.csv", "Raw rows and lock/sync counters") + ". Maxima are the worst recorded transaction across repetitions; p99 can hide writer starvation affecting fewer than 1% of transactions. Group commit depends on concurrency and scheduling; a ratio measured here is not a guarantee at a given connection count.")
    paragraph("## Persisted reads with concurrent writes")
    mixed = read_rows("mixed.csv")
    summary["mixed"] = mixed
    table(["Readers", "Writers", "Reads/s", "Writes/s", "Read p99 µs", "Write p99 µs"], [[r,w,*[f"{median([x for x in mixed if x['readers']==str(r) and x['writers']==str(w)],k):,.1f}" for k in ['reads_per_second','writes_per_second','read_p99_us','write_p99_us']]] for r in [1,2,4,8,16] for w in [0,1,4]])
    paragraph("100K persisted fixture rows, 1M point reads and 40K inserted rows in mixed runs, batch size 10, Fast. Zero writers isolates reads. Validation and its scan run outside the timed window. " + link("mixed.csv", "Raw repetitions and tails") + "; [harness](crates/elitesql-core/benches/concurrent_rw.rs).")
    paragraph("## Mutations, identities, foreign keys and derived indexes")
    contention = read_rows("contention.csv")
    summary["contention"] = contention
    table(["Profile", "Cache preparation", "Reads/s", "Writes/s", "Read p99 µs", "Write p99 µs"], [[workload,"warm" if cache=='warm' else 'reopened',*[f"{median([x for x in contention if x['workload']==workload and x['cache']==cache],k):,.1f}" for k in ['reads_per_second','writes_per_second','read_p99_us','write_p99_us']]] for workload in ['insert','update','delete','identity','foreign-key','derived'] for cache in ['warm','cold']])
    paragraph("16 readers, four writers, 50K persisted rows, 100K reads, 5K mutations, batch size 10, three runs per condition. The raw cache label `cold` means reopen without warmup: this Mac reports eviction counters 0/0. It **does not mean disk-cold I/O**. Derived is a different workload with more per-write index work; the difference from insert does not isolate one index's overhead. " + link("contention.csv") + "; [harness](crates/elitesql-core/benches/contention_matrix.rs).")
    paragraph("## SQL and bound parameters")
    sql = estimates("sql")
    assert len(sql) == 7 and all(len(values) == 3 for values in sql.values()), 'Incomplete SQL samples'
    summary["criterion"]["sql"] = sql
    table(["Query", "Median estimated mean", "Range of process means"], [[key,ns_time(stats.median(v['mean']['point_estimate'] for v in values)),f"{ns_time(min(v['mean']['point_estimate'] for v in values))}–{ns_time(max(v['mean']['point_estimate'] for v in values))}"] for key,values in sql.items()])
    paragraph("[Harness](crates/elitesql-core/benches/sql.rs): 1M orders and 10K users, with unique email and user-id indexes. Timings include parsing, planning and execution. The indexed join matches about 100 orders and returns the top 10. `full_scan_filter_1m` asks for **LIMIT 5** and may stop early: the name identifies fixture size, not a full-table traversal. GROUP BY cases consume the 1M rows. Bound and literal variants use the same result shape; differences this small require the retained confidence intervals.")
    paragraph("## Synthetic ANN: 100K vectors")
    vector = estimates("vector")
    assert len(vector) == 4 and all(len(values) == 3 for values in vector.values()), 'Incomplete ANN samples'
    summary["criterion"]["vector"] = vector
    recalls = {}
    ingest, reopen = [], []
    for i in range(1,4):
        text = (OUT / f"vector-r{i}.log").read_text()
        for ef,recall in re.findall(r"ef_search=(\d+)\): ([0-9.]+)",text):
            recalls.setdefault(ef,[]).append(float(recall))
        ingest.append(duration(re.search(r"ANN indexed ingest: (.+)",text)[1]))
        reopen.append(duration(re.search(r"open with persisted graph .*: (.+)",text)[1]))
    table(["ef_search", "Recall@10 range", "Median search mean", "Search mean range"], [[ef,f"{min(recalls[str(ef)]):.4f}–{max(recalls[str(ef)]):.4f}",ns_time(stats.median(v['mean']['point_estimate'] for v in vector[f'vector_100k/search_top10_ef{ef}'])),f"{ns_time(min(v['mean']['point_estimate'] for v in vector[f'vector_100k/search_top10_ef{ef}']))}–{ns_time(max(v['mean']['point_estimate'] for v in vector[f'vector_100k/search_top10_ef{ef}']))}"] for ef in [64,128,256,512]])
    summary['ann'] = dict(recall=recalls,ingest_seconds=ingest,reopen_seconds=reopen)
    paragraph(f"Indexed ingest: **{stats.median(ingest):.3f} s** [{min(ingest):.3f}–{max(ingest):.3f}]. Open with the persisted graph: **{stats.median(reopen)*1000:.1f} ms** [{min(reopen)*1000:.1f}–{max(reopen)*1000:.1f}]. All recall gates passed in all three fresh builds.")
    paragraph("[Harness](crates/elitesql-core/benches/vector.rs): dimension 64, 1,024 deterministic clusters, noise 0.6, cosine top-10. Quality compares against brute force for 50 fixed queries; Criterion samples independently generated queries and includes their generation in the timed loop. Construction uses m=16 and ef_construction=200, with the normal profile rather than the monolithic override. This synthetic corpus does not predict semantic retrieval quality or high-dimensional production memory needs. The previous Potion/MIRACL 250K run is historical and was not rerun for this refresh.")
    paragraph("## WAL preallocation diagnostic")
    wal = read_rows("wal-preallocation.csv")
    summary['wal_preallocation'] = wal
    table(["Mode", "Sync p50 µs [range]", "Sync p95 µs [range]"], [[mode,spread([r for r in wal if r['mode']==mode],'p50_us',1),spread([r for r in wal if r['mode']==mode],'p95_us',1)] for mode in sorted({r['mode'] for r in wal})])
    paragraph("Five paired repetitions, alternating mode, 100 writes of one 4 KiB frame per mode; the preallocated file reserves 64 MiB before timing. This measures `File::sync_data` on this storage, not SQL commit throughput or a shipped WAL-preallocation feature. " + link("wal-preallocation.csv") + ".")
    paragraph("## Observed process memory")
    memory_rows=[]
    for prefix in ['scale-txn-1m','scale-txn-10m','scale-bulk-10m']:
        for engine in ['elitesql','sqlite']:
            values=[v for k,v in summary['resources'].items() if k.startswith(prefix+'-r') and k.endswith('-'+engine)]
            memory_rows.append([prefix,engine,spread(values,'maximum resident set size',1,2**20),spread(values,'peak memory footprint',1,2**20)])
    values=[v for k,v in summary['resources'].items() if re.fullmatch('vector-r[123]',k)]
    memory_rows.append(['ANN including ground truth','elitesql',spread(values,'maximum resident set size',1,2**20),spread(values,'peak memory footprint',1,2**20)])
    table(["Whole process", "Engine", "Peak RSS MiB [range]", "Peak physical footprint MiB [range]"],memory_rows)
    paragraph("Values come from `/usr/bin/time -l`. Each peak covers setup, workload, validation and cleanup; the ANN process also retains original vectors and brute-force ground truth. These are not engine-only heap measurements and exclude system-wide filesystem cache; RSS ratios therefore do not compare total host-memory use between engines. Clean mapped pages can contribute to RSS while remaining reclaimable. The configured 384 MiB envelope must not be advertised as a hard process-memory maximum.")
    paragraph("## Reproducing and inspecting this run")
    paragraph("```bash\n# Build first; do not measure while the compiler is running.\ncargo bench --locked -p elitesql-core --no-run --message-format=json \\\n  > /tmp/elitesql-bench-build.jsonl\npython3 scripts/refresh-benchmarks.py \\\n  --build-json /tmp/elitesql-bench-build.jsonl \\\n  --output benchmark-results/local-refresh\n```\n\nUse a new output directory. `--resume` skips successful jobs only when source and binary hashes still match. This archived run's " + link('jobs.json','job manifest') + " is the authoritative complete command matrix. Every measurement uses temporary benchmark databases; the runner does not publish or commit repository changes.")
    paragraph("For this captured dataset, `python3 " + REL + "/summarize.py` regenerates this document and " + link('summary.json') + ". It verifies successful statuses and unchanged measured source hashes. To reproduce the measured dirty tree elsewhere, start at the recorded base SHA and apply " + link('source.patch') + " before building. Criterion samples/estimates are preserved under " + link('criterion/', 'criterion/') + "; their timestamps and status files distinguish this run from earlier artifacts.")
    paragraph("## Interpretation, limitations and historical results")
    paragraph("- Transactional load and direct sorted load have different APIs and preconditions; do not apply the bulk speedup to arbitrary transactions.\n- High writer throughput can coexist with latency tails or starvation. Compare p99, maxima, checkpoint costs and durability together.\n- The runs use one laptop, deterministic fixtures and filesystem caches that are not forcibly cold. They do not characterize long-duration service behavior, network/client overhead, all data widths or every selectivity.\n- Synthetic ANN recall measures approximation of exact vector neighbors. It is not a relevance benchmark, and the 64D result cannot establish 256D/768D memory capacity.\n- CI/functional acceptance and crash-recovery tests are documented in the implementation report; benchmark success is not a proof of every integrity invariant or power-loss scenario.")
    paragraph("Historical material: [archived September 5 benchmark document](benchmark-results/archive/benchmark-2026-09-05.md), [September 5 acceptance](benchmark-results/current-acceptance-2026-09-05.md), [September 4 acceptance](benchmark-results/current-acceptance-2026-09-04.md), [August 23 acceptance](benchmark-results/current-acceptance-2026-08-23.md), and [the separate September 11 instrumented integrity/performance comparison](benchmark-results/review-2026-09-11/README.md). Those figures are not relabelled as fresh measurements. [Implementation and validation](docs/implementacion-plan.md).")
    (OUT / 'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
    (ROOT / 'benchmark.md').write_text('\n'.join(lines))
    print(f"Wrote benchmark.md from {len(statuses)} successful jobs")


if __name__ == '__main__':
    main()
