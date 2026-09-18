use super::{
    transaction::{Transaction, check, joined},
    types::validate_query,
    *,
};
use crate::{
    config::{validate_database_path, validate_root},
    domain::GraphQuery,
    protocol::ErrorCode,
    registry::BackendFuture,
    runtime::Cancellation,
};
use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

type Schema = (
    &'static str,
    &'static str,
    &'static [(&'static str, bool, &'static str)],
);
const SCHEMAS: &[Schema] = &[
    (
        "paper",
        ":create paper {paper_id: String => title: String}",
        &[("paper_id", true, "String"), ("title", false, "String")],
    ),
    (
        "author",
        ":create author {author_id: String => name: String, normalized_name: String}",
        &[
            ("author_id", true, "String"),
            ("name", false, "String"),
            ("normalized_name", false, "String"),
        ],
    ),
    (
        "topic",
        ":create topic {topic_id: String => name: String, normalized_name: String}",
        &[
            ("topic_id", true, "String"),
            ("name", false, "String"),
            ("normalized_name", false, "String"),
        ],
    ),
    (
        "authored_by",
        ":create authored_by {paper_id: String, author_id: String}",
        &[("paper_id", true, "String"), ("author_id", true, "String")],
    ),
    (
        "has_topic",
        ":create has_topic {paper_id: String, topic_id: String}",
        &[("paper_id", true, "String"), ("topic_id", true, "String")],
    ),
    (
        "graph_meta",
        ":create graph_meta {key: String => version: Int}",
        &[("key", true, "String"), ("version", false, "Int")],
    ),
];

struct OpenGraph {
    db: DbInstance,
    _lock: File,
}
/// One SQLite-backed Cozo handle, serialized within this process and exclusively
/// locked across processes. Clones share lifecycle; close closes all clones.
#[derive(Clone)]
pub struct CozoGraphStore {
    inner: Arc<Mutex<Option<OpenGraph>>>,
}

impl CozoGraphStore {
    /// Trusted startup paths only. New storage is atomically published from a
    /// same-directory staging file; existing unmarked/legacy schemas are refused.
    /// Creation seals `cancellation`; use a distinct request for later mutations.
    pub async fn open(
        root: PathBuf,
        path: PathBuf,
        cancellation: Cancellation,
    ) -> GraphResult<Self> {
        joined(cancellation, move |c| {
            let root = validate_root(&root).map_err(|_| GraphError::InvalidPath)?;
            let path = validate_database_path(&root, &path).map_err(|_| GraphError::InvalidPath)?;
            check(&c)?;
            fs::create_dir_all(&root).map_err(|_| GraphError::Storage)?;
            let lock_path = path.with_file_name(format!(
                "{}.lock",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .ok_or(GraphError::InvalidPath)?
            ));
            validate_database_path(&root, &lock_path).map_err(|_| GraphError::InvalidPath)?;
            let lock = lock_file(&lock_path)?;
            validate_database_path(&root, &path).map_err(|_| GraphError::InvalidPath)?;
            if !path.exists() {
                initialize(&root, &path, &c)?;
            }
            check(&c)?;
            let db = DbInstance::new("sqlite", &path, "").map_err(|_| GraphError::Storage)?;
            validate_schema(&db)?;
            check(&c)?;
            Ok(Self {
                inner: Arc::new(Mutex::new(Some(OpenGraph { db, _lock: lock }))),
            })
        })
        .await
    }

    pub async fn close(&self) -> GraphResult<()> {
        let this = self.clone();
        joined(Cancellation::default(), move |_| {
            this.inner.lock().map_err(|_| GraphError::Storage)?.take();
            Ok(())
        })
        .await
    }

