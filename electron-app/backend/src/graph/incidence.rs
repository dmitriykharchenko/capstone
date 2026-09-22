use super::{GraphError, GraphResult, transaction::Transaction};
use crate::runtime::Cancellation;
use cozo::DataValue;
use std::collections::BTreeMap;

/// Check BOTH raw directions, not a join that could hide a missing cache edge.
/// These checks bound returned rows, not native scan work or lock duration.
pub(super) fn check_paper(tx: &Transaction, id: &str, c: &Cancellation) -> GraphResult<()> {
    for author in [true, false] {
        check(tx, author, Some(("paper_id", id)), c)?;
    }
    Ok(())
}

pub(super) fn check_node(
    tx: &Transaction,
    author: bool,
    id: &str,
    c: &Cancellation,
) -> GraphResult<()> {
    check(tx, author, Some(("node_id", id)), c)
}

pub(super) fn check_all(tx: &Transaction, c: &Cancellation) -> GraphResult<()> {
    for author in [true, false] {
        check(tx, author, None, c)?;
    }
    Ok(())
}

fn check(
    tx: &Transaction,
    author: bool,
    scope: Option<(&str, &str)>,
    c: &Cancellation,
) -> GraphResult<()> {
    let (forward, reverse, node) = if author {
        ("authored_by", "author_paper", "author_id")
    } else {
        ("has_topic", "topic_paper", "topic_id")
    };
    let (filter, params) = match scope {
        Some((column, id)) => (
            format!(", {column} = $id"),
            BTreeMap::from([("id".into(), DataValue::from(id))]),
        ),
        None => (String::new(), BTreeMap::new()),
    };
    let rows = tx.run(&format!(
        "?[paper_id, node_id] := *{forward}{{paper_id, {node}: node_id}}, not *{reverse}{{paper_id, {node}: node_id}}{filter}\n\
         ?[paper_id, node_id] := *{reverse}{{paper_id, {node}: node_id}}, not *{forward}{{paper_id, {node}: node_id}}{filter}\n\
         :limit 1"
    ), params, c)?;
    if rows.rows.is_empty() {
        Ok(())
    } else {
        Err(GraphError::InvalidSchema)
    }
}
