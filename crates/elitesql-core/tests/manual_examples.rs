//! The examples printed in `manual.md`, run verbatim.
//!
//! Documentation that claims an output or an error message is a promise. This
//! suite keeps those promises honest: the snippets here are the ones a reader
//! copies, and the assertions are the values and messages the manual shows.

use elitesql_core::{
    Db, IndexingMode, QueryOutput, Record, Value, VectorIndexOptions, VectorMetric,
    VectorSearchOptions,
};

fn new_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("app.esql")).unwrap();
    (dir, db)
}

/// manual.md — "Vectors: storing and searching embeddings", steps 1 to 4.
#[test]
fn vectors_declare_insert_and_search() {
    let (_d, db) = new_db();

    // 1. Declare the column and index it.
    db.query(
        "CREATE TABLE docs (
           title     text NOT NULL,
           lang      text,
           embedding vector(4)
         )",
    )
    .unwrap();
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
        .unwrap();

    // 2. Insert a vector: the SQL literal is a JSON array inside a string.
    db.query(
        "INSERT INTO docs (title, lang, embedding)
         VALUES ('hello', 'en', '[0.1, 0.2, 0.3, 0.4]')",
    )
    .unwrap();
    db.query(
        "INSERT INTO docs (title, lang, embedding) VALUES
           ('hola',    'es', '[0.11, 0.19, 0.31, 0.39]'),
           ('bonjour', 'fr', '[-0.9, 0.05, 0.2, 0.1]')",
    )
    .unwrap();

    // The two errors the manual quotes for a bad vector.
    let err = db
        .query("INSERT INTO docs (title, embedding) VALUES ('bad', '[1.0, 2.0]')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("expects vector<float32, 4>, got dimension 2"),
        "unexpected message: {err}"
    );
    let err = db
        .query("INSERT INTO docs (title, embedding) VALUES ('bad', 'nope')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("invalid vector literal for 'embedding' (expected a JSON array of numbers)"),
        "unexpected message: {err}"
    );

    // The API path: the embedding arrives from a model as a Vec<f32>.
    let embedding: Vec<f32> = vec![0.09, 0.21, 0.29, 0.41];
    let mut record = Record::new();
    record.insert("title".into(), Value::Text("guten tag".into()));
    record.insert("lang".into(), Value::Text("de".into()));
    record.insert("embedding".into(), Value::Vector(embedding));
    db.insert("docs", record).unwrap();

    // 3. Search: closest first, and the manual's ranking.
    let query = [0.1f32, 0.2, 0.3, 0.4];
    let hits = db
        .search_vector(
            "docs",
            "embedding",
            &query,
            3,
            &VectorSearchOptions::default(),
        )
        .unwrap();
    let titles: Vec<&Value> = hits.iter().map(|h| &h.record["title"]).collect();
    assert_eq!(
        titles,
        vec![
            &Value::Text("hello".into()),
            &Value::Text("guten tag".into()),
            &Value::Text("hola".into()),
        ]
    );
    // Cosine distance: 0 is identical, and the order is by increasing distance.
    assert!(
        hits[0].distance < 0.0001,
        "distance was {}",
        hits[0].distance
    );
    assert!(hits.windows(2).all(|w| w[0].distance <= w[1].distance));
    assert!(hits.iter().all(|h| !h.id.is_empty()));

    // 4. Filter by metadata: still up to top_k hits, only matching records.
    let mut filter = Record::new();
    filter.insert("lang".into(), Value::Text("es".into()));
    let hits = db
        .search_vector(
            "docs",
            "embedding",
            &query,
            5,
            &VectorSearchOptions {
                ef_search: Some(128),
                filter: Some(filter),
            },
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record["title"], Value::Text("hola".into()));
}

/// manual.md — "Vectors", step 5: index options, async indexing, drop.
#[test]
fn vector_index_options_async_and_drop() {
    let (_d, db) = new_db();
    db.query("CREATE TABLE chunks (body text, embedding vector(4))")
        .unwrap();
    db.create_vector_index(
        "chunks",
        "embedding",
        VectorIndexOptions {
            metric: VectorMetric::Cosine,
            mode: IndexingMode::Async,
            m: 24,
            ef_construction: 200,
            quantized: true,
        },
    )
    .unwrap();
    db.query("INSERT INTO chunks (body, embedding) VALUES ('c', '[0.1, 0.2, 0.3, 0.4]')")
        .unwrap();
    // Async: the commit may return before the vector is searchable.
    db.wait_vector_indexing();
    let query = [0.1f32, 0.2, 0.3, 0.4];
    assert_eq!(
        db.search_vector(
            "chunks",
            "embedding",
            &query,
            1,
            &VectorSearchOptions::default()
        )
        .unwrap()
        .len(),
        1
    );

    // Dropping the index keeps the column and its vectors.
    db.drop_vector_index("chunks", "embedding").unwrap();
    assert_eq!(db.scan("chunks").unwrap().len(), 1);
    let err = db
        .search_vector(
            "chunks",
            "embedding",
            &query,
            1,
            &VectorSearchOptions::default(),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("create one with create_vector_index"), "{err}");
}

/// manual.md — "What SQL can and cannot do with a vector".
#[test]
fn vectors_in_sql_reads_writes_and_refusals() {
    let (_d, db) = new_db();
    db.query("CREATE TABLE docs (title text NOT NULL, embedding vector(4))")
        .unwrap();
    db.query("INSERT INTO docs (title, embedding) VALUES ('hello', '[0.1, 0.2, 0.3, 0.4]')")
        .unwrap();

    // SELECT returns the vector itself.
    match db
        .query("SELECT title, embedding FROM docs WHERE title = 'hello'")
        .unwrap()
    {
        QueryOutput::Rows { rows, .. } => {
            assert_eq!(rows[0][1], Value::Vector(vec![0.1, 0.2, 0.3, 0.4]));
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // UPDATE accepts the same literal.
    db.query("UPDATE docs SET embedding = '[0.5, 0.5, 0.5, 0.5]' WHERE title = 'hello'")
        .unwrap();
    assert_eq!(
        db.scan("docs").unwrap()[0].1["embedding"],
        Value::Vector(vec![0.5, 0.5, 0.5, 0.5])
    );

    // Comparing or ranking by a vector is refused, with a message that says
    // where to go instead.
    let err = db
        .query("SELECT * FROM docs WHERE embedding = '[0.1, 0.2, 0.3, 0.4]'")
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot compare"), "{err}");
    let err = db
        .query("SELECT * FROM docs ORDER BY distance(embedding, '[0.1, 0.2, 0.3, 0.4]')")
        .unwrap_err()
        .to_string();
    assert!(err.contains("use search_vector"), "{err}");
}