    /// Apply all prepared replacements/removals/reset in ONE transaction. Nothing
    /// becomes visible before the sticky final seal and actual Cozo COMMIT. This
    /// API cannot atomically publish with history/retrieval in different stores.
    pub async fn apply(
        &self,
        operations: Vec<GraphOperation>,
        cancellation: Cancellation,
    ) -> GraphResult<()> {
        let operations = operations
            .into_iter()
            .map(GraphOperation::validate)
            .collect::<GraphResult<Vec<_>>>()?;
        if operations.is_empty() {
            return Err(GraphError::InvalidInput);
        }
        let this = self.clone();
        joined(cancellation, move |c| {
            this.apply_sync(operations, &c, || Ok(()))
        })
        .await
    }
    fn apply_sync(
        &self,
        operations: Vec<GraphOperation>,
        c: &Cancellation,
        before_commit: impl FnOnce() -> GraphResult<()>,
    ) -> GraphResult<()> {
        check(c)?;
        let guard = self.inner.lock().map_err(|_| GraphError::Storage)?;
        let graph = guard.as_ref().ok_or(GraphError::Closed)?;
        check(c)?;
        let tx = Transaction::new(&graph.db, true);
        for operation in operations {
            stage(&tx, operation, c)?;
        }
        tx.run("?[author_id] := *author{author_id}, not *authored_by{author_id} :rm author {author_id}", BTreeMap::new(), c)?;
        tx.run(
            "?[topic_id] := *topic{topic_id}, not *has_topic{topic_id} :rm topic {topic_id}",
            BTreeMap::new(),
            c,
        )?;
        before_commit()?;
        tx.commit(c)
    }
    pub async fn reset(&self, cancellation: Cancellation) -> GraphResult<()> {
        self.apply(vec![GraphOperation::Reset], cancellation).await
    }

    pub async fn query_typed(
        &self,
        query: GraphQuery,
        cancellation: Cancellation,
    ) -> GraphResult<GraphResponse> {
        let query = validate_query(query)?;
        let this = self.clone();
        joined(cancellation, move |c| {
            let guard = this.inner.lock().map_err(|_| GraphError::Storage)?;
            let graph = guard.as_ref().ok_or(GraphError::Closed)?;
            check(&c)?;
            let tx = Transaction::new(&graph.db, false);
            let data = query_data(&tx, query, &c)?;
            tx.abort()?;
            check(&c)?;
            Ok(GraphResponse {
                enabled: true,
                available: true,
                data,
            })
        })
        .await
    }
    pub async fn status(&self, cancellation: Cancellation) -> GraphResult<GraphStatus> {
        let this = self.clone();
        joined(cancellation, move |c| {
            let guard = this.inner.lock().map_err(|_| GraphError::Storage)?;
            let graph = guard.as_ref().ok_or(GraphError::Closed)?;
            let tx = Transaction::new(&graph.db, false);
            let count = |script| -> GraphResult<usize> {
                let rows = tx.run(script, BTreeMap::new(), &c)?;
                rows.rows
                    .first()
                    .and_then(|r| r.first())
                    .and_then(DataValue::get_int)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or(GraphError::Storage)
            };
            let status = GraphStatus {
                papers: count("?[count(id)] := *paper{paper_id: id}")?,
                authors: count("?[count(id)] := *author{author_id: id}")?,
                topics: count("?[count(id)] := *topic{topic_id: id}")?,
                edges: count(
                    "?[count(paper_id), count(author_id)] := *authored_by{paper_id, author_id}",
                )? + count(
                    "?[count(paper_id), count(topic_id)] := *has_topic{paper_id, topic_id}",
                )?,
            };
            tx.abort()?;
            check(&c)?;
            Ok(status)
        })
        .await
    }
}

impl GraphStore for CozoGraphStore {
    fn query(&self, query: GraphQuery, cancellation: Cancellation) -> BackendFuture<Value> {
        let this = self.clone();
        Box::pin(async move {
            serde_json::to_value(
                this.query_typed(query, cancellation)
                    .await
                    .map_err(ErrorCode::from)?,
            )
            .map_err(|_| ErrorCode::Internal)
        })
    }
    fn replace_paper(
        &self,
        paper_id: String,
        title: String,
        authors: Vec<String>,
        topics: Vec<String>,
        cancellation: Cancellation,
    ) -> BackendFuture<()> {
        let this = self.clone();
        Box::pin(async move {
            let paper = PreparedPaper::new(&paper_id, &title, &authors, &topics)
                .map_err(ErrorCode::from)?;
            this.apply(vec![GraphOperation::Replace(paper)], cancellation)
                .await
                .map_err(Into::into)
        })
    }
}

