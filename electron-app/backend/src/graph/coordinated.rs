//! Journal-only exact transitions. These types are not authorization: the caller
//! must reload a durable pending operation while holding the derived-store gate.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const MAX_SCOPE_NODES: usize = 16_384;
pub(crate) const MAX_SCOPE_EDGES: usize = 65_536;
pub(crate) const MAX_AFFECTED_PAPERS: usize = 4_096;
pub(crate) const MAX_ID_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_TRANSITION_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fence {
    pub generation: i64,
    pub id: String,
    pub direction: String,
    pub digest: String,
}
impl Fence {
    pub fn from_publication(p: &GraphPublication) -> Self {
        Self {
            generation: p.generation(),
            id: p.write_id().into(),
            direction: match p.kind() {
                GraphWriteKind::Initialize => "initialize",
                GraphWriteKind::Apply => "apply",
                GraphWriteKind::Restore => "restore",
                GraphWriteKind::Reset => "reset",
                GraphWriteKind::Forward => "forward",
                GraphWriteKind::Compensate => "compensate",
                GraphWriteKind::CoordinatedReset => "coordinated_reset",
            }
            .into(),
            digest: p.authority_digest().unwrap_or("").into(),
        }
    }
    pub fn next(&self, direction: &str, digest: String) -> GraphResult<Self> {
        let next = Self {
            generation: self
                .generation
                .checked_add(1)
                .ok_or(GraphError::GenerationOverflow)?,
            id: uuid::Uuid::new_v4().to_string(),
            direction: direction.into(),
            digest,
        };
        next.validate()?;
        Ok(next)
    }
    pub fn validate(&self) -> GraphResult<()> {
        let id = uuid::Uuid::parse_str(&self.id).map_err(|_| GraphError::InvalidArtifact)?;
        if id.get_version() != Some(uuid::Version::Random)
            || id.to_string() != self.id
            || self.generation < 0
        {
            return Err(GraphError::InvalidArtifact);
        }
        match self.direction.as_str() {
            "initialize" if self.generation == 0 && self.digest.is_empty() => (),
            "apply" | "restore" | "reset" if self.generation > 0 && self.digest.is_empty() => (),
            "forward" | "compensate" | "coordinated_reset"
                if self.generation > 0
                    && self.digest.len() == 64
                    && self
                        .digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => {}
            _ => return Err(GraphError::InvalidArtifact),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopeNode {
    pub kind: NodeKind,
    pub id: String,
    pub name: Option<String>,
    pub papers: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scope {
    pub paper_id: String,
    pub paper: Option<PaperGraph>,
    pub nodes: Vec<ScopeNode>,
    pub titles: BTreeMap<String, String>,
}
impl Scope {
    pub fn validate(&self) -> GraphResult<()> {
        super::super::recovery::PaperRecoveryArtifact::from_observed(
            self.paper_id.clone(),
            self.paper.clone(),
        )?;
        if self.nodes.len() > MAX_SCOPE_NODES {
            return Err(GraphError::ArtifactTooLarge);
        }
        let mut affected = BTreeSet::new();
        let mut edges = 0usize;
        let mut bytes = 0usize;
        let mut keys = BTreeSet::new();
        for n in &self.nodes {
            if !matches!(n.kind, NodeKind::Author | NodeKind::Topic)
                || !keys.insert((n.kind, n.id.clone()))
                || n.papers.windows(2).any(|w| w[0] >= w[1])
            {
                return Err(GraphError::InvalidArtifact);
            }
            let prefix = if n.kind == NodeKind::Author {
                "author:"
            } else {
                "topic:"
            };
            if !n.id.strip_prefix(prefix).is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }) {
                return Err(GraphError::InvalidArtifact);
            }
            if n.papers.len() > MAX_RECOVERY_NODES {
                return Err(GraphError::ArtifactTooLarge);
            }
            match &n.name {
                None if n.papers.is_empty() => (),
                Some(name)
                    if !n.papers.is_empty()
                        && name.len() <= 4096
                        && super::super::identity::compact(name)? == *name
                        && stable_node_id(n.kind, name)? == n.id => {}
                _ => return Err(GraphError::InvalidArtifact),
            }
            for id in &n.papers {
                super::super::recovery::canonical_id(id)?;
                bytes = bytes.saturating_add(id.len());
                affected.insert(id);
            }
            edges = edges.saturating_add(n.papers.len());
        }
        if self
            .nodes
            .windows(2)
            .any(|w| (w[0].kind, &w[0].id) >= (w[1].kind, &w[1].id))
        {
            return Err(GraphError::InvalidArtifact);
        }
        if edges > MAX_SCOPE_EDGES || affected.len() > MAX_AFFECTED_PAPERS || bytes > MAX_ID_BYTES {
            return Err(GraphError::ArtifactTooLarge);
        }
        if affected != self.titles.keys().collect() {
            return Err(GraphError::InvalidArtifact);
        }
        for title in self.titles.values() {
            if title.len() > 4096 || super::super::identity::compact(title)? != *title {
                return Err(GraphError::InvalidArtifact);
            }
        }
        if let Some(p) = &self.paper
            && self.titles.get(&self.paper_id) != Some(&p.title)
        {
            return Err(GraphError::InvalidArtifact);
        }
        let labels = labels(self.paper.as_ref());
        for ((kind, id), name) in &labels {
            let n = self
                .nodes
                .iter()
                .find(|n| n.kind == *kind && n.id == *id)
                .ok_or(GraphError::InvalidArtifact)?;
            if n.name.as_ref() != Some(name) || n.papers.binary_search(&self.paper_id).is_err() {
                return Err(GraphError::InvalidArtifact);
            }
        }
        for n in &self.nodes {
            if n.papers.binary_search(&self.paper_id).is_ok()
                != labels.contains_key(&(n.kind, n.id.clone()))
            {
                return Err(GraphError::InvalidArtifact);
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| GraphError::InvalidArtifact)?
            .len()
            > MAX_TRANSITION_BYTES
        {
            return Err(GraphError::ArtifactTooLarge);
        }
        Ok(())
    }
    pub fn digest(&self) -> GraphResult<String> {
        self.validate()?;
        Ok(
            Sha256::digest(serde_json::to_vec(self).map_err(|_| GraphError::InvalidArtifact)?)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        )
    }
    pub fn desired(&self, artifact: &PaperRecoveryArtifact) -> GraphResult<Self> {
        if artifact.paper_id() != self.paper_id {
            return Err(GraphError::InvalidArtifact);
        }
        let mut out = self.clone();
        out.paper = artifact.paper().cloned();
        out.titles.remove(&out.paper_id);
        if let Some(p) = &out.paper {
            out.titles.insert(out.paper_id.clone(), p.title.clone());
        }
        let desired = labels(out.paper.as_ref());
        for n in &mut out.nodes {
            n.papers.retain(|id| id != &out.paper_id);
            if let Some(name) = desired.get(&(n.kind, n.id.clone())) {
                n.papers.push(out.paper_id.clone());
                n.papers.sort();
                n.name = Some(name.clone());
            }
            if n.papers.is_empty() {
                n.name = None;
            }
        }
        out.validate()?;
        Ok(out)
    }
}
fn labels(p: Option<&PaperGraph>) -> BTreeMap<(NodeKind, String), String> {
    let mut out = BTreeMap::new();
    if let Some(p) = p {
        for n in &p.authors {
            out.insert((NodeKind::Author, n.author_id.clone()), n.name.clone());
        }
        for n in &p.topics {
            out.insert((NodeKind::Topic, n.topic_id.clone()), n.name.clone());
        }
    }
    out
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transition {
    pub before: Scope,
    pub desired: Scope,
    pub base: Fence,
    pub forward: Fence,
    pub reverse: Fence,
}
impl Transition {
    pub fn validate(&self) -> GraphResult<()> {
        self.base.validate()?;
        self.forward.validate()?;
        self.reverse.validate()?;
        let desired = PaperRecoveryArtifact::from_observed(
            self.desired.paper_id.clone(),
            self.desired.paper.clone(),
        )?;
        let mut union = labels(self.before.paper.as_ref());
        union.extend(labels(self.desired.paper.as_ref()));
        if self
            .before
            .nodes
            .iter()
            .map(|n| (n.kind, n.id.clone()))
            .collect::<Vec<_>>()
            != union.into_keys().collect::<Vec<_>>()
        {
            return Err(GraphError::InvalidArtifact);
        }
        if self.before.desired(&desired)? != self.desired
            || self.forward.generation
                != self
                    .base
                    .generation
                    .checked_add(1)
                    .ok_or(GraphError::GenerationOverflow)?
            || self.reverse.generation
                != self
                    .forward
                    .generation
                    .checked_add(1)
                    .ok_or(GraphError::GenerationOverflow)?
            || self.forward.direction != "forward"
            || self.reverse.direction != "compensate"
            || self.forward.digest != self.desired.digest()?
            || self.reverse.digest != self.before.digest()?
            || self.base.id == self.forward.id
            || self.forward.id == self.reverse.id
            || self.base.id == self.reverse.id
        {
            return Err(GraphError::InvalidArtifact);
        }
        if serde_json::to_vec(self)
            .map_err(|_| GraphError::InvalidArtifact)?
            .len()
            > MAX_TRANSITION_BYTES
        {
            return Err(GraphError::ArtifactTooLarge);
        }
        Ok(())
    }
}

impl CozoGraphStore {
    pub(crate) async fn publication_scope(
        &self,
        paper_id: String,
        desired: PaperRecoveryArtifact,
        c: Cancellation,
    ) -> GraphResult<(Fence, Scope)> {
        let this = self.clone();
        joined(c, move |c| {
            let guard = this.inner.lock().map_err(|_| GraphError::Storage)?;
            let graph = guard.as_ref().ok_or(GraphError::Closed)?;
            let tx = Transaction::new(&graph.db, false);
            let current = read_recovery(&tx, &paper_id, &c, || {})?;
            let mut keys = labels(current.paper());
            keys.extend(labels(desired.paper()));
            let nodes = keys.into_keys().collect::<Vec<_>>();
            let scope = read_scope(&tx, &paper_id, &nodes, &c)?;
            let fence = Fence::from_publication(&super::super::tracking::read(&tx, &c)?);
            tx.abort()?;
            Ok((fence, scope))
        })
        .await
    }
    pub(crate) async fn exact_scope(
        &self,
        scope: Scope,
        c: Cancellation,
    ) -> GraphResult<(Fence, Scope)> {
        scope.validate()?;
        let this = self.clone();
        joined(c, move |c| {
            let guard = this.inner.lock().map_err(|_| GraphError::Storage)?;
            let tx = Transaction::new(&guard.as_ref().ok_or(GraphError::Closed)?.db, false);
            let keys = scope
                .nodes
                .iter()
                .map(|n| (n.kind, n.id.clone()))
                .collect::<Vec<_>>();
            let observed = read_scope(&tx, &scope.paper_id, &keys, &c)?;
            let fence = Fence::from_publication(&super::super::tracking::read(&tx, &c)?);
            tx.abort()?;
            Ok((fence, observed))
        })
        .await
    }
    pub(crate) async fn transition(
        &self,
        t: Transition,
        reverse: bool,
        c: Cancellation,
    ) -> GraphResult<()> {
        t.validate()?;
        let this = self.clone();
        joined(c, move |c| {
            let (expected, desired, base, next) = if reverse { (&t.desired, &t.before, &t.forward, &t.reverse) } else { (&t.before, &t.desired, &t.base, &t.forward) };
            let guard = this.inner.lock().map_err(|_| GraphError::Storage)?;
            let tx = Transaction::new(&guard.as_ref().ok_or(GraphError::Closed)?.db, true);
            let keys = expected.nodes.iter().map(|n| (n.kind, n.id.clone())).collect::<Vec<_>>();
            if read_scope(&tx, &expected.paper_id, &keys, &c)? != *expected { return Err(GraphError::InvalidArtifact); }
            super::super::tracking::coordinated(&tx, base, next, &c)?;
            let artifact = PaperRecoveryArtifact::from_observed(desired.paper_id.clone(), desired.paper.clone())?;
            stage(&tx, artifact.restore_operation()?, &c)?;
            for n in &desired.nodes {
                let (table, key) = match n.kind { NodeKind::Author => ("author", "author_id"), NodeKind::Topic => ("topic", "topic_id"), _ => return Err(GraphError::InvalidArtifact) };
                if let Some(name) = &n.name {
                    tx.run(&format!("?[{key}, name, normalized_name] <- [[$id, $name, $normalized]] :put {table} {{{key} => name, normalized_name}}"), params(json!({"id": n.id, "name": name, "normalized": normalize_node_name(name)?})), &c)?;
                } else {
                    tx.run(&format!("?[{key}] <- [[$id]] :rm {table} {{{key}}}"), params(json!({"id": n.id})), &c)?;
                }
            }
            if read_scope(&tx, &desired.paper_id, &keys, &c)? != *desired { return Err(GraphError::InvalidArtifact); }
            this.commit_write(tx, &c)
        }).await
    }
    pub(crate) async fn coordinated_reset(
        &self,
        base: Fence,
        next: Fence,
        c: Cancellation,
    ) -> GraphResult<()> {
        if next.direction != "coordinated_reset" {
            return Err(GraphError::InvalidArtifact);
        }
        let this = self.clone();
        joined(c, move |c| {
            let guard = this.inner.lock().map_err(|_| GraphError::Storage)?;
            let tx = Transaction::new(&guard.as_ref().ok_or(GraphError::Closed)?.db, true);
            super::super::tracking::coordinated(&tx, &base, &next, &c)?;
            stage(&tx, GraphOperation::Reset, &c)?;
            tx.run(
                "?[author_id] := *author{author_id} :rm author {author_id}",
                BTreeMap::new(),
                &c,
            )?;
            tx.run(
                "?[topic_id] := *topic{topic_id} :rm topic {topic_id}",
                BTreeMap::new(),
                &c,
            )?;
            this.commit_write(tx, &c)
        })
        .await
    }
}

fn read_scope(
    tx: &Transaction,
    paper_id: &str,
    keys: &[(NodeKind, String)],
    c: &Cancellation,
) -> GraphResult<Scope> {
    if keys.len() > MAX_SCOPE_NODES {
        return Err(GraphError::ArtifactTooLarge);
    }
    let paper = read_recovery(tx, paper_id, c, || {})?.paper().cloned();
    let mut scope = Scope {
        paper_id: paper_id.into(),
        paper,
        nodes: vec![],
        titles: BTreeMap::new(),
    };
    let mut edges = 0usize;
    let mut bytes = 0usize;
    let mut papers = BTreeSet::new();
    let mut retained_bytes = 0usize;
    for (kind, id) in keys {
        let (table, key, reverse) = match kind {
            NodeKind::Author => ("author", "author_id", "author_paper"),
            NodeKind::Topic => ("topic", "topic_id", "topic_paper"),
            _ => return Err(GraphError::InvalidArtifact),
        };
        super::super::incidence::check_node(tx, *kind == NodeKind::Author, id, c)?;
        let rows = strings(tx.run(
            &format!(
                "?[name, normalized_name] := *{table}{{{key}: $id, name, normalized_name}} :limit 2"
            ),
            params(json!({"id":id})),
            c,
        )?)?;
        let name = match rows.as_slice() {
            [] => None,
            [r] if r.len() == 2 && normalize_node_name(&r[0])? == r[1] => Some(r[0].clone()),
            _ => return Err(GraphError::InvalidSchema),
        };
        retained_bytes = retained_bytes
            .saturating_add(id.len())
            .saturating_add(name.as_ref().map_or(0, String::len));
        if retained_bytes > MAX_TRANSITION_BYTES {
            return Err(GraphError::ArtifactTooLarge);
        }
        let rows = strings(tx.run(
            &format!("?[paper_id] := *{reverse}{{{key}: $id, paper_id}} :limit 4097"),
            params(json!({"id":id})),
            c,
        )?)?;
        if rows.len() > MAX_RECOVERY_NODES {
            return Err(GraphError::ArtifactTooLarge);
        }
        let mut incident = Vec::new();
        for r in rows {
            let [id] = r.as_slice() else {
                return Err(GraphError::InvalidSchema);
            };
            let titles = strings(tx.run(
                "?[title] := *paper{paper_id: $id, title} :limit 2",
                params(json!({"id":id})),
                c,
            )?)?;
            if !matches!(titles.as_slice(), [t] if t.len() == 1 && t[0].len() <= 4096 && super::super::identity::compact(&t[0]).is_ok_and(|s| s == t[0]))
            {
                return Err(GraphError::InvalidSchema);
            }
            if scope
                .titles
                .insert(id.clone(), titles[0][0].clone())
                .is_none()
            {
                retained_bytes = retained_bytes
                    .saturating_add(id.len())
                    .saturating_add(titles[0][0].len());
            }
            retained_bytes = retained_bytes.saturating_add(id.len());
            if retained_bytes > MAX_TRANSITION_BYTES {
                return Err(GraphError::ArtifactTooLarge);
            }
            edges += 1;
            bytes = bytes.saturating_add(id.len());
            papers.insert(id.clone());
            if edges > MAX_SCOPE_EDGES || bytes > MAX_ID_BYTES || papers.len() > MAX_AFFECTED_PAPERS
            {
                return Err(GraphError::ArtifactTooLarge);
            }
            incident.push(id.clone());
        }
        incident.sort();
        scope.nodes.push(ScopeNode {
            kind: *kind,
            id: id.clone(),
            name,
            papers: incident,
        });
    }
    scope.validate()?;
    Ok(scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(groups: Vec<Vec<String>>) -> Scope {
        let mut titles = BTreeMap::new();
        let mut nodes = groups
            .into_iter()
            .enumerate()
            .map(|(i, mut papers)| {
                papers.sort();
                for id in &papers {
                    titles.insert(id.clone(), "Title".into());
                }
                let name = format!("Author {i}");
                ScopeNode {
                    kind: NodeKind::Author,
                    id: stable_node_id(NodeKind::Author, &name).unwrap(),
                    name: (!papers.is_empty()).then_some(name),
                    papers,
                }
            })
            .collect::<Vec<_>>();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        Scope {
            paper_id: "target".into(),
            paper: None,
            nodes,
            titles,
        }
    }
    fn ids(n: usize, len: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("{i:04}{}", "x".repeat(len - 4)))
            .collect()
    }

    #[test]
    fn coordinated_scope_exact_count_and_byte_limits() {
        for n in [MAX_SCOPE_NODES - 1, MAX_SCOPE_NODES, MAX_SCOPE_NODES + 1] {
            assert_eq!(
                scope(vec![vec![]; n]).validate().is_ok(),
                n <= MAX_SCOPE_NODES
            );
        }
        for n in [
            MAX_AFFECTED_PAPERS - 1,
            MAX_AFFECTED_PAPERS,
            MAX_AFFECTED_PAPERS + 1,
        ] {
            let all = ids(n, 5);
            assert_eq!(
                scope(vec![all[..2048].to_vec(), all[2048..].to_vec()])
                    .validate()
                    .is_ok(),
                n <= MAX_AFFECTED_PAPERS
            );
        }
        for n in [MAX_SCOPE_EDGES - 1, MAX_SCOPE_EDGES, MAX_SCOPE_EDGES + 1] {
            let mut groups = vec![ids(4096, 5); n / 4096];
            if n % 4096 != 0 {
                groups.push(ids(n % 4096, 5));
            }
            assert_eq!(scope(groups).validate().is_ok(), n <= MAX_SCOPE_EDGES);
        }
        for delta in [-1, 0, 1] {
            let mut groups = vec![ids(128, 1024); 32];
            if delta < 0 {
                groups[31][127].pop();
            } else if delta > 0 {
                groups[31][127].push('x');
            }
            assert_eq!(scope(groups).validate().is_ok(), delta <= 0);
        }
        let mut value = scope(vec![ids(3000, 5)]);
        let overhead = serde_json::to_vec(&value).unwrap().len();
        let mut extra = MAX_TRANSITION_BYTES - overhead;
        for title in value.titles.values_mut() {
            let take = extra.min(4096 - title.len());
            title.push_str(&"x".repeat(take));
            extra -= take;
        }
        assert_eq!(extra, 0);
        assert_eq!(
            serde_json::to_vec(&value).unwrap().len(),
            MAX_TRANSITION_BYTES
        );
        value.validate().unwrap();
        let id = value
            .titles
            .iter()
            .find(|(_, s)| s.len() < 4096)
            .unwrap()
            .0
            .clone();
        value.titles.get_mut(&id).unwrap().pop();
        value.validate().unwrap();
        value.titles.get_mut(&id).unwrap().push_str("xx");
        assert_eq!(value.validate(), Err(GraphError::ArtifactTooLarge));
    }
}