#[cfg(unix)]
fn lock_file(path: &Path) -> GraphResult<File> {
    use std::os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, fs::OpenOptionsExt},
    };
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| GraphError::InvalidPath)?;
    let meta = lock.metadata().map_err(|_| GraphError::InvalidPath)?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(GraphError::InvalidPath);
    }
    // SAFETY: live owned fd; nonblocking advisory exclusive lock. Dropping the
    // descriptor releases it. No raw paths or provider details reach errors.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(GraphError::Locked);
    }
    Ok(lock)
}
#[cfg(not(unix))]
fn lock_file(_: &Path) -> GraphResult<File> {
    Err(GraphError::InvalidPath)
}

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn initialize(root: &Path, path: &Path, c: &Cancellation) -> GraphResult<()> {
    let stage_path = root.join(format!(".graph-{}.staging", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(&stage_path).map_err(|_| GraphError::Storage)?;
    // Arm cleanup only after create_new proved ownership of this random file.
    let staging = Staging(stage_path);
    let db = DbInstance::new("sqlite", &staging.0, "").map_err(|_| GraphError::Storage)?;
    let tx = Transaction::new(&db, true);
    for (_, script, _) in SCHEMAS {
        tx.run(script, BTreeMap::new(), c)?;
    }
    tx.run(
        "?[key, version] <- [['electron-paper-graph', 1]] :put graph_meta {key => version}",
        BTreeMap::new(),
        c,
    )?;
    tx.commit_staging()?;
    drop(db);
    File::open(&staging.0)
        .and_then(|f| f.sync_all())
        .map_err(|_| GraphError::Storage)?;
    validate_database_path(root, path).map_err(|_| GraphError::InvalidPath)?;
    super::publication::publish_no_replace(&staging.0, path, c)
}
fn validate_schema(db: &DbInstance) -> GraphResult<()> {
    for (name, _, expected) in SCHEMAS {
        let columns = db
            .run_script(
                &format!("::columns {name}"),
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|_| GraphError::InvalidSchema)?
            .into_json();
        let rows = columns["rows"]
            .as_array()
            .ok_or(GraphError::InvalidSchema)?;
        if rows.len() != expected.len()
            || rows.iter().zip(*expected).any(|(row, (column, key, ty))| {
                row[0] != *column || row[1] != *key || row[3] != *ty
            })
        {
            return Err(GraphError::InvalidSchema);
        }
    }
    let marker = db
        .run_script(
            "?[key, version] := *graph_meta{key, version}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .map_err(|_| GraphError::InvalidSchema)?
        .into_json();
    if marker["rows"] != json!([["electron-paper-graph", 1]]) {
        return Err(GraphError::InvalidSchema);
    }
    Ok(())
}

fn params(value: Value) -> BTreeMap<String, DataValue> {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), DataValue::from(v.clone())))
        .collect()
}
fn stage(tx: &Transaction, operation: GraphOperation, c: &Cancellation) -> GraphResult<()> {
    match operation {
        GraphOperation::Reset => {
            for script in [
                "?[paper_id, author_id] := *authored_by{paper_id, author_id} :rm authored_by {paper_id, author_id}",
                "?[paper_id, topic_id] := *has_topic{paper_id, topic_id} :rm has_topic {paper_id, topic_id}",
                "?[paper_id] := *paper{paper_id} :rm paper {paper_id}",
            ] {
                tx.run(script, BTreeMap::new(), c)?;
            }
        }
        GraphOperation::Remove(id) => {
            remove_edges(tx, &id, c)?;
            tx.run(
                "?[paper_id] <- [[$id]] :rm paper {paper_id}",
                params(json!({"id": id})),
                c,
            )?;
        }
        GraphOperation::Replace(prepared) => {
            let p = prepared.paper();
            remove_edges(tx, &p.paper_id, c)?;
            tx.run(
                "?[paper_id, title] <- [[$id, $title]] :put paper {paper_id => title}",
                params(json!({"id": p.paper_id, "title": p.title})),
                c,
            )?;
            let authors: Vec<_> = p
                .authors
                .iter()
                .map(|a| Ok(json!([a.author_id, a.name, normalize_node_name(&a.name)?])))
                .collect::<GraphResult<_>>()?;
            let topics: Vec<_> = p
                .topics
                .iter()
                .map(|t| Ok(json!([t.topic_id, t.name, normalize_node_name(&t.name)?])))
                .collect::<GraphResult<_>>()?;
            tx.run("?[author_id, name, normalized_name] <- $rows :put author {author_id => name, normalized_name}", params(json!({"rows": authors})), c)?;
            tx.run("?[topic_id, name, normalized_name] <- $rows :put topic {topic_id => name, normalized_name}", params(json!({"rows": topics})), c)?;
            tx.run("?[paper_id, author_id] <- $rows :put authored_by {paper_id, author_id}", params(json!({"rows": p.authors.iter().map(|a| json!([p.paper_id, a.author_id])).collect::<Vec<_>>()})), c)?;
            tx.run("?[paper_id, topic_id] <- $rows :put has_topic {paper_id, topic_id}", params(json!({"rows": p.topics.iter().map(|t| json!([p.paper_id, t.topic_id])).collect::<Vec<_>>()})), c)?;
        }
    }
    Ok(())
}
fn remove_edges(tx: &Transaction, id: &str, c: &Cancellation) -> GraphResult<()> {
    for script in [
        "?[paper_id, author_id] := *authored_by{paper_id, author_id}, paper_id = $id :rm authored_by {paper_id, author_id}",
        "?[paper_id, topic_id] := *has_topic{paper_id, topic_id}, paper_id = $id :rm has_topic {paper_id, topic_id}",
    ] {
        tx.run(script, params(json!({"id": id})), c)?;
    }
    Ok(())
}

fn strings(rows: NamedRows) -> GraphResult<Vec<Vec<String>>> {
    rows.rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| v.get_str().map(str::to_owned).ok_or(GraphError::Storage))
                .collect()
        })
        .collect()
}
fn pairs(
    tx: &Transaction,
    script: &str,
    id: &str,
    c: &Cancellation,
) -> GraphResult<Vec<(String, String)>> {
    let mut pairs = strings(tx.run(script, params(json!({"id": id})), c)?)?
        .into_iter()
        .map(|r| match r.as_slice() {
            [id, name] => Ok((id.clone(), name.clone())),
            _ => Err(GraphError::Storage),
        })
        .collect::<GraphResult<Vec<_>>>()?;
    pairs.sort_by_cached_key(|(id, name)| {
        (normalize_node_name(name).unwrap_or_default(), id.clone())
    });
    Ok(pairs)
}
fn authors(pairs: Vec<(String, String)>) -> Vec<AuthorNode> {
    pairs
        .into_iter()
        .map(|(author_id, name)| AuthorNode { author_id, name })
        .collect()
}
fn topics(pairs: Vec<(String, String)>) -> Vec<TopicNode> {
    pairs
        .into_iter()
        .map(|(topic_id, name)| TopicNode { topic_id, name })
        .collect()
}
fn paper(tx: &Transaction, id: &str, c: &Cancellation) -> GraphResult<Option<PaperGraph>> {
    let rows = strings(tx.run(
        "?[title] := *paper{paper_id: $id, title}",
        params(json!({"id": id})),
        c,
    )?)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let title = row.first().ok_or(GraphError::Storage)?.clone();
    Ok(Some(PaperGraph {
        paper_id: id.into(),
        title,
        authors: authors(pairs(
            tx,
            "?[author_id, name] := *authored_by{paper_id: $id, author_id}, *author{author_id, name}",
            id,
            c,
        )?),
        topics: topics(pairs(
            tx,
            "?[topic_id, name] := *has_topic{paper_id: $id, topic_id}, *topic{topic_id, name}",
            id,
            c,
        )?),
    }))
}
fn papers(
    tx: &Transaction,
    script: &str,
    id: &str,
    c: &Cancellation,
) -> GraphResult<Vec<PaperGraph>> {
    let mut result = Vec::new();
    for row in strings(tx.run(script, params(json!({"id": id})), c)?)? {
        if let Some(p) = paper(tx, row.first().ok_or(GraphError::Storage)?, c)? {
            result.push(p);
        }
    }
    result.sort_by(|a, b| a.paper_id.cmp(&b.paper_id));
    Ok(result)
}
fn query_data(
    tx: &Transaction,
    query: GraphQuery,
    c: &Cancellation,
) -> GraphResult<GraphQueryData> {
    Ok(match query {
        GraphQuery::PaperDetails { paper_id } => GraphQueryData::PaperDetails {
            paper: paper(tx, &paper_id, c)?,
        },
        GraphQuery::PapersByAuthor { author_id } => GraphQueryData::PapersByAuthor {
            papers: papers(
                tx,
                "?[paper_id] := *authored_by{paper_id, author_id: $id}",
                &author_id,
                c,
            )?,
        },
        GraphQuery::PapersByTopic { topic_id } => GraphQueryData::PapersByTopic {
            papers: papers(
                tx,
                "?[paper_id] := *has_topic{paper_id, topic_id: $id}",
                &topic_id,
                c,
            )?,
        },
        GraphQuery::TopicsByAuthor { author_id } => GraphQueryData::TopicsByAuthor {
            topics: topics(pairs(
                tx,
                "?[topic_id, name] := *authored_by{paper_id, author_id: $id}, *has_topic{paper_id, topic_id}, *topic{topic_id, name}",
                &author_id,
                c,
            )?),
        },
        GraphQuery::Coauthors { author_id } => GraphQueryData::Coauthors {
            coauthors: authors(pairs(
                tx,
                "?[other_id, name] := *authored_by{paper_id, author_id: $id}, *authored_by{paper_id, author_id: other_id}, other_id != $id, *author{author_id: other_id, name}",
                &author_id,
                c,
            )?),
        },
        GraphQuery::Neighborhood { node_id, max_depth } => GraphQueryData::Neighborhood {
            neighborhood: neighborhood(tx, &node_id, max_depth, c)?,
        },
    })
}
fn neighborhood(
    tx: &Transaction,
    id: &str,
    depth: u32,
    c: &Cancellation,
) -> GraphResult<GraphNeighborhood> {
    let mut nodes = BTreeMap::new();
    for (kind, script) in [
        (
            NodeKind::Paper,
            "?[id, label] := *paper{paper_id: id, title: label}",
        ),
        (
            NodeKind::Author,
            "?[id, label] := *author{author_id: id, name: label}",
        ),
        (
            NodeKind::Topic,
            "?[id, label] := *topic{topic_id: id, name: label}",
        ),
    ] {
        for (node_id, label) in pairs(tx, script, id, c)? {
            nodes.insert(
                node_id.clone(),
                GraphNode {
                    node_id,
                    kind,
                    label,
                },
            );
        }
    }
    if !nodes.contains_key(id) {
        return Ok(GraphNeighborhood::default());
    }
    let mut edges = Vec::new();
    let mut adjacency: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (kind, script) in [
        (
            EdgeKind::AuthoredBy,
            "?[paper_id, author_id] := *authored_by{paper_id, author_id}",
        ),
        (
            EdgeKind::HasTopic,
            "?[paper_id, topic_id] := *has_topic{paper_id, topic_id}",
        ),
    ] {
        for (source_id, target_id) in pairs(tx, script, id, c)? {
            if !nodes.contains_key(&source_id) || !nodes.contains_key(&target_id) {
                return Err(GraphError::Storage);
            }
            adjacency
                .entry(source_id.clone())
                .or_default()
                .insert(target_id.clone());
            adjacency
                .entry(target_id.clone())
                .or_default()
                .insert(source_id.clone());
            edges.push(GraphEdge {
                source_id,
                target_id,
                kind,
            });
        }
    }
    let mut visited = BTreeSet::from([id.to_string()]);
    let mut frontier = visited.clone();
    for _ in 0..depth {
        check(c)?;
        frontier = frontier
            .iter()
            .flat_map(|id| adjacency.get(id).into_iter().flatten())
            .filter(|n| !visited.contains(*n))
            .cloned()
            .collect();
        visited.extend(frontier.iter().cloned());
    }
    let mut selected: Vec<_> = nodes
        .into_values()
        .filter(|n| visited.contains(&n.node_id))
        .collect();
    selected.sort_by_cached_key(|n| {
        (
            n.kind,
            normalize_node_name(&n.label).unwrap_or_default(),
            n.node_id.clone(),
        )
    });
    edges.retain(|e| visited.contains(&e.source_id) && visited.contains(&e.target_id));
    edges.sort_by(|a, b| {
        (a.kind, &a.source_id, &a.target_id).cmp(&(b.kind, &b.source_id, &b.target_id))
    });
    Ok(GraphNeighborhood {
        nodes: selected,
        edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn replacement(title: &str) -> GraphOperation {
        GraphOperation::Replace(
            PreparedPaper::new("p1", title, &[title.into()], &[title.into()]).unwrap(),
        )
    }
    async fn setup() -> (tempfile::TempDir, CozoGraphStore) {
        let root = tempfile::tempdir().unwrap();
        let store = CozoGraphStore::open(
            root.path().into(),
            root.path().join("graph.sqlite3"),
            Cancellation::default(),
        )
        .await
        .unwrap();
        store
            .apply(vec![replacement("Original")], Cancellation::default())
            .await
            .unwrap();
        (root, store)
    }
    async fn original(store: &CozoGraphStore) {
        let v = store
            .query(
                GraphQuery::PaperDetails {
                    paper_id: "p1".into(),
                },
                Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(v["paper"]["title"], "Original");
        assert_eq!(
            store.status(Cancellation::default()).await.unwrap(),
            GraphStatus {
                papers: 1,
                authors: 1,
                topics: 1,
                edges: 2
            }
        );
    }

    #[tokio::test]
    async fn graph_final_stage_failure_and_cancellation_roll_back_and_reopen() {
        let (root, store) = setup().await;
        for cancel in [false, true] {
            let c = Cancellation::default();
            let result = store.apply_sync(
                vec![GraphOperation::Reset, replacement("Changed")],
                &c,
                || {
                    if cancel {
                        assert!(c.cancel());
                        Ok(())
                    } else {
                        Err(GraphError::Storage)
                    }
                },
            );
            assert_eq!(
                result,
                Err(if cancel {
                    GraphError::Cancelled
                } else {
                    GraphError::Storage
                })
            );
            original(&store).await;
        }
        store.close().await.unwrap();
        let reopened = CozoGraphStore::open(
            root.path().into(),
            root.path().join("graph.sqlite3"),
            Cancellation::default(),
        )
        .await
        .unwrap();
        original(&reopened).await;
    }

    #[tokio::test]
    async fn graph_statement_failure_aborts_before_store_lock_is_released() {
        let (root, store) = setup().await;
        {
            let guard = store.inner.lock().unwrap();
            let tx = Transaction::new(&guard.as_ref().unwrap().db, true);
            let c = Cancellation::default();
            stage(&tx, replacement("Changed"), &c).unwrap();
            assert!(tx.run("not valid CozoScript", BTreeMap::new(), &c).is_err());
            drop(tx);
            // Rollback and its SQLite lock release must have finished, not just
            // the Cozo Abort ack. An external EXCLUSIVE lock is possible now.
            let sql = sqlite::open(root.path().join("graph.sqlite3")).unwrap();
            sql.execute("BEGIN EXCLUSIVE; ROLLBACK").unwrap();
        }
        original(&store).await;
    }

    #[tokio::test]
    async fn graph_real_sqlite_commit_failure_is_not_reported_as_success() {
        let (root, store) = setup().await;
        // A second SQLite reader permits staging but prevents the writer's
        // COMMIT from upgrading to EXCLUSIVE. This is a real nested commit Err,
        // the error discarded by upstream MultiTransaction::commit().
        let reader = sqlite::open(root.path().join("graph.sqlite3")).unwrap();
        reader.execute("BEGIN; SELECT count(*) FROM cozo").unwrap();
        let c = Cancellation::default();
        assert_eq!(
            store.apply(vec![replacement("Changed")], c.clone()).await,
            Err(GraphError::Storage)
        );
        assert!(!c.cancel());
        reader.execute("ROLLBACK").unwrap();
        original(&store).await;
        store.close().await.unwrap();
        let reopened = CozoGraphStore::open(
            root.path().into(),
            root.path().join("graph.sqlite3"),
            Cancellation::default(),
        )
        .await
        .unwrap();
        original(&reopened).await;
    }

    #[test]
    fn graph_cancelled_initialization_cleans_only_its_owned_staging_file() {
        let root = tempfile::tempdir().unwrap();
        let unrelated = root.path().join(".graph-unrelated.staging");
        fs::write(&unrelated, "keep").unwrap();
        let target = root.path().join("graph.sqlite3");
        let c = Cancellation::default();
        c.cancel();
        assert_eq!(
            initialize(root.path(), &target, &c),
            Err(GraphError::Cancelled)
        );
        assert!(!target.exists());
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "keep");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
